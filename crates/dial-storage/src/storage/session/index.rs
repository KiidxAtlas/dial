//! Cached listing metadata. Summarizing a journal means parsing all of it, which made the home
//! screen and the resume picker stall on large histories. Each project's state directory keeps
//! one summary per journal, keyed by the journal's length and modification time, so a listing
//! parses only the journals that changed since the last one. The cache is advisory: a missing,
//! unreadable or stale file only costs the parse it would have saved.

use super::SessionInfo;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Bump when `SessionInfo` or the summary rules change, so older summaries are recomputed.
const VERSION: u32 = 1;
const FILE: &str = "session-index.json";

/// The identity a summary was computed for. Appends change the length; rewrites change the
/// modification time.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stamp {
    len: u64,
    modified: u64,
}

impl Stamp {
    pub(super) fn of(path: &Path) -> Option<Self> {
        let meta = fs::symlink_metadata(path).ok()?;
        let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            len: meta.len(),
            modified: u64::try_from(modified.as_nanos()).ok()?,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    info: SessionInfo,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

struct Dir {
    index: Index,
    dirty: bool,
}

/// Summaries for the state directories one listing touches; `save` writes back what changed.
#[derive(Default)]
pub(super) struct Cache {
    dirs: HashMap<PathBuf, Dir>,
}

/// `<state>/sessions/<file>` → (`<state>`, `<file>`).
fn locate(path: &Path) -> Option<(PathBuf, String)> {
    let name = path.file_name()?.to_str()?.to_owned();
    let state = path.parent()?.parent()?.to_path_buf();
    Some((state, name))
}

impl Cache {
    fn dir(&mut self, state: PathBuf) -> &mut Dir {
        self.dirs.entry(state).or_insert_with_key(|state| {
            let index = crate::platform::atomic::read_private(&state.join(FILE))
                .ok()
                .flatten()
                .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
                .filter(|index| index.version == VERSION)
                .unwrap_or_default();
            Dir {
                index,
                dirty: false,
            }
        })
    }

    /// The cached summary when the journal still has the stamp it was computed for.
    pub(super) fn get(&mut self, path: &Path, stamp: Stamp) -> Option<SessionInfo> {
        let (state, name) = locate(path)?;
        let entry = self.dir(state).index.entries.get(&name)?;
        (entry.stamp == stamp).then(|| SessionInfo {
            path: path.to_path_buf(),
            ..entry.info.clone()
        })
    }

    /// Record a summary computed from a journal that had `stamp` before it was read.
    pub(super) fn put(&mut self, path: &Path, stamp: Stamp, info: &SessionInfo) {
        let Some((state, name)) = locate(path) else {
            return;
        };
        let dir = self.dir(state);
        dir.index.entries.insert(
            name,
            Entry {
                stamp,
                info: info.clone(),
            },
        );
        dir.dirty = true;
    }

    /// Write back changed indexes, dropping summaries of journals that no longer exist.
    /// Failures are ignored: the next listing recomputes what could not be saved.
    pub(super) fn save(self) {
        for (state, mut dir) in self.dirs {
            if !dir.dirty {
                continue;
            }
            let sessions = state.join("sessions");
            dir.index
                .entries
                .retain(|name, _| sessions.join(name).is_file());
            dir.index.version = VERSION;
            if let Ok(bytes) = serde_json::to_vec(&dir.index) {
                let _ = crate::platform::atomic::write_private(&state.join(FILE), &bytes);
            }
        }
    }
}
