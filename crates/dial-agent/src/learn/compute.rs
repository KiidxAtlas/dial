//! Cooperative local-compute handoff. Requests are live file locks, not stale PID markers.
use crate::config::types::Config;
use crate::platform::procinfo::ProcId;
use anyhow::{Result, ensure};
use fs2::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::RefCell, fs::File, path::Path, time::Duration};

thread_local! {
    static BACKGROUND: RefCell<Option<File>> = const { RefCell::new(None) };
    /// When the lease was last (re)acquired; time spent yielded to coding is not counted.
    static LEASED_SINCE: RefCell<Option<std::time::Instant>> = const { RefCell::new(None) };
}

/// Seconds the current lease has been held since it was acquired or last reacquired.
pub fn lease_elapsed_secs() -> u64 {
    LEASED_SINCE.with(|since| since.borrow().map_or(0, |at| at.elapsed().as_secs()))
}

/// Whether this thread is doing accounted background work, with or without a shared lease.
pub fn background_active() -> bool {
    LEASED_SINCE.with(|since| since.borrow().is_some())
}

/// Add the time of the current lease stretch to today's budget and restart the stretch.
fn settle_usage(user: &Path) -> Result<()> {
    let secs = LEASED_SINCE.with(|since| {
        since
            .borrow_mut()
            .take()
            .map_or(0, |at| at.elapsed().as_secs())
    });
    super::resources::record_usage(user, secs)
}

pub struct Request {
    _file: tempfile::NamedTempFile,
}
impl Request {
    fn new(user: &Path, kind: &str) -> Result<Self> {
        let directory = user.join(kind);
        std::fs::create_dir_all(&directory)?;
        let file = tempfile::Builder::new()
            .prefix("request-")
            .tempfile_in(directory)?;
        file.as_file().lock_exclusive()?;
        Ok(Self { _file: file })
    }
    pub fn foreground(user: &Path) -> Result<Self> {
        super::resources::note_foreground(user);
        Self::new(user, "foreground-compute")
    }
}

fn pending(user: &Path, kind: &str) -> Result<bool> {
    let entries = match std::fs::read_dir(user.join(kind)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        let file = match File::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&file)?;
            } // A crashed requester never blocks work.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}
pub fn foreground_pending(user: &Path) -> Result<bool> {
    pending(user, "foreground-compute")
}
pub fn background_pending(user: &Path) -> Result<bool> {
    pending(user, "background-compute")
}
pub fn can_yield() -> bool {
    BACKGROUND.with(|slot| slot.borrow().is_some())
}

pub fn paused(user: &Path) -> bool {
    user.join("background.pause").exists()
}
pub fn should_yield(user: &Path) -> Result<bool> {
    Ok(paused(user) || foreground_pending(user)?)
}
pub fn register(user: &Path, kind: &str, project: &Path, state: &Path, model: &str) -> Result<()> {
    crate::config::settings_store::write_private(
        &user.join("compute-owner.json"),
        serde_json::json!({
            "pid":std::process::id(),"proc":ProcId::current(),"kind":kind,"project":project,"state":state,"model":model,
            "since":super::improve::now(),"protocol":1
        })
        .to_string()
        .as_bytes(),
    )
}
pub fn clear_owner(user: &Path) {
    let path = user.join("compute-owner.json");
    if std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .is_some_and(|value| value["pid"] == std::process::id())
    {
        let _ = std::fs::remove_file(path);
    }
}
pub struct OwnerRecord {
    user: std::path::PathBuf,
}
impl Drop for OwnerRecord {
    fn drop(&mut self) {
        clear_owner(&self.user);
    }
}
pub fn owner_record(
    user: &Path,
    kind: &str,
    project: &Path,
    state: &Path,
    model: &str,
) -> Result<OwnerRecord> {
    register(user, kind, project, state, model)?;
    Ok(OwnerRecord { user: user.into() })
}

#[derive(Debug)]
pub struct Preempted;
impl std::fmt::Display for Preempted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("background compute yielded")
    }
}
impl std::error::Error for Preempted {}

/// Interrupt background generation without scoring the interrupted answer or executing its tools.
pub fn background_call<T>(
    config: &Config,
    backend: &mut crate::llm::provider::Backend,
    call: impl FnMut(
        &mut crate::llm::provider::Backend,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<T>,
) -> Result<T> {
    background_call_controlled(config, backend, None, call)
}

pub fn background_call_controlled<T>(
    config: &Config,
    backend: &mut crate::llm::provider::Backend,
    external: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    mut call: impl FnMut(
        &mut crate::llm::provider::Backend,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<T>,
) -> Result<T> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    loop {
        ensure!(
            !external.is_some_and(|flag| flag.load(Ordering::SeqCst)),
            "background call interrupted"
        );
        let cancel = Arc::new(AtomicBool::new(false));
        if !can_yield() && !background_active() {
            return call(backend, external.cloned().unwrap_or(cancel));
        }
        let preempt = Arc::new(AtomicBool::new(false));
        let result = std::thread::scope(|scope| {
            let (done, rx) = mpsc::channel::<()>();
            let flag = cancel.clone();
            let yielded = preempt.clone();
            scope.spawn(move || {
                loop {
                    if external.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    if (paused(&config.user_dir)
                        || (config.compute_locks
                            && foreground_pending(&config.user_dir).unwrap_or(false)))
                        || config.state_dir.join("improvement.stop").exists()
                    {
                        yielded.store(true, Ordering::SeqCst);
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    if !matches!(
                        rx.recv_timeout(Duration::from_millis(40)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            });
            let result = call(backend, cancel);
            drop(done);
            result
        });
        if !preempt.load(Ordering::SeqCst) {
            return result;
        }
        backend.release();
        super::auto::check_stop(config)?;
        Background::yield_controlled(config, external.map(|flag| flag.as_ref()))?;
    }
}

fn acquire(config: &Config, cancel: Option<&AtomicBool>) -> Result<File> {
    let file = super::auto::lock_file(&config.user_dir.join("compute.lock"))?;
    loop {
        ensure!(
            !cancel.is_some_and(|c| c.load(Ordering::SeqCst)),
            "steering job interrupted"
        );
        super::auto::check_stop(config)?;
        if !should_yield(&config.user_dir)? {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    // Recheck after acquisition so queued foreground requests win the race.
                    if !should_yield(&config.user_dir)? {
                        return Ok(file);
                    }
                    FileExt::unlock(&file)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Disabling shared arbitration does not disable operator controls.
fn wait_unlocked(config: &Config, cancel: Option<&AtomicBool>) -> Result<()> {
    loop {
        ensure!(
            !cancel.is_some_and(|c| c.load(Ordering::SeqCst)),
            "steering job interrupted"
        );
        super::auto::check_stop(config)?;
        if !paused(&config.user_dir) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

pub struct Background {
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    user: std::path::PathBuf,
    compute_locks: bool,
}
impl Background {
    pub fn acquire(config: &Config) -> Result<Self> {
        Self::acquire_controlled(config, None)
    }
    pub fn acquire_controlled(config: &Config, cancel: Option<&AtomicBool>) -> Result<Self> {
        ensure!(
            !can_yield() && !background_active(),
            "background compute lease is already held"
        );
        if config.compute_locks {
            let _request = Request::new(&config.user_dir, "background-compute")?;
            let file = acquire(config, cancel)?;
            register(
                &config.user_dir,
                "background",
                &config.project_root,
                &config.state_dir,
                &config.model,
            )?;
            BACKGROUND.with(|slot| *slot.borrow_mut() = Some(file));
        } else {
            wait_unlocked(config, cancel)?;
        }
        LEASED_SINCE.with(|since| *since.borrow_mut() = Some(std::time::Instant::now()));
        Ok(Self {
            _thread: std::marker::PhantomData,
            user: config.user_dir.clone(),
            compute_locks: config.compute_locks,
        })
    }
    /// Called only after the training subprocess exited with a committed checkpoint.
    pub fn yield_to_coding(config: &Config) -> Result<()> {
        Self::yield_controlled(config, None)
    }
    pub fn yield_controlled(config: &Config, cancel: Option<&AtomicBool>) -> Result<()> {
        if !config.compute_locks {
            settle_usage(&config.user_dir)?;
            wait_unlocked(config, cancel)?;
            LEASED_SINCE.with(|since| *since.borrow_mut() = Some(std::time::Instant::now()));
            return Ok(());
        }
        let _request = Request::new(&config.user_dir, "background-compute")?;
        let file = BACKGROUND.with(|slot| slot.borrow_mut().take());
        let file =
            file.ok_or_else(|| anyhow::anyhow!("training has no background compute lease"))?;
        clear_owner(&config.user_dir);
        settle_usage(&config.user_dir)?;
        FileExt::unlock(&file)?;
        drop(file);
        let file = acquire(config, cancel)?;
        register(
            &config.user_dir,
            "background",
            &config.project_root,
            &config.state_dir,
            &config.model,
        )?;
        BACKGROUND.with(|slot| *slot.borrow_mut() = Some(file));
        LEASED_SINCE.with(|since| *since.borrow_mut() = Some(std::time::Instant::now()));
        Ok(())
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        if !self.compute_locks {
            if let Err(error) = settle_usage(&self.user) {
                eprintln!("dial: could not record background compute time: {error:#}");
            }
        } else if let Some(file) = BACKGROUND.with(|slot| slot.borrow_mut().take()) {
            clear_owner(&self.user);
            if let Err(error) = settle_usage(&self.user) {
                eprintln!("dial: could not record background compute time: {error:#}");
            }
            let _ = FileExt::unlock(&file);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod controlled_tests;

#[cfg(test)]
mod unlocked_tests;
