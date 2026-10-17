//! Immutable extraction reuse. Identity comes from an opened no-follow regular file,
//! never from a pathname stat alone. Nothing is published after a revision change.
use super::{MAX_INPUT, Workspace};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fs::{File, Metadata},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    file: crate::platform::FileIdentity,
    size: u64,
    modified: std::time::SystemTime,
}
impl Identity {
    fn of(file: &File, meta: &Metadata) -> Result<Self, String> {
        if !meta.is_file() || meta.len() > MAX_INPUT {
            return Err("rich document must be a regular file no larger than 64 MiB".into());
        }
        Ok(Self {
            file: crate::platform::fs::identity(file).map_err(|e| e.to_string())?,
            size: meta.len(),
            modified: meta.modified().map_err(|e| e.to_string())?,
        })
    }
}
pub(super) struct Opened {
    pub file: File,
    pub path: PathBuf,
    pub identity: Identity,
}
fn open_file(ws: &Workspace, path: &Path) -> Result<File, String> {
    let relative = path
        .strip_prefix(ws.root())
        .map_err(|_| "document is outside the workspace")?;
    if relative.as_os_str().is_empty() {
        return Err("document path must name a file".into());
    }
    crate::platform::fs::open_beneath(ws.root(), relative, false)
        .map_err(|e| format!("opening rich document: {e}"))
}
impl Opened {
    pub fn open(ws: &Workspace, path: &Path) -> Result<Self, String> {
        // Authorization precedes even a global cache hit.
        let authorized = ws.resolve(path.to_str().ok_or("document path must be UTF-8")?)?;
        if authorized != path {
            return Err("document path changed during resolution".into());
        }
        if let Some(crate::tools::exec::sandbox::Policy::Dev { deny_read, .. }) =
            ws.policy.as_ref().filter(|_| !ws.unsandboxed)
        {
            for denied in deny_read {
                let denied = crate::tools::exec::sandbox::resolve(denied)?;
                if path.starts_with(denied) {
                    return Err("document access denied by workspace policy".into());
                }
            }
        }
        let file = open_file(ws, path)?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        let identity = Identity::of(&file, &metadata)?;
        Ok(Self {
            file,
            path: path.to_owned(),
            identity,
        })
    }
    pub fn validate(&self, ws: &Workspace) -> Result<(), String> {
        if Identity::of(
            &self.file,
            &self.file.metadata().map_err(|e| e.to_string())?,
        )? != self.identity
            || Self::open(ws, &self.path)?.identity != self.identity
        {
            return Err("rich document changed while reading; retry the read".into());
        }
        Ok(())
    }
}
struct TextEntry {
    path: PathBuf,
    identity: Identity,
    format: String,
    selector: Option<String>,
    text: Arc<String>,
}
#[derive(Default)]
struct TextCache {
    entries: VecDeque<TextEntry>,
    bytes: usize,
}
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEXT_ENTRIES: usize = 128;
static TEXT: LazyLock<Mutex<TextCache>> = LazyLock::new(|| Mutex::new(TextCache::default()));
pub(super) fn extracted(
    opened: &Opened,
    ws: &Workspace,
    format: &str,
    selector: Option<&str>,
    extract: impl FnOnce() -> Result<String, String>,
) -> Result<Arc<String>, String> {
    {
        let mut cache = TEXT.lock();
        // Changed revisions never leave stale successes reachable or pinned in the cache.
        let mut i = 0;
        while i < cache.entries.len() {
            if cache.entries[i].path == opened.path && cache.entries[i].identity != opened.identity
            {
                let old = cache.entries.remove(i).unwrap();
                cache.bytes -= old.text.capacity();
            } else {
                i += 1;
            }
        }
        if let Some(i) = cache.entries.iter().position(|e| {
            e.path == opened.path
                && e.identity == opened.identity
                && e.format == format
                && e.selector.as_deref() == selector
        }) {
            let entry = cache.entries.remove(i).unwrap();
            let text = Arc::clone(&entry.text);
            cache.entries.push_back(entry);
            drop(cache);
            opened.validate(ws)?;
            return Ok(text);
        }
    }
    let result = Arc::new(extract()?);
    opened.validate(ws)?;
    if result.len() > super::MAX_CONTENT as usize {
        return Err("extracted text exceeds 8 MiB".into());
    }
    if result.capacity() > MAX_BYTES {
        return Ok(result);
    }
    let mut cache = TEXT.lock();
    // Concurrent identical misses replace, rather than multiply, retained data.
    if let Some(i) = cache.entries.iter().position(|e| {
        e.path == opened.path && e.format == format && e.selector.as_deref() == selector
    }) {
        let old = cache.entries.remove(i).unwrap();
        cache.bytes -= old.text.capacity();
    }
    while cache.bytes + result.capacity() > MAX_BYTES || cache.entries.len() >= MAX_TEXT_ENTRIES {
        let Some(old) = cache.entries.pop_front() else {
            break;
        };
        cache.bytes -= old.text.capacity();
    }
    cache.bytes += result.capacity();
    cache.entries.push_back(TextEntry {
        path: opened.path.clone(),
        identity: opened.identity,
        format: format.into(),
        selector: selector.map(str::to_owned),
        text: Arc::clone(&result),
    });
    Ok(result)
}
