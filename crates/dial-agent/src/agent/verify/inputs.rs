//! Bounded, content-based identities for explicitly complete verification inputs.
//!
//! No inference of dependencies from command text or filename extensions: absence of a complete
//! snapshot means unknown, never unchanged. Snapshots contain hashes, not source or environment values.
use crate::config::types::VerificationInputs;
use crate::tools::Workspace;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path},
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TIME: Duration = Duration::from_millis(250);

// Wall-clock allowance for a capture. Tests pin it on their own thread: a huge value keeps a loaded
// machine from making a probe and the run it predicts disagree; zero proves exhaustion is unknown.
#[cfg(test)]
thread_local! {
    pub(in crate::agent::verify) static TIME_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

fn budget(limit: Duration) -> Duration {
    #[cfg(test)]
    if let Some(forced) = TIME_OVERRIDE.with(|forced| forced.get()) {
        return forced;
    }
    limit
}

#[derive(Clone, Copy)]
enum IdentityKind {
    Content,
    InstalledRuntime,
}

impl IdentityKind {
    fn entries(self) -> usize {
        match self {
            Self::Content => MAX_ENTRIES,
            Self::InstalledRuntime => 16_384,
        }
    }

    fn time(self) -> Duration {
        budget(match self {
            Self::Content => MAX_TIME,
            Self::InstalledRuntime => Duration::from_secs(1),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::agent) struct Snapshot([u8; 32]);

impl Snapshot {
    pub(in crate::agent) fn digest(&self) -> String {
        crate::hex(&self.0)
    }
}

struct Capture<'a> {
    workspace: &'a Workspace,
    digest: Sha256,
    seen: BTreeSet<String>,
    bytes: u64,
    started: Instant,
    excluded: BTreeSet<String>,
    kind: IdentityKind,
}

/// Declarations are operator-owned contracts, not permission grants. Every visited path passes
/// workspace resolution. Unlike source search, directory membership includes ignored/hidden files.
pub(in crate::agent) fn capture(
    workspace: &Workspace,
    inputs: &VerificationInputs,
) -> Option<Snapshot> {
    capture_excluding(workspace, inputs, BTreeSet::new(), IdentityKind::Content)
}

fn capture_excluding(
    workspace: &Workspace,
    inputs: &VerificationInputs,
    excluded: BTreeSet<String>,
    kind: IdentityKind,
) -> Option<Snapshot> {
    if inputs.paths.is_empty() || inputs.paths.len() > 64 || inputs.environment.len() > 64 {
        return None;
    }
    let mut capture = Capture {
        workspace,
        digest: Sha256::new(),
        seen: BTreeSet::new(),
        bytes: 0,
        started: Instant::now(),
        excluded,
        kind,
    };
    capture.field(b"dial-verification-inputs-v2");
    let mut paths = inputs.paths.clone();
    paths.sort();
    for path in paths {
        capture.visit(Path::new(&path))?;
    }
    let mut environment = inputs.environment.clone();
    environment.sort();
    for name in environment {
        capture.field(name.as_bytes());
        match std::env::var_os(&name) {
            Some(value) => {
                capture.field(b"present");
                capture.field(value.as_encoded_bytes());
            }
            None => capture.field(b"absent"),
        }
    }
    (capture.started.elapsed() <= kind.time()).then(|| Snapshot(capture.digest.finalize().into()))
}

impl Capture<'_> {
    fn field(&mut self, value: &[u8]) {
        self.digest.update((value.len() as u64).to_le_bytes());
        self.digest.update(value);
    }

    /// A declared path. Symlinks are rejected in *every* component, including parents of a declared
    /// file; directory members inherit that check and verify only their own entry.
    fn visit(&mut self, relative: &Path) -> Option<()> {
        if !self.admit(relative)? {
            return Some(());
        }
        let mut path = self.workspace.root().to_path_buf();
        let mut metadata = None;
        for component in relative.components() {
            if !matches!(component, Component::Normal(_)) {
                return None;
            }
            path.push(component);
            metadata = Some(real_entry(&path)?);
        }
        if !self.confined(relative, &path) {
            return None;
        }
        self.entry(relative, &path, metadata?)
    }

    /// Workspace resolution (symlink, protected and sensitive storage policy) returns `path` itself.
    fn confined(&self, relative: &Path, path: &Path) -> bool {
        relative
            .to_str()
            .and_then(|name| self.workspace.resolve(name).ok())
            .is_some_and(|resolved| resolved == path)
    }

    /// `None` makes the capture unknown; `Some(false)` skips an excluded path.
    fn admit(&self, relative: &Path) -> Option<bool> {
        if self.started.elapsed() > self.kind.time() {
            return None;
        }
        let name = relative.to_str()?;
        if self.excluded.contains(name) {
            return Some(false);
        }
        if name.is_empty() || name.contains('\\') {
            return None;
        }
        Some(true)
    }

    /// `path` is `relative` under the root, confined and with every component already checked not
    /// to be a symlink; `before` is the entry's own metadata from that check.
    fn entry(&mut self, relative: &Path, path: &Path, before: fs::Metadata) -> Option<()> {
        let name = relative.to_str()?;
        if !self.seen.insert(name.to_owned()) {
            return Some(());
        }
        if self.seen.len() > self.kind.entries() {
            return None;
        }
        self.field(name.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            self.field(&before.permissions().mode().to_le_bytes());
        }
        self.field(&[u8::from(before.permissions().readonly())]);
        if matches!(self.kind, IdentityKind::InstalledRuntime) {
            self.field(&runtime_identity(&before)?);
        }
        if before.is_dir() {
            self.field(b"directory");
            let mut children = Vec::new();
            for entry in fs::read_dir(path).ok()? {
                if children.len() + self.seen.len() >= self.kind.entries()
                    || self.started.elapsed() > self.kind.time()
                {
                    return None;
                }
                children.push(entry.ok()?.file_name());
            }
            children.sort();
            for child in children {
                let relative = relative.join(&child);
                if !self.admit(&relative)? {
                    continue;
                }
                let path = path.join(&child);
                let metadata = real_entry(&path)?;
                // An installed runtime is its own fresh workspace without protected or sensitive
                // paths, and this member's parents are already confined real directories, so
                // resolution could only return `path` again. Project members may hold protected
                // storage and are always resolved.
                if matches!(self.kind, IdentityKind::Content) && !self.confined(&relative, &path) {
                    return None;
                }
                self.entry(&relative, &path, metadata)?;
            }
        } else if before.is_file() {
            self.field(b"file");
            // An installed runtime file's identity is the single metadata observation above. Nothing
            // is read before a second observation, so one would add no protection, only a syscall.
            if matches!(self.kind, IdentityKind::InstalledRuntime) {
                return Some(());
            }
            if before.len() > MAX_BYTES.saturating_sub(self.bytes) {
                return None;
            }
            let mut file = fs::File::open(path).ok()?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 8192];
            let mut size = 0u64;
            loop {
                let count = file.read(&mut buffer).ok()?;
                if count == 0 {
                    break;
                }
                size += count as u64;
                self.bytes += count as u64;
                if self.bytes > MAX_BYTES || self.started.elapsed() > self.kind.time() {
                    return None;
                }
                hash.update(&buffer[..count]);
            }
            if size != before.len() {
                return None;
            }
            self.field(&hash.finalize());
        } else {
            return None;
        }
        let after = fs::symlink_metadata(path).ok()?;
        if before.file_type() != after.file_type()
            || before.len() != after.len()
            || before.modified().ok()? != after.modified().ok()?
            || before.permissions() != after.permissions()
            || (matches!(self.kind, IdentityKind::InstalledRuntime)
                && runtime_identity(&before)? != runtime_identity(&after)?)
        {
            return None;
        }
        Some(())
    }
}

/// The metadata of `path` when it is a real entry, not a symlink.
fn real_entry(path: &Path) -> Option<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).ok()?;
    (!metadata.file_type().is_symlink()).then_some(metadata)
}

/// ctime detects content replacements even when a tool preserves mtime and size. This identity is
/// only for the installed Unix toolchain; workspace dependencies always use content hashes.
fn runtime_identity(metadata: &fs::Metadata) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let fields = [
            metadata.dev(),
            metadata.ino(),
            metadata.size(),
            metadata.mode() as u64,
            metadata.uid() as u64,
            metadata.gid() as u64,
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ];
        Some(fields.into_iter().flat_map(u64::to_le_bytes).collect())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

mod automatic;
pub(super) use automatic::{PythonRuntime, python_runtime, python_snapshot};

#[cfg(test)]
mod tests;
