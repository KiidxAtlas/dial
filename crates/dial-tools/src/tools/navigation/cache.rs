//! Immutable, workspace-scoped evidence. Every lookup follows a bounded fresh read and SHA256.
use super::{declarations, tokens};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

pub(super) const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Identity {
    size: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime: (i64, i64),
    #[cfg(unix)]
    ctime: (i64, i64),
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}
impl Identity {
    pub(super) fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            size: meta.len(),
            #[cfg(unix)]
            dev: meta.dev(),
            #[cfg(unix)]
            ino: meta.ino(),
            #[cfg(unix)]
            mtime: (meta.mtime(), meta.mtime_nsec()),
            #[cfg(unix)]
            ctime: (meta.ctime(), meta.ctime_nsec()),
            #[cfg(not(unix))]
            modified: meta.modified().ok(),
            #[cfg(not(unix))]
            created: meta.created().ok(),
        }
    }
}

pub(super) struct Source {
    text: String,
    folded: String,
    starts: Vec<usize>,
    folded_starts: Vec<usize>,
    pub(super) postings: BTreeMap<String, Vec<usize>>,
    pub(super) path_terms: BTreeSet<String>,
    pub(super) declarations: Vec<(usize, usize, String)>,
    decoded_bytes: usize,
}
impl Source {
    fn new(path: &Path, relative: &str, text: &str) -> Self {
        let mut source = Self {
            text: String::with_capacity(text.len()),
            folded: String::with_capacity(text.len()),
            starts: Vec::new(),
            folded_starts: Vec::new(),
            postings: BTreeMap::new(),
            path_terms: tokens(relative),
            declarations: declarations(path, text),
            decoded_bytes: 0,
        };
        for (index, line) in text.lines().enumerate() {
            if index > 0 {
                source.text.push('\n');
                source.folded.push('\n');
            }
            source.starts.push(source.text.len());
            source.folded_starts.push(source.folded.len());
            source.text.push_str(line);
            source.folded.push_str(&line.to_lowercase());
            for term in tokens(line) {
                source.postings.entry(term).or_default().push(index + 1);
            }
        }
        // Charge allocated capacities, including conservative BTree node overhead,
        // not compressed file bytes. Oversized indexes remain accurate but uncached.
        source.decoded_bytes = std::mem::size_of::<Self>()
            + source.text.capacity()
            + source.folded.capacity()
            + (source.starts.capacity() + source.folded_starts.capacity())
                * std::mem::size_of::<usize>()
            + source
                .postings
                .iter()
                .map(|(term, lines)| {
                    128 + term.capacity() + lines.capacity() * std::mem::size_of::<usize>()
                })
                .sum::<usize>()
            + source
                .path_terms
                .iter()
                .map(|term| 128 + term.capacity())
                .sum::<usize>()
            + source.declarations.capacity() * std::mem::size_of::<(usize, usize, String)>()
            + source
                .declarations
                .iter()
                .map(|(_, _, kind)| kind.capacity())
                .sum::<usize>();
        source
    }
    pub(super) fn lines(&self) -> usize {
        self.starts.len()
    }
    fn slice<'a>(text: &'a str, starts: &[usize], start: usize, end: usize) -> &'a str {
        let begin = starts[start - 1];
        let end = starts.get(end).map_or(text.len(), |next| next - 1);
        &text[begin..end]
    }
    pub(super) fn body(&self, start: usize, end: usize) -> &str {
        Self::slice(&self.text, &self.starts, start, end)
    }
    pub(super) fn folded(&self, start: usize, end: usize) -> &str {
        Self::slice(&self.folded, &self.folded_starts, start, end)
    }
}
struct Entry {
    identity: Identity,
    revision: [u8; 32],
    source: Arc<Source>,
    bytes: usize,
}
#[derive(Default)]
struct Cache {
    // Oldest first. A tiny fixed entry ceiling makes a linear LRU boring and bounded.
    entries: Vec<((PathBuf, PathBuf), Entry)>,
    bytes: usize,
}
impl Cache {
    fn remove(&mut self, index: usize) -> Entry {
        let (_, entry) = self.entries.remove(index);
        self.bytes -= entry.bytes;
        entry
    }
}
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));
pub(super) fn invalidate(path: &Path) {
    if let Ok(mut cache) = CACHE.lock() {
        for index in (0..cache.entries.len()).rev() {
            let (root, file) = &cache.entries[index].0;
            if file.starts_with(path) || root.starts_with(path) {
                cache.remove(index);
            }
        }
    }
}
pub(super) fn prune(root: &Path) {
    if let Ok(mut cache) = CACHE.lock() {
        for index in (0..cache.entries.len()).rev() {
            let ((scope, path), entry) = &cache.entries[index];
            if scope == root
                && !fs::symlink_metadata(path).is_ok_and(|meta| {
                    meta.is_file()
                        && !meta.file_type().is_symlink()
                        && Identity::of(&meta) == entry.identity
                })
            {
                cache.remove(index);
            }
        }
    }
}
pub(super) fn evidence(
    root: &Path,
    path: &Path,
    relative: &str,
    identity: &Identity,
    revision: [u8; 32],
    text: &str,
) -> Arc<Source> {
    if let Ok(mut cache) = CACHE.lock()
        && let Some(index) = cache
            .entries
            .iter()
            .position(|((scope, file), _)| scope == root && file == path)
    {
        let entry = cache.remove(index);
        if entry.identity == *identity && entry.revision == revision {
            let source = Arc::clone(&entry.source);
            cache.bytes += entry.bytes;
            cache
                .entries
                .push(((root.to_path_buf(), path.to_path_buf()), entry));
            return source;
        }
    }
    // Poisoning, an eviction or an uncacheable source uses the same evidence algorithm.
    let source = Arc::new(Source::new(path, relative, text));
    let bytes = source.decoded_bytes + 256 + root.as_os_str().len() + path.as_os_str().len();
    if bytes <= MAX_CACHE_BYTES
        && let Ok(mut cache) = CACHE.lock()
    {
        // A concurrent reader may have installed this path while we parsed it.
        if let Some(index) = cache
            .entries
            .iter()
            .position(|((scope, file), _)| scope == root && file == path)
        {
            cache.remove(index);
        }
        while cache.entries.len() >= MAX_CACHE_ENTRIES || cache.bytes + bytes > MAX_CACHE_BYTES {
            cache.remove(0);
        }
        cache.bytes += bytes;
        cache.entries.push((
            (root.to_path_buf(), path.to_path_buf()),
            Entry {
                identity: identity.clone(),
                revision,
                source: Arc::clone(&source),
                bytes,
            },
        ));
    }
    source
}
