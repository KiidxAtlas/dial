//! One cancellable, bounded completion walk per shell; never walk on the input thread.
use std::path::PathBuf;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 50_000;
const MAX_BYTES: usize = 8 * 1024 * 1024;
// Background-only: slow disks get more time without lifting memory/work bounds.
const WALK_BUDGET: Duration = Duration::from_secs(10);
const CACHE_TTL: Duration = Duration::from_secs(30);

type Request = (u64, PathBuf);
struct Shared {
    request: Mutex<Option<Request>>,
    wake: Condvar,
    generation: AtomicU64,
    stop: AtomicBool,
}
pub(super) struct Files {
    shared: Arc<Shared>,
    result: Arc<Mutex<Option<(u64, Listing)>>>,
    root: Option<PathBuf>,
    requested: Instant,
    running: bool,
    /// Unchanged rescans must not restart the composer's vocabulary scan.
    delivered: Option<u64>,
    /// Warning identity is independent of paths, and resets only on a root change.
    warned: Vec<Cause>,
}
pub(super) struct Listing {
    pub paths: Vec<String>,
    pub diagnostic: Option<String>,
    causes: Vec<Cause>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Cause {
    Entries(usize),
    Bytes(usize),
    Time(Duration),
    Unreadable,
}

fn diagnostic(causes: &[Cause]) -> Option<String> {
    if causes.is_empty() {
        return None;
    }
    let reasons: Vec<_> = causes.iter().map(|cause| match cause {
        Cause::Entries(limit) => format!("scan reached {limit} entries (files and directories); exclude generated/dependency directories with .gitignore or .ignore"),
        Cause::Bytes(limit) => format!("cached path names reached the {limit}-byte budget; exclude generated/dependency directories with .gitignore or .ignore"),
        Cause::Time(limit) => format!("scan reached its {}-second time budget; try a narrower project root or ignore large generated directories", limit.as_secs_f64()),
        Cause::Unreadable => "one or more paths could not be read; check that the project exists and its directories are readable".into(),
    }).collect();
    Some(format!(
        "File completion is partial: {}. Type a path explicitly to attach files outside this cache.",
        reasons.join("; ")
    ))
}

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: usize,
    time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: MAX_ENTRIES,
            bytes: MAX_BYTES,
            time: WALK_BUDGET,
        }
    }
}
impl Files {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            request: Mutex::new(None),
            wake: Condvar::new(),
            generation: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let result = Arc::new(Mutex::new(None));
        let output = result.clone();
        let worker = shared.clone();
        std::thread::spawn(move || {
            loop {
                let request = {
                    let mut slot = worker.request.lock().unwrap();
                    while slot.is_none() && !worker.stop.load(Ordering::Acquire) {
                        slot = worker.wake.wait(slot).unwrap();
                    }
                    if worker.stop.load(Ordering::Acquire) {
                        return;
                    }
                    slot.take().unwrap()
                };
                let (generation, root) = request;
                if let Some(listing) = walk(&root, || {
                    worker.stop.load(Ordering::Acquire)
                        || worker.generation.load(Ordering::Acquire) != generation
                }) {
                    // One replaceable result slot bounds memory and never blocks the worker.
                    *output.lock().unwrap() = Some((generation, listing));
                }
            }
        });
        Self {
            shared,
            result,
            root: None,
            requested: Instant::now() - CACHE_TTL,
            running: false,
            delivered: None,
            warned: Vec::new(),
        }
    }
    pub fn stop(&self) {
        let mut pending = self.shared.request.lock().unwrap();
        self.shared.stop.store(true, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        pending.take();
        self.shared.wake.notify_one();
    }
    pub fn request(&mut self, root: &std::path::Path) {
        if self.shared.stop.load(Ordering::Acquire) {
            return;
        }
        let changed = self.root.as_deref() != Some(root);
        if !changed && (self.running || self.requested.elapsed() < CACHE_TTL) {
            return;
        }
        if changed {
            self.warned.clear();
        }
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.root = Some(root.to_owned());
        self.requested = Instant::now();
        self.running = true;
        // Discard cached results from the previous generation.
        self.result.lock().unwrap().take();
        *self.shared.request.lock().unwrap() = Some((generation, root.to_owned()));
        self.shared.wake.notify_one();
    }
    pub fn poll(&mut self) -> Option<Listing> {
        let result = self.result.lock().unwrap().take();
        if let Some((generation, mut listing)) = result
            && generation == self.shared.generation.load(Ordering::Acquire)
        {
            self.running = false;
            let digest = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                self.root.hash(&mut hasher);
                listing.paths.hash(&mut hasher);
                listing.causes.hash(&mut hasher);
                hasher.finish()
            };
            if self.delivered == Some(digest) {
                return None;
            }
            self.delivered = Some(digest);
            let new_causes: Vec<_> = listing
                .causes
                .iter()
                .copied()
                .filter(|cause| !self.warned.contains(cause))
                .collect();
            listing.diagnostic = diagnostic(&new_causes);
            self.warned.extend(new_causes);
            return Some(listing);
        }
        None
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        self.stop();
        // Do not join a filesystem syscall from the UI thread. There is at most one worker,
        // which exits at the next traversal boundary, even if the shell has already gone.
    }
}
fn walk(root: &std::path::Path, cancelled: impl Fn() -> bool) -> Option<Listing> {
    walk_bounded(root, cancelled, Limits::default())
}
fn walk_bounded(
    root: &std::path::Path,
    cancelled: impl Fn() -> bool,
    limits: Limits,
) -> Option<Listing> {
    let start = Instant::now();
    let mut paths = Vec::new();
    let mut bytes = 0;
    let mut causes = Vec::new();
    for (seen, entry) in ignore::WalkBuilder::new(root)
        .follow_links(false)
        .require_git(false)
        .build()
        .enumerate()
    {
        if cancelled() {
            return None;
        }
        if seen >= limits.entries {
            causes.push(Cause::Entries(limits.entries));
            break;
        }
        if start.elapsed() >= limits.time {
            causes.push(Cause::Time(limits.time));
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                if !causes.contains(&Cause::Unreadable) {
                    causes.push(Cause::Unreadable);
                }
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(path) = entry.path().strip_prefix(root) {
            let path = path.to_string_lossy();
            if path.chars().any(|c| c.is_control() || c.is_whitespace()) {
                continue;
            }
            if bytes + path.len() > limits.bytes {
                causes.push(Cause::Bytes(limits.bytes));
                break;
            }
            bytes += path.len();
            paths.push(path.into_owned());
        }
    }
    if cancelled() {
        return None;
    }
    paths.sort();
    Some(Listing {
        paths,
        diagnostic: diagnostic(&causes),
        causes,
    })
}

#[cfg(test)]
mod tests;
