//! Shared writer coordination and conservative three-way text rebasing.
//! Only cooperating Dial commits are serialized. External editors remain protected by the
//! platform transaction's exact identity checks. Whole-file replacements and projections never
//! acquire implicit merge semantics.
use super::{
    Workspace,
    edit_transaction::{Change, read},
};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    marker::PhantomData,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

thread_local! {
    static LEASES: RefCell<HashMap<PathBuf, (usize, File)>> = RefCell::new(HashMap::new());
}

/// Reentrant on the owning thread, exclusive across threads and processes. Never move a lease
/// between threads: native tool callbacks can recursively enter the same document authority.
pub(crate) struct Lease {
    _documents: Vec<DocumentLease>,
}
impl Lease {
    pub fn acquire(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, String> {
        // A total path order prevents cross-document deadlocks. Recursive callbacks may reuse
        // already held documents, but may not acquire a new path below their current lock order.
        let paths: std::collections::BTreeSet<_> = paths.into_iter().collect();
        let inversion = LEASES.with(|held| {
            let held = held.borrow();
            paths
                .iter()
                .any(|path| !held.contains_key(path) && held.keys().any(|prior| prior > path))
        });
        if inversion {
            return Err("nested document acquisition would invert lock order; submit one atomic document batch".into());
        }
        let documents = paths
            .into_iter()
            .map(DocumentLease::acquire)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            _documents: documents,
        })
    }
}

struct DocumentLease {
    root: PathBuf,
    _thread: PhantomData<Rc<()>>,
}
impl DocumentLease {
    fn acquire(root: PathBuf) -> Result<Self, String> {
        if LEASES.with(|leases| {
            let mut leases = leases.borrow_mut();
            if let Some((depth, _)) = leases.get_mut(&root) {
                *depth += 1;
                true
            } else {
                false
            }
        }) {
            return Ok(Self {
                root,
                _thread: PhantomData,
            });
        }
        #[cfg(unix)]
        let owner = unsafe { libc::geteuid() }.to_string();
        #[cfg(not(unix))]
        let owner = format!(
            "{:x}",
            Sha256::digest(std::env::temp_dir().as_os_str().as_encoded_bytes())
        );
        let directory = std::env::temp_dir().join(format!("dial-document-locks-{owner}"));
        crate::storage::settings_store::private_dir(&directory).map_err(|e| e.to_string())?;
        let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = crate::platform::fs::open_owner_only(&options, &directory.join(key))
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && started.elapsed() < Duration::from_secs(10) =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => return Err(format!("document writer unavailable: {error}")),
            }
        }
        LEASES.with(|leases| leases.borrow_mut().insert(root.clone(), (1, file)));
        Ok(Self {
            root,
            _thread: PhantomData,
        })
    }
}
impl Drop for DocumentLease {
    fn drop(&mut self) {
        LEASES.with(|leases| {
            let mut leases = leases.borrow_mut();
            if let Some((depth, _)) = leases.get_mut(&self.root) {
                *depth -= 1;
                if *depth == 0 {
                    leases.remove(&self.root);
                }
            }
        });
    }
}

/// Refresh stale proposals while holding a writer lease. Callers validate and render the merged
/// proposal normally, then commit under the same lease. Seen-line grants never expand by merging.
pub(crate) fn rebase(ws: &Workspace, changes: &mut [Change]) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for change in changes {
        let (Some(before), Some(after)) = (&change.before, &change.after) else {
            continue;
        };
        let Some(current) = read(ws, &change.path)? else {
            continue;
        };
        if current.text == before.text {
            continue;
        }
        if change.authored_whole || change.projection.is_some() {
            return Err(format!(
                "{} changed; whole-file or projected edits require a fresh read",
                ws.rel(&change.path)
            ));
        }
        let merged = merge(&before.text, after, &current.text).ok_or_else(|| {
            format!(
                "{} has overlapping or ambiguous concurrent edits; re-read it",
                ws.rel(&change.path)
            )
        })?;
        if let Some(flags) = &change.seen_after {
            change.seen_after = Some(super::hashline::transition_flags(after, &merged, flags));
        }
        change.after = Some(merged);
        change.before = Some(current);
        notes.push(format!(
            "{}: rebased non-overlapping concurrent changes",
            ws.rel(&change.path)
        ));
    }
    Ok(notes)
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}
fn edits(base: &str, next: &str) -> Vec<Edit> {
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(250))
        .diff_lines(base, next);
    diff.ops()
        .iter()
        .filter(|op| op.tag() != similar::DiffTag::Equal)
        .map(|op| {
            let old = op.old_range();
            let new = op.new_range();
            Edit {
                start: old.start,
                end: old.end,
                text: diff.new_slices()[new].concat(),
            }
        })
        .collect()
}
fn overlaps(a: &Edit, b: &Edit) -> bool {
    if a.start == a.end || b.start == b.end {
        a.start <= b.end && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}
fn merge(base: &str, proposed: &str, current: &str) -> Option<String> {
    let mut ours = edits(base, proposed);
    let theirs = edits(base, current);
    if ours.iter().any(|a| theirs.iter().any(|b| overlaps(a, b))) {
        return None;
    }
    ours.extend(theirs);
    ours.sort_by_key(|edit| edit.start);
    let lines: Vec<_> = base.split_inclusive('\n').collect();
    let mut result = String::new();
    let mut cursor = 0;
    for edit in ours {
        result.extend(lines[cursor..edit.start].iter().copied());
        result.push_str(&edit.text);
        cursor = edit.end;
    }
    result.extend(lines[cursor..].iter().copied());
    Some(result)
}

#[cfg(test)]
mod tests;
