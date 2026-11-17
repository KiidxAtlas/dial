//! Cooperative local-compute handoff. Requests are live file locks, not stale PID markers.
use crate::config::types::Config;
use crate::platform::procinfo::ProcId;
use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::RefCell, fs::File, path::Path, time::Duration};

thread_local! {
    static BACKGROUND: RefCell<Option<File>> = const { RefCell::new(None) };
    /// When the lease was last (re)acquired; time spent yielded to coding is not counted.
    static LEASED_SINCE: RefCell<Option<std::time::Instant>> = const { RefCell::new(None) };
    static LEASE_DAY_CLOCK: RefCell<Option<std::sync::Arc<std::sync::Mutex<super::resources::UsageClock>>>> = const { RefCell::new(None) };
    static REQUEST_CANCEL: RefCell<Option<std::sync::Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Bind cancellation to this maintenance request, restoring any enclosing scope on exit.
/// Existing gate, worker and compute checkpoints observe the same flag.
pub fn with_cancellation<T>(
    cancel: std::sync::Arc<AtomicBool>,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    struct Restore(Option<std::sync::Arc<AtomicBool>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUEST_CANCEL.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(REQUEST_CANCEL.with(|slot| slot.replace(Some(cancel))));
    check_cancelled()?;
    work()
}

pub(super) fn check_cancelled() -> Result<()> {
    if REQUEST_CANCEL.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }) {
        return Err(crate::llm::Interrupted.into());
    }
    Ok(())
}

/// Seconds the current lease has been held since it was acquired or last reacquired.
pub fn lease_elapsed_secs() -> u64 {
    LEASED_SINCE.with(|since| since.borrow().map_or(0, |at| at.elapsed().as_secs()))
}

pub(super) fn lease_elapsed_today_secs_at(at: u64) -> u64 {
    LEASE_DAY_CLOCK.with(|clock| {
        clock.borrow().as_ref().map_or(0, |clock| {
            clock
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sample(lease_elapsed_secs(), at)
        })
    })
}

/// Whether this thread is doing accounted background work, with or without a shared lease.
pub fn background_active() -> bool {
    LEASED_SINCE.with(|since| since.borrow().is_some())
}

// Failed settlement is retained process-wide, including after the guard's thread exits. This is
// not crash durability: a process crash can still lose unpersisted compute. Admission retries the
// debt before starting another lease; an unresolved write failure fails closed.
type UsageDebt = std::collections::HashMap<std::path::PathBuf, (u64, u64)>;
static UNSETTLED: std::sync::LazyLock<std::sync::Mutex<UsageDebt>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(UsageDebt::new()));

fn retry_usage(user: &Path, at: u64) -> Result<()> {
    let mut pending = UNSETTLED.lock().unwrap_or_else(|e| e.into_inner());
    retry_usage_locked(&mut pending, user, at)
}

fn retry_usage_locked(pending: &mut UsageDebt, user: &Path, at: u64) -> Result<()> {
    if let Some(&(day, secs)) = pending.get(user) {
        ensure!(
            day <= at / 86_400,
            "unsettled compute belongs to a later UTC day"
        );
        if day == at / 86_400 {
            super::resources::record_usage_at(user, secs, at)?;
        }
        pending.remove(user);
    }
    Ok(())
}

fn start_lease_clock() {
    LEASE_DAY_CLOCK.with(|clock| {
        *clock.borrow_mut() = Some(std::sync::Arc::new(std::sync::Mutex::new(
            super::resources::UsageClock::new(super::resources::now()),
        )))
    });
    LEASED_SINCE.with(|since| *since.borrow_mut() = Some(std::time::Instant::now()));
}

/// Settle only the current UTC day's portion; the single-day record discards expired days.
fn settle_usage(user: &Path) -> Result<()> {
    settle_usage_at(user, super::resources::now())
}

fn settle_usage_at(user: &Path, at: u64) -> Result<()> {
    let secs = lease_elapsed_today_secs_at(at);
    LEASED_SINCE.with(|since| since.borrow_mut().take());
    let mut pending = UNSETTLED.lock().unwrap_or_else(|e| e.into_inner());
    if secs > 0 {
        let debt = pending
            .entry(user.to_path_buf())
            .or_insert((at / 86_400, 0));
        if debt.0 < at / 86_400 {
            *debt = (at / 86_400, 0);
        }
        debt.1 = debt.1.saturating_add(secs);
    }
    retry_usage_locked(&mut pending, user, at)
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
        file.as_file().lock()?;
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
        match file.try_lock().map_err(std::io::Error::from) {
            Ok(()) => {
                std::fs::File::unlock(&file)?;
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
    let cancel = REQUEST_CANCEL.with(|slot| slot.borrow().clone());
    background_call_controlled(config, backend, cancel.as_ref(), call)
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
    let scoped = REQUEST_CANCEL.with(|slot| slot.borrow().clone());
    let request_cancel = scoped.as_ref();
    // Assistance roles may already have their own interrupt flag. Link both sources without
    // overwriting either caller's flag or losing request cancellation inside a role call.
    let separate_flags = request_cancel
        .zip(external)
        .is_some_and(|(request, role)| !Arc::ptr_eq(request, role));
    loop {
        ensure!(
            !external.is_some_and(|flag| flag.load(Ordering::SeqCst))
                && !request_cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)),
            "background call interrupted"
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let accounted = can_yield() || background_active();
        if !accounted && !separate_flags {
            return call(
                backend,
                external.or(request_cancel).cloned().unwrap_or(cancel),
            );
        }
        let preempt = Arc::new(AtomicBool::new(false));
        let leased_since = LEASED_SINCE.with(|since| *since.borrow());
        let day_clock = LEASE_DAY_CLOCK.with(|clock| clock.borrow().clone());
        let result = std::thread::scope(|scope| {
            let (done, rx) = mpsc::channel::<()>();
            let flag = cancel.clone();
            let yielded = preempt.clone();
            let watcher = scope.spawn(move || -> Result<()> {
                loop {
                    if external.is_some_and(|flag| flag.load(Ordering::SeqCst))
                        || request_cancel.is_some_and(|flag| flag.load(Ordering::SeqCst))
                    {
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    if let (Some(since), Some(clock)) = (leased_since, &day_clock) {
                        let at = super::resources::now();
                        let elapsed = clock
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .sample(since.elapsed().as_secs(), at);
                        if let Err(error) =
                            super::auto::check_compute_budget_at(config, elapsed, at)
                        {
                            flag.store(true, Ordering::SeqCst);
                            return Err(error);
                        }
                    }
                    if accounted
                        && (paused(&config.user_dir)
                            || (config.compute_locks
                                && foreground_pending(&config.user_dir).unwrap_or(false))
                            || config.state_dir.join("improvement.stop").exists())
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
                Ok(())
            });
            let result = call(backend, cancel);
            drop(done);
            if let Err(error) = watcher.join().expect("background compute watcher panicked") {
                backend.release();
                return Err(error);
            }
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
    let file = super::auto::lock_file(&crate::config::paths::compute_lock_file(&config.user_dir))?;
    loop {
        ensure!(
            !cancel.is_some_and(|c| c.load(Ordering::SeqCst)),
            "steering job interrupted"
        );
        super::auto::check_stop(config)?;
        if !should_yield(&config.user_dir)? {
            match file.try_lock().map_err(std::io::Error::from) {
                Ok(()) => {
                    // Recheck after acquisition so queued foreground requests win the race.
                    if !should_yield(&config.user_dir)? {
                        return Ok(file);
                    }
                    std::fs::File::unlock(&file)?;
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
        retry_usage(&config.user_dir, super::resources::now())?;
        super::auto::check_compute_budget_at(config, 0, super::resources::now())?;
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
        start_lease_clock();
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
            start_lease_clock();
            return Ok(());
        }
        let _request = Request::new(&config.user_dir, "background-compute")?;
        let file = BACKGROUND.with(|slot| slot.borrow_mut().take());
        let file =
            file.ok_or_else(|| anyhow::anyhow!("training has no background compute lease"))?;
        clear_owner(&config.user_dir);
        settle_usage(&config.user_dir)?;
        std::fs::File::unlock(&file)?;
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
        start_lease_clock();
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
            let _ = std::fs::File::unlock(&file);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod controlled_tests;

#[cfg(test)]
mod unlocked_tests;
