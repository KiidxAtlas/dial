//! Background flushing for checkpoint records written with
//! [`crate::platform::atomic::write_checkpoint_deferred`].
//!
//! Records are published before the agent's write proceeds, so a process crash never loses one;
//! only their `fsync` runs here. [`Flusher::drain`] waits for every queued flush and reports the
//! first failure since the previous drain. Dropping the flusher waits for queued work.
use crate::platform::atomic::DeferredSync;
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

enum Job {
    Sync(DeferredSync),
    Barrier(mpsc::Sender<()>),
}

type Failure = Arc<Mutex<Option<String>>>;

#[derive(Default)]
pub(super) struct Flusher {
    worker: Mutex<Option<(mpsc::Sender<Job>, JoinHandle<()>)>>,
    failure: Failure,
}

fn record(failure: &Failure, pending: DeferredSync) {
    let path = pending.path().display().to_string();
    if let Err(error) = pending.finish() {
        let mut slot = failure.lock().unwrap_or_else(|p| p.into_inner());
        slot.get_or_insert_with(|| format!("syncing checkpoint record {path}: {error}"));
    }
}

impl Flusher {
    /// Queue `pending`; without a worker thread it is flushed inline.
    pub(super) fn submit(&self, pending: DeferredSync) {
        let mut worker = self.worker.lock().unwrap_or_else(|p| p.into_inner());
        if worker.is_none() {
            let (sender, receiver) = mpsc::channel::<Job>();
            let failure = Arc::clone(&self.failure);
            let spawned = std::thread::Builder::new()
                .name("dial-checkpoint-sync".into())
                .spawn(move || {
                    for job in receiver {
                        match job {
                            Job::Sync(pending) => record(&failure, pending),
                            Job::Barrier(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                });
            match spawned {
                Ok(handle) => *worker = Some((sender, handle)),
                Err(_) => return record(&self.failure, pending),
            }
        }
        let Some((sender, _)) = worker.as_ref() else {
            return record(&self.failure, pending);
        };
        if let Err(mpsc::SendError(Job::Sync(pending))) = sender.send(Job::Sync(pending)) {
            record(&self.failure, pending);
        }
    }

    /// Wait for every queued flush; report the first failure since the last drain.
    pub(super) fn drain(&self) -> Result<()> {
        let barrier = {
            let worker = self.worker.lock().unwrap_or_else(|p| p.into_inner());
            worker.as_ref().and_then(|(sender, _)| {
                let (done, wait) = mpsc::channel();
                sender.send(Job::Barrier(done)).ok().map(|()| wait)
            })
        };
        if let Some(wait) = barrier {
            let _ = wait.recv();
        }
        match self
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            Some(failure) => bail!("{failure}"),
            None => Ok(()),
        }
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((sender, handle)) = worker {
            drop(sender);
            let _ = handle.join();
        }
    }
}
