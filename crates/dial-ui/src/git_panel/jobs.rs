//! Bounded Git work and latest outcomes shared across panel lifetimes.
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        mpsc::{self, Receiver},
    },
};

const MAX_JOBS: usize = 8;
const MAX_OUTCOMES: usize = 32;
const MAX_OUTCOME_BYTES: usize = 128 * 1024;
const MAX_MESSAGE_BYTES: usize = 4096;
static JOBS: LazyLock<Mutex<Jobs>> = LazyLock::new(|| Mutex::new(Jobs::default()));

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobNotice {
    pub(crate) id: u64,
    pub(crate) running: bool,
    pub(crate) error: bool,
    pub(crate) message: String,
}

#[derive(Default)]
struct Jobs {
    next_id: u64,
    active: HashMap<PathBuf, JobNotice>,
    completed: VecDeque<(PathBuf, JobNotice)>,
    completed_bytes: usize,
}

fn record_bytes(root: &Path, notice: &JobNotice) -> usize {
    root.as_os_str().as_encoded_bytes().len() + notice.message.len()
}

fn bounded_message(message: String) -> String {
    if message.len() <= MAX_MESSAGE_BYTES {
        // Do not retain a large spare allocation from the operation's output buffer.
        return message.into_boxed_str().into_string();
    }
    let mut end = MAX_MESSAGE_BYTES - "…".len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(MAX_MESSAGE_BYTES);
    bounded.push_str(&message[..end]);
    bounded.push('…');
    bounded
}

impl Jobs {
    fn notice(&self, root: &Path) -> Option<JobNotice> {
        self.active
            .get(root)
            .or_else(|| {
                self.completed
                    .iter()
                    .find(|(path, _)| path == root)
                    .map(|(_, notice)| notice)
            })
            .cloned()
    }

    fn finish(&mut self, root: PathBuf, id: u64, message: String, error: bool) {
        if self.active.get(&root).is_some_and(|notice| notice.id == id) {
            self.active.remove(&root);
        }
        if let Some(index) = self.completed.iter().position(|(path, _)| path == &root) {
            let (path, previous) = self.completed.remove(index).expect("known outcome");
            self.completed_bytes -= record_bytes(&path, &previous);
        }
        let notice = JobNotice {
            id,
            running: false,
            error,
            message: bounded_message(message),
        };
        let bytes = record_bytes(&root, &notice);
        if bytes > MAX_OUTCOME_BYTES {
            return;
        }
        while self.completed.len() >= MAX_OUTCOMES
            || self.completed_bytes + bytes > MAX_OUTCOME_BYTES
        {
            let Some((path, previous)) = self.completed.pop_front() else {
                break;
            };
            self.completed_bytes -= record_bytes(&path, &previous);
        }
        self.completed_bytes += bytes;
        self.completed.push_back((root, notice));
    }
}

struct Permit {
    root: PathBuf,
    id: u64,
    done: bool,
}
impl Permit {
    fn complete(mut self, message: String, error: bool) {
        JOBS.lock()
            .unwrap_or_else(|error| error.into_inner())
            .finish(self.root.clone(), self.id, message, error);
        self.done = true;
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.done {
            JOBS.lock()
                .unwrap_or_else(|error| error.into_inner())
                .finish(
                    self.root.clone(),
                    self.id,
                    "Git operation stopped unexpectedly; inspect repository state".to_owned(),
                    true,
                );
        }
    }
}

impl super::Repo {
    /// Loaded panels use Git's resolved top-level path as the repository identity. Outcomes are
    /// process-local, latest-per-repository and bounded; reading never consumes another panel's result.
    pub(crate) fn job_notice(&self) -> Option<JobNotice> {
        JOBS.lock()
            .unwrap_or_else(|error| error.into_inner())
            .notice(&self.root)
    }

    /// No waiting queue: refuse excess or same-repository work, even after an overlay closes.
    pub(crate) fn spawn_job<T: Send + 'static>(
        &self,
        label: &'static str,
        operation: impl FnOnce(Self) -> Result<T, String> + Send + 'static,
    ) -> Receiver<Result<T, String>> {
        let (tx, rx) = mpsc::channel();
        let permit = {
            let mut jobs = JOBS.lock().unwrap_or_else(|error| error.into_inner());
            if jobs.active.contains_key(&self.root) {
                let _ = tx.send(Err("Another Git operation is still running for this repository; retry when it finishes".to_owned()));
                return rx;
            }
            if jobs.active.len() >= MAX_JOBS {
                let _ = tx.send(Err(
                    "Git workers are busy; retry after an operation finishes".to_owned(),
                ));
                return rx;
            }
            jobs.next_id = jobs.next_id.wrapping_add(1);
            let id = jobs.next_id;
            jobs.active.insert(
                self.root.clone(),
                JobNotice {
                    id,
                    running: true,
                    error: false,
                    message: format!("{label} in progress…"),
                },
            );
            Permit {
                root: self.root.clone(),
                id,
                done: false,
            }
        };
        let id = permit.id;
        let repo = self.clone();
        let failure = tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("git-panel-job".to_owned())
            .spawn(move || {
                let result = operation(repo);
                let (message, error) = match &result {
                    Ok(_) => (format!("{label} completed"), false),
                    Err(error) => (format!("{label} failed: {error}"), true),
                };
                permit.complete(message, error);
                let _ = tx.send(result);
            })
        {
            let message = format!("Starting Git worker: {error}");
            JOBS.lock()
                .unwrap_or_else(|error| error.into_inner())
                .finish(self.root.clone(), id, message.clone(), true);
            let _ = failure.send(Err(message));
        }
        rx
    }
}

#[cfg(test)]
mod tests;
