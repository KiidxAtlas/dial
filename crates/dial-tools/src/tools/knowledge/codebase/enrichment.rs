//! Best-effort automatic post-edit hints. One worker belongs to the cache, not to a turn.
//! Admission, polling and task drop never wait for source IO or for the worker to finish.
use super::*;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

/// Freshly checked lexical hints, not verification evidence or a resolved dependency graph.
#[derive(Debug, Default)]
pub struct Hints {
    pub text: String,
    pub sources: Vec<(String, String)>,
}

pub struct Task {
    cancel: Arc<AtomicBool>,
    result: Receiver<Hints>,
}

impl Task {
    pub fn try_take(&self) -> Result<Hints, mpsc::TryRecvError> {
        self.result.try_recv()
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

struct Request {
    paths: Vec<String>,
    cancel: Arc<AtomicBool>,
    result: SyncSender<Hints>,
}

pub(super) struct Worker {
    requests: Option<SyncSender<Request>>,
    busy: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(
        work: impl Fn(Vec<String>, &dyn Fn() -> bool) -> Result<Hints> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Request>(1);
        let busy = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let working = busy.clone();
        let stop = shutdown.clone();
        let thread = thread::Builder::new()
            .name("dial-impact-hints".into())
            .spawn(move || {
                while let Ok(request) = rx.recv() {
                    let cancelled =
                        || stop.load(Ordering::Acquire) || request.cancel.load(Ordering::Acquire);
                    if !cancelled()
                        && let Ok(hints) = work(request.paths, &cancelled)
                        && !cancelled()
                    {
                        let _ = request.result.try_send(hints);
                    }
                    working.store(false, Ordering::Release);
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: Some(tx),
            busy,
            shutdown,
            thread: Some(thread),
        })
    }

    fn submit(&self, paths: Vec<String>) -> Option<Task> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let request = Request {
            paths,
            cancel: cancel.clone(),
            result: tx,
        };
        if self.requests.as_ref()?.try_send(request).is_err() {
            self.busy.store(false, Ordering::Release);
            return None;
        }
        Some(Task { cancel, result: rx })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.requests.take();
        // Only cache lifecycle teardown joins. Turns own Task, whose drop merely cancels.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Codebase {
    /// No filesystem reads, database queries or waiting on cache locks on the caller's path.
    /// One admitted task per cache; busy caches simply omit this optional enrichment.
    pub fn try_impact_hints(&self, paths: Vec<String>) -> Option<Task> {
        if paths.is_empty() || paths.len() > 4 {
            return None;
        }
        let mut worker = self.enrichment.try_lock()?;
        if worker.is_none() {
            let conn = self.conn.try_lock()?;
            let path = PathBuf::from(conn.path()?);
            let root = self.root.clone();
            // Opening a separate read-only connection happens in the worker, never here. The
            // worker owns no Arc<Codebase>, avoiding a lifecycle cycle and mandatory-gate locks.
            *worker = Some(
                Worker::start(move |paths, cancelled| {
                    ensure!(!cancelled(), "impact hints cancelled");
                    let conn = Connection::open_with_flags(
                        &path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )?;
                    conn.busy_timeout(Duration::ZERO)?;
                    let cache = Codebase {
                        root: root.clone(),
                        conn: Mutex::new(conn),
                        enrichment: Mutex::new(None),
                    };
                    cache.related_source_hints(&paths, 3000, cancelled)
                })
                .ok()?,
            );
        }
        worker.as_ref()?.submit(paths)
    }
}

#[cfg(test)]
mod tests;
