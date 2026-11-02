//! Immutable, workspace-scoped evidence. Every lookup follows a bounded fresh read and SHA256.
use super::{declarations, tokens};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

pub(super) const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 4096;

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

#[derive(Clone)]
pub(super) struct Source {
    text: String,
    folded: String,
    starts: Vec<usize>,
    folded_starts: Vec<usize>,
    pub(super) postings: BTreeMap<String, Vec<usize>>,
    pub(super) path_terms: BTreeSet<String>,
    pub(super) declarations: Vec<(usize, usize, String)>,
    syntax_indexed: bool,
}
impl Source {
    fn new(relative: &str, text: &str) -> Self {
        let mut source = Self {
            text: String::with_capacity(text.len()),
            folded: String::with_capacity(text.len()),
            starts: Vec::new(),
            folded_starts: Vec::new(),
            postings: BTreeMap::new(),
            path_terms: tokens(relative),
            declarations: Vec::new(),
            syntax_indexed: false,
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
        source
    }
    fn decoded_bytes(&self) -> usize {
        // Charge allocated capacities, including conservative BTree node overhead,
        // not compressed file bytes. Oversized indexes remain accurate but uncached.
        std::mem::size_of::<Self>()
            + self.text.capacity()
            + self.folded.capacity()
            + (self.starts.capacity() + self.folded_starts.capacity())
                * std::mem::size_of::<usize>()
            + self
                .postings
                .iter()
                .map(|(term, lines)| {
                    128 + term.capacity() + lines.capacity() * std::mem::size_of::<usize>()
                })
                .sum::<usize>()
            + self
                .path_terms
                .iter()
                .map(|term| 128 + term.capacity())
                .sum::<usize>()
            + self.declarations.capacity() * std::mem::size_of::<(usize, usize, String)>()
            + self
                .declarations
                .iter()
                .map(|(_, _, kind)| kind.capacity())
                .sum::<usize>()
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
    /// Position in `Cache::order`; larger is more recent.
    stamp: u64,
    /// The sweep (one `find` retrieval) that last used this entry.
    sweep: u64,
}
type Key = (PathBuf, PathBuf);
/// Keyed by (scope, file) with a recency index. Eviction is scan-resistant: a retrieval sweeps
/// every candidate file in order, which under plain LRU on a repository larger than the cache
/// evicts each entry just before the next sweep needs it (no hits at all). Entries used in the
/// current sweep are never evicted to admit new ones, so later sweeps hit what earlier ones kept.
#[derive(Default)]
struct Cache {
    entries: std::collections::HashMap<Key, Entry>,
    order: BTreeMap<u64, Key>,
    next_stamp: u64,
    sweep: u64,
    bytes: usize,
}
impl Cache {
    fn remove(&mut self, key: &Key) -> Option<Entry> {
        let entry = self.entries.remove(key)?;
        self.order.remove(&entry.stamp);
        self.bytes -= entry.bytes;
        Some(entry)
    }
    fn insert(&mut self, key: Key, mut entry: Entry) {
        self.remove(&key);
        entry.stamp = self.next_stamp;
        entry.sweep = self.sweep;
        self.next_stamp += 1;
        self.bytes += entry.bytes;
        self.order.insert(entry.stamp, key.clone());
        self.entries.insert(key, entry);
    }
    /// Makes room for `bytes` by evicting entries the current sweep has not used, oldest first.
    /// `false` when only entries this sweep relies on are left.
    fn make_room(&mut self, bytes: usize) -> bool {
        while self.entries.len() >= MAX_CACHE_ENTRIES || self.bytes + bytes > MAX_CACHE_BYTES {
            let Some(key) = self.order.values().next().cloned() else {
                return false;
            };
            if self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.sweep == self.sweep)
            {
                return false;
            }
            self.remove(&key);
        }
        true
    }
    fn keys_where(&self, mut keep: impl FnMut(&Key, &Entry) -> bool) -> Vec<Key> {
        self.entries
            .iter()
            .filter(|(key, entry)| !keep(key, entry))
            .map(|(key, _)| key.clone())
            .collect()
    }
}
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));
pub(super) fn invalidate(path: &Path) {
    if let Ok(mut cache) = CACHE.lock() {
        let stale =
            cache.keys_where(|(root, file), _| !(file.starts_with(path) || root.starts_with(path)));
        for key in stale {
            cache.remove(&key);
        }
    }
}
/// Drops entries of `root` whose file changed, and starts a new retrieval sweep.
pub(super) fn prune(root: &Path) {
    if let Ok(mut cache) = CACHE.lock() {
        cache.sweep += 1;
        let stale = cache.keys_where(|(scope, path), entry| {
            scope != root
                || fs::symlink_metadata(path).is_ok_and(|meta| {
                    meta.is_file()
                        && !meta.file_type().is_symlink()
                        && Identity::of(&meta) == entry.identity
                })
        });
        for key in stale {
            cache.remove(&key);
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
    accepts: impl FnOnce(&Source) -> bool,
) -> Option<Arc<Source>> {
    let key: Key = (root.to_path_buf(), path.to_path_buf());
    let cached = CACHE.lock().ok().and_then(|mut cache| {
        let entry = cache.remove(&key)?;
        if entry.identity != *identity || entry.revision != revision {
            return None;
        }
        let source = Arc::clone(&entry.source);
        cache.insert(key.clone(), entry);
        Some(source)
    });
    let was_cached = cached.is_some();
    let mut source = cached.unwrap_or_else(|| Arc::new(Source::new(relative, text)));
    let accepted = accepts(&source);
    if was_cached && (!accepted || source.syntax_indexed) {
        return accepted.then_some(source);
    }
    // Retain query-independent token indexes even for nonmatches, so the next query
    // can reuse them. Only matching files need syntax; never cache a negative verdict.
    if accepted {
        let source = Arc::make_mut(&mut source);
        source.declarations = declarations(path, text);
        source.syntax_indexed = true;
    }
    let bytes = source.decoded_bytes() + 256 + root.as_os_str().len() + path.as_os_str().len();
    if bytes <= MAX_CACHE_BYTES
        && let Ok(mut cache) = CACHE.lock()
    {
        // A concurrent reader may have installed this path while we parsed it.
        cache.remove(&key);
        if cache.make_room(bytes) {
            cache.insert(
                key,
                Entry {
                    identity: identity.clone(),
                    revision,
                    source: Arc::clone(&source),
                    bytes,
                    stamp: 0,
                    sweep: 0,
                },
            );
        }
    }
    accepted.then_some(source)
}

#[cfg(test)]
mod tests;
