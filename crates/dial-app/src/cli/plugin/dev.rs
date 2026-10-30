//! Explicitly trusted local development, isolated from the user's extension store.
use crate::tools::integrations::plugin::{
    Host, authoring,
    installer::{PluginStore, runtime_spec},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const MAX_FILES: usize = 4096;
const MAX_BYTES: u64 = 64 << 20;
const MAX_FILE_BYTES: u64 = 8 << 20;

struct Session {
    host: Host,
    name: String,
    workspace: PathBuf,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.host.close();
    }
}

/// Invoking `plugin dev` explicitly trusts this source tree, but grants no agent authority.
pub fn run(directory: &Path, once: bool, interval_ms: u64) -> Result<()> {
    ensure!(
        (50..=60_000).contains(&interval_ms),
        "development interval must be 50..60000 ms"
    );
    ensure!(
        fs::symlink_metadata(directory)?.is_dir(),
        "development source must be a real directory"
    );
    let directory = directory.canonicalize()?;
    let mut seen = fingerprint(&directory)?;
    let temporary = tempfile::tempdir().context("creating isolated development store")?;
    let store = PluginStore::new(temporary.path().join("plugins"));
    let interrupt = crate::platform::interrupt::install_sigint();
    let mut session = Some(load(&directory, &store, None)?);
    poll(session.as_ref().unwrap(), &interrupt)?;
    if once {
        return Ok(());
    }
    eprintln!(
        "Watching {} (Ctrl-C to stop); user extension store is untouched",
        directory.display()
    );
    let mut last_error = None;
    while !interrupt.load(Ordering::SeqCst) {
        // Short sleeps keep even a long watch interval responsive to Ctrl-C.
        for _ in 0..interval_ms.div_ceil(50) {
            if interrupt.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if interrupt.load(Ordering::SeqCst) {
            break;
        }
        match fingerprint(&directory) {
            Ok(current) if current != seen => {
                seen = current;
                last_error = None;
                // Closing/dropping first releases the worker's installation lease before update.
                drop(session.take());
                match load(
                    &directory,
                    &store,
                    store.list()?.first().map(|p| p.name.as_str()),
                ) {
                    Ok(loaded) => {
                        session = Some(loaded);
                    }
                    Err(error) => eprintln!(
                        "development reload failed: {error:#}; waiting for a source change"
                    ),
                }
            }
            Ok(_) => {
                last_error = None;
            }
            Err(error) => {
                let diagnostic = format!("{error:#}");
                if last_error.as_ref() != Some(&diagnostic) {
                    eprintln!("development watch failed: {diagnostic}");
                    last_error = Some(diagnostic);
                }
            }
        }
        if let Some(loaded) = &session {
            if let Err(error) = poll(loaded, &interrupt) {
                eprintln!("development poll failed: {error:#}; waiting for a source change");
                drop(session.take());
            }
        }
    }
    Ok(())
}

fn load(directory: &Path, store: &PluginStore, previous: Option<&str>) -> Result<Session> {
    // Check is intentionally non-executing; installation/start below are explicitly trusted here.
    let manifest: Value = serde_json::from_slice(&fs::read(directory.join("dial-plugin.json"))?)?;
    let python = manifest
        .pointer("/python/interpreter")
        .and_then(Value::as_str)
        .unwrap_or("python3");
    print!("{}", authoring::check(directory, Path::new(python))?);
    let plugin = if let Some(name) = previous {
        store.update(Some(name))?;
        store.get(name)?
    } else {
        let source = directory
            .to_str()
            .context("development source path is not UTF-8")?;
        store.install(&format!("file:{source}"))?
    };
    let (program, args, env) = runtime_spec(&plugin)?.context("extension has no runtime")?;
    let session = Session {
        host: Host::default(),
        name: plugin.name,
        workspace: directory.to_owned(),
    };
    session
        .host
        .start_one(&session.name, &program, &args, &env, 10);
    for diagnostic in session.host.take_notices() {
        eprintln!("{diagnostic}");
    }
    ensure!(
        session.host.loaded_names().contains(&session.name),
        "development extension failed to initialize"
    );
    println!("Initialized {}", session.name);
    if let Some(contributions) = session.host.contributions(&session.name) {
        println!("{}", serde_json::to_string_pretty(&contributions)?);
    }
    Ok(session)
}

fn poll(session: &Session, interrupt: &AtomicBool) -> Result<()> {
    let mut callback = |method: &str, envelope: &Value| {
        display_callback(&session.host, &session.name, method, envelope)
    };
    for plugin in session.host.persistent_names() {
        session
            .host
            .request_with_host(
                &plugin,
                "poll",
                json!({"workspace": session.workspace}),
                interrupt,
                &mut callback,
            )
            .with_context(|| format!("polling {plugin}"))?;
    }
    for diagnostic in session.host.take_notices() {
        eprintln!("{}", json!(diagnostic));
    }
    Ok(())
}

fn display_callback(host: &Host, owner: &str, method: &str, envelope: &Value) -> Result<Value> {
    let params = envelope.pointer("/callback/params").unwrap_or(envelope);
    match method {
        "progress" | "events.emit" => {
            // JSON encoding makes terminal controls inert; never print raw extension text.
            ensure!(
                serde_json::to_vec(params)?.len() <= 64 << 10,
                "display event exceeds 64 KiB"
            );
            println!("{method}: {params}");
            Ok(json!({"ok":true}))
        }
        "views.update" => {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .context("view id required")?;
            let revision = params
                .get("revision")
                .and_then(Value::as_u64)
                .context("view revision required")?;
            let mut owned = host
                .views()
                .into_iter()
                .find(|view| view.plugin == owner && view.view.id == id)
                .context("view is not owned by this extension")?;
            owned.view.elements = serde_json::from_value(
                params
                    .get("elements")
                    .context("view elements required")?
                    .clone(),
            )?;
            let revision = host.update_view(owner, owned.view.clone(), revision)?;
            println!("view: {}", serde_json::to_string(&owned.view)?);
            Ok(json!({"revision":revision}))
        }
        "views.remove" => {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .context("view id required")?;
            host.remove_view(owner, id)?;
            println!("removed view: {}", json!(id));
            Ok(json!({"ok":true}))
        }
        _ => bail!(
            "host operation {method:?} is unavailable in development mode; no effectful approval is granted"
        ),
    }
}

/// Content-based, bounded traversal; metadata-only changes do not restart the worker.
fn fingerprint(root: &Path) -> Result<[u8; 32]> {
    fn visit(
        path: &Path,
        depth: usize,
        files: &mut usize,
        bytes: &mut u64,
        hash: &mut Sha256,
    ) -> Result<()> {
        ensure!(
            depth <= 32,
            "development source exceeds 32 directory levels"
        );
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            *files += 1;
            ensure!(
                *files <= MAX_FILES,
                "development source exceeds {MAX_FILES} entries"
            );
            entries.push(entry?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some(
                    ".git"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | "node_modules"
                        | ".tox"
                        | ".pytest_cache"
                )
            ) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            hash.update(name.as_encoded_bytes());
            hash.update([0]);
            if kind.is_dir() {
                hash.update(b"directory");
                visit(&entry.path(), depth + 1, files, bytes, hash)?;
                hash.update(b"end-directory");
            } else if kind.is_file() {
                // Cap actual bytes read too, rather than trusting a racing metadata length.
                let mut content = Vec::new();
                fs::File::open(entry.path())?
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_end(&mut content)?;
                ensure!(
                    content.len() as u64 <= MAX_FILE_BYTES,
                    "development source file exceeds 8 MiB"
                );
                *bytes += content.len() as u64;
                ensure!(*bytes <= MAX_BYTES, "development source exceeds 64 MiB");
                hash.update((content.len() as u64).to_le_bytes());
                hash.update(content);
            }
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    visit(root, 0, &mut 0, &mut 0, &mut hash)?;
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests;
