//! Immutable extraction reuse keyed by the exact authorized source bytes.
//! Parsers consume a private snapshot, never reopen an independently mutable source path.
use super::Workspace;
use crate::tools::knowledge::evidence::source;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::File,
    io::{Seek, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity([u8; 32]);
impl Identity {
    fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}
pub(super) struct Opened {
    pub path: PathBuf,
    pub identity: Identity,
    pub bytes: Vec<u8>,
}
impl Opened {
    pub fn open(ws: &Workspace, path: &Path) -> Result<Self, String> {
        let input = path.to_str().ok_or("document path must be UTF-8")?;
        let bytes = source::read(ws, input).map_err(|e| format!("{e:#}"))?;
        Ok(Self {
            identity: Identity::of(&bytes),
            bytes,
            path: path.to_owned(),
        })
    }

    /// A seekable private copy for parsers that require a file descriptor.
    pub fn file(&self) -> Result<File, String> {
        let mut file = tempfile::tempfile().map_err(|e| e.to_string())?;
        file.write_all(&self.bytes).map_err(|e| e.to_string())?;
        file.rewind().map_err(|e| e.to_string())?;
        Ok(file)
    }

    pub fn validate(&self, ws: &Workspace) -> Result<(), String> {
        let input = self.path.to_str().ok_or("document path must be UTF-8")?;
        let bytes = source::read(ws, input).map_err(|e| format!("{e:#}"))?;
        if Identity::of(&bytes) != self.identity {
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
