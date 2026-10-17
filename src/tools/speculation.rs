//! Optional preparation of admitted native AST observations. Never runs processes, callbacks,
//! edits, directory scans or hinted third-party tools. A cache is not an execution grant.
use super::{
    ToolOutput, Workspace,
    ext::{Registry, ToolContext},
};
use crate::runtime::cancellation::{Scope, TurnCancellation};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

const MAX_SOURCE: u64 = 2 * 1024 * 1024;
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub started: u64,
    pub completed: u64,
    pub reused: u64,
    pub discarded: u64,
    pub preparation_ms: u64,
}
#[derive(Default)]
struct Counters {
    started: AtomicU64,
    completed: AtomicU64,
    reused: AtomicU64,
    discarded: AtomicU64,
    preparation_ms: AtomicU64,
}
impl Counters {
    fn snapshot(&self) -> Stats {
        Stats {
            started: self.started.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Acquire),
            reused: self.reused.load(Ordering::Relaxed),
            discarded: self.discarded.load(Ordering::Relaxed),
            preparation_ms: self.preparation_ms.load(Ordering::Relaxed),
        }
    }
}
struct Pending {
    _owner: TurnCancellation,
    source: PathBuf,
    scope: Scope,
    started: Instant,
    result: Receiver<Option<(String, ToolOutput)>>,
}
#[derive(Default)]
pub(crate) struct Pool {
    pending: Mutex<BTreeMap<String, Pending>>,
    counters: Arc<Counters>,
}
impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Speculation").field(&self.stats()).finish()
    }
}
/// Drops unconsumed preparation on every batch exit, including interruption and errors.
pub(crate) struct Batch(std::sync::Arc<Pool>);
impl Drop for Batch {
    fn drop(&mut self) {
        self.0.clear();
    }
}
impl Pool {
    pub fn batch(self: &std::sync::Arc<Self>) -> Batch {
        self.clear();
        Batch(self.clone())
    }
    pub fn stats(&self) -> Stats {
        self.counters.snapshot()
    }
    pub fn pending(&self, owner_key: &str, args: &Value) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(&key(owner_key, args))
    }
    pub fn clear(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for entry in pending.values() {
            entry.scope.cancel();
            self.counters.discarded.fetch_add(1, Ordering::Relaxed);
        }
        pending.clear();
    }
    /// The execution owner supplies a key only after schema, role, permission and rule checks.
    pub fn start(
        &self,
        workspace: &Workspace,
        config: crate::config::types::Config,
        registry: &Registry,
        args: Value,
        owner_key: String,
        owner: TurnCancellation,
    ) -> bool {
        if registry.origin("ast_search") != Some(super::catalog::Origin::Native) {
            return false;
        }
        let Some(tool) = registry.get("ast_search").cloned() else {
            return false;
        };
        if tool.needs_approval(&args)
            || tool.may_modify_workspace(&args)
            || tool.supports_tool_dispatch()
        {
            return false;
        }
        let scope = owner.read();
        let pending_owner = owner.clone();
        let Some(source) = single_source(workspace, &args) else {
            return false;
        };
        let key = key(&owner_key, &args);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if pending.len() >= 2 || pending.contains_key(&key) || scope.is_cancelled() {
            return false;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let started = Instant::now();
        let counters = self.counters.clone();
        let worker_scope = scope.clone();
        let worker_source = source.clone();
        let workspace = workspace.speculative_workspace();
        let worker = std::thread::Builder::new()
            .name("dial-read-preparation".into())
            .spawn(move || {
                let _owner = owner;
                let observed = (|| {
                    if worker_scope.is_cancelled() {
                        return None;
                    }
                    let output = super::execution::run(&worker_scope.flag(), || {
                        tool.call(
                            &ToolContext {
                                workspace: &workspace,
                                config: &config,
                                mutation_authorizer: None,
                                confirm: None,
                            },
                            &args,
                        )
                    });
                    if output.is_error || worker_scope.is_cancelled() {
                        return None;
                    }
                    let observations: Vec<crate::runtime::fingerprint::Observation> =
                        serde_json::from_value(
                            output.data.as_ref()?.get("source_observations")?.clone(),
                        )
                        .ok()?;
                    if observations.len() != 1
                        || observations[0].path != worker_source
                        || observations[0].bytes as u64 > MAX_SOURCE
                    {
                        return None;
                    }
                    let captured = &observations[0].sha256;
                    (fingerprint(&worker_source, &worker_scope).as_deref()
                        == Some(captured.as_str()))
                    .then(|| {
                        (
                            captured.clone(),
                            output.with_effects(super::outcome::Effects::None),
                        )
                    })
                })();
                counters.preparation_ms.fetch_add(
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                let _ = sender.send(observed);
                counters.completed.fetch_add(1, Ordering::Release);
            });
        if worker.is_err() {
            return false;
        }
        self.counters.started.fetch_add(1, Ordering::Relaxed);
        pending.insert(
            key,
            Pending {
                _owner: pending_owner,
                source,
                scope,
                started,
                result: receiver,
            },
        );
        true
    }
    /// Ordinary owner admission runs again before this call. An unfinished/stale cache never waits
    /// or substitutes a different query: cancel it and run the ordinary tool instead.
    pub fn take(&self, workspace: &Workspace, args: &Value, owner_key: &str) -> Option<ToolOutput> {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&key(owner_key, args))?;
        let result = if !entry.scope.is_cancelled()
            && entry.started.elapsed() <= Duration::from_secs(5)
            && single_source(workspace, args).as_ref() == Some(&entry.source)
        {
            entry
                .result
                .try_recv()
                .ok()
                .flatten()
                .and_then(|(digest, output)| {
                    (fingerprint(&entry.source, &entry.scope).as_deref() == Some(digest.as_str()))
                        .then_some(output)
                })
        } else {
            None
        };
        entry.scope.cancel();
        if result.is_some() {
            self.counters.reused.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.discarded.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.clear();
    }
}
fn key(owner: &str, args: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(owner, args)).expect("JSON arguments serialize"))
    )
}
fn single_source(workspace: &Workspace, args: &Value) -> Option<PathBuf> {
    let input = args.get("path")?.as_str()?;
    if input.contains("://") {
        return None;
    }
    let path = workspace.resolve(input).ok()?;
    if !path.starts_with(workspace.root())
        || path
            .strip_prefix(workspace.root())
            .ok()?
            .components()
            .any(|part| {
                part.as_os_str() == ".git" || part.as_os_str() == crate::workspace_copy::SCRATCH_DIR
            })
    {
        return None;
    }
    let original = workspace.root().join(input);
    let mut cursor = Some(original.as_path());
    while let Some(path) = cursor {
        if std::fs::symlink_metadata(path)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
        if path == workspace.root() {
            break;
        }
        cursor = path.parent();
    }
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    (metadata.is_file() && metadata.len() <= MAX_SOURCE).then_some(path)
}
fn fingerprint(path: &Path, scope: &Scope) -> Option<String> {
    if scope.is_cancelled() {
        return None;
    }
    let mut file = crate::platform::fs::open_nofollow_nonblocking(
        std::fs::OpenOptions::new().read(true),
        path,
    )
    .ok()?;
    if !file.metadata().ok()?.is_file() || file.metadata().ok()?.len() > MAX_SOURCE {
        return None;
    }
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0;
    loop {
        if scope.is_cancelled() {
            return None;
        }
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_SOURCE {
            return None;
        }
        digest.update(&buffer[..count]);
    }
    Some(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests;
