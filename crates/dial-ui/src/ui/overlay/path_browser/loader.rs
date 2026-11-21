//! One bounded mailbox per browser. Filesystem calls never run on the input/render thread.
use super::{Entry, MAX_ENTRIES};
use crate::config::schema::PathKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

// A stalled network filesystem cannot create unbounded detached workers across reopened panels.
static WORKERS: AtomicUsize = AtomicUsize::new(0);
const MAX_WORKERS: usize = 16;

pub(super) enum Request {
    Open {
        path: Option<PathBuf>,
        base: PathBuf,
    },
    List {
        dir: PathBuf,
        select: Option<String>,
    },
    Validate(PathBuf),
}

pub(super) enum Reply {
    Listing {
        dir: PathBuf,
        entries: Vec<Entry>,
        note: Option<String>,
        select: Option<String>,
    },
    Validated(PathBuf),
    Error(String),
}

#[derive(Default)]
struct State {
    generation: u64,
    request: Option<Request>,
    reply: Option<(u64, Reply)>,
    closed: bool,
}

pub(super) struct Loader(Arc<(Mutex<State>, Condvar)>);

impl Loader {
    pub(super) fn new(kind: PathKind) -> Result<Self, String> {
        Self::start(move |request, stale| load(kind, request, stale))
    }

    pub(super) fn start(
        handler: impl Fn(Request, &dyn Fn() -> bool) -> Reply + Send + 'static,
    ) -> Result<Self, String> {
        WORKERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_WORKERS).then_some(n + 1)
            })
            .map_err(|_| {
                "Filesystem workers are busy; close and reopen the browser to retry".to_owned()
            })?;
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        if let Err(error) = std::thread::Builder::new()
            .name("path-browser".into())
            .spawn(move || {
                struct Permit;
                impl Drop for Permit {
                    fn drop(&mut self) {
                        WORKERS.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _permit = Permit;
                let (lock, wake) = &*worker;
                loop {
                    let (generation, request) = {
                        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while state.request.is_none() && !state.closed {
                            state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                        if state.closed {
                            return;
                        }
                        (
                            state.generation,
                            state.request.take().expect("request is ready"),
                        )
                    };
                    let stale = || {
                        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        state.closed || state.generation != generation
                    };
                    let reply = handler(request, &stale);
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if !state.closed && state.generation == generation {
                        state.reply = Some((generation, reply));
                    }
                }
            })
        {
            WORKERS.fetch_sub(1, Ordering::AcqRel);
            return Err(format!("Cannot start filesystem worker: {error}"));
        }
        Ok(Self(shared))
    }

    pub(super) fn request(&self, request: Request) {
        let mut state = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.reply = None;
        state.request = Some(request);
        self.0.1.notify_one();
    }

    pub(super) fn cancel(&self) {
        let mut state = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.request = None;
        state.reply = None;
    }

    pub(super) fn collect(&self) -> Option<Reply> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reply
            .take()
            .map(|(_, reply)| reply)
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.0.1.notify_one();
    }
}

fn load(kind: PathKind, request: Request, stale: impl Fn() -> bool) -> Reply {
    match request {
        Request::Open { path, base } => {
            let (dir, select) = match path {
                Some(path) if path.is_dir() => (path, None),
                Some(path) => {
                    let dir = path
                        .parent()
                        .filter(|p| p.is_dir())
                        .map_or(base, |p| p.to_path_buf());
                    let select = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
                    (dir, select)
                }
                None => (base, None),
            };
            listing(kind, dir, select, stale)
        }
        Request::List { dir, select } => listing(kind, dir, select, stale),
        Request::Validate(path) => match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => listing(kind, path, None, stale),
            Ok(_) if kind != PathKind::Directory => Reply::Validated(path),
            Ok(_) => Reply::Error(format!("{} is not a folder", path.display())),
            Err(error) => Reply::Error(format!("Cannot open {}: {error}", path.display())),
        },
    }
}

fn listing(
    kind: PathKind,
    dir: PathBuf,
    select: Option<String>,
    stale: impl Fn() -> bool,
) -> Reply {
    let mut entries = Vec::new();
    let mut note = None;
    match std::fs::read_dir(&dir) {
        Ok(read) => {
            for (n, entry) in read.enumerate().take(MAX_ENTRIES + 1) {
                if stale() {
                    break;
                }
                if n == MAX_ENTRIES {
                    note = Some(format!(
                        "Only the first {MAX_ENTRIES} directory entries were scanned; filtering searches this partial list. Type a full path to reach an omitted entry"
                    ));
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        note = Some(format!("Partial listing: cannot read an entry: {error}"));
                        continue;
                    }
                };
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                // Metadata follows links; leave parent components intact for OS path resolution.
                let meta = match std::fs::metadata(entry.path()) {
                    Ok(meta) => meta,
                    Err(error) => {
                        note = Some(format!("Partial listing: cannot inspect an entry: {error}"));
                        continue;
                    }
                };
                let dir = meta.is_dir();
                if kind == PathKind::Directory && !dir {
                    continue;
                }
                entries.push(Entry {
                    name,
                    dir,
                    executable: !dir && crate::platform::fs::is_executable(&entry.path()),
                });
            }
        }
        Err(error) => note = Some(format!("Cannot read this folder: {error}")),
    }
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Reply::Listing {
        dir,
        entries,
        note,
        select,
    }
}

#[cfg(test)]
mod tests;
