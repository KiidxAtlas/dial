//! Cooperative local-compute handoff. Requests are live file locks, not stale PID markers.
use crate::config::types::Config;
use crate::tools::background::procinfo::ProcId;
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
    crate::storage::settings_store::write_private(
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
mod tests {
    use super::*;
    #[test]
    fn background_generation_is_retried_after_foreground_handoff_without_scoring_a_partial_reply() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let config = Config {
            user_dir: root.path().join("user"),
            state_dir: root.path().join("state"),
            ..Config::default()
        };
        let _lease = Background::acquire(&config).unwrap();
        let mut backend = crate::llm::provider::Backend::new(
            crate::llm::provider::ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:1".into(),
                model: "fixture".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        );
        let mut calls = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                let request = Request::foreground(&config.user_dir).unwrap();
                let lock =
                    super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
                lock.lock_exclusive().unwrap();
                drop(request);
                std::thread::sleep(Duration::from_millis(100));
                FileExt::unlock(&lock).unwrap();
            });
            let reply = background_call(&config, &mut backend, |_, cancel| {
                calls += 1;
                if calls == 1 {
                    let start = std::time::Instant::now();
                    while !cancel.load(Ordering::SeqCst) {
                        assert!(start.elapsed() < Duration::from_secs(2));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return Err(crate::llm::Interrupted.into());
                }
                Ok("complete retried reply")
            })
            .unwrap();
            assert_eq!(reply, "complete retried reply");
        });
        assert_eq!(calls, 2);
        assert!(can_yield());
    }
    #[test]
    fn requests_are_live_and_crashed_markers_do_not_block() {
        let home = tempfile::tempdir().unwrap();
        let request = Request::foreground(home.path()).unwrap();
        assert!(foreground_pending(home.path()).unwrap());
        drop(request);
        assert!(!foreground_pending(home.path()).unwrap());
        std::fs::write(home.path().join("foreground-compute/stale"), "").unwrap();
        assert!(!foreground_pending(home.path()).unwrap());
    }

    #[test]
    fn stop_interrupts_a_paused_training_wait_and_releases_its_lease() {
        let root = tempfile::tempdir().unwrap();
        let config = Config {
            user_dir: root.path().join("user"),
            state_dir: root.path().join("state"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let lease = Background::acquire(&config).unwrap();
        let _foreground = Request::foreground(&config.user_dir).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
            });
            assert!(
                Background::yield_to_coding(&config)
                    .unwrap_err()
                    .to_string()
                    .contains("stopped")
            );
        });
        assert!(!can_yield());
        drop(lease);
        let lock = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
        lock.try_lock_exclusive().unwrap();
    }
}

#[cfg(test)]
mod controlled_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[test]
    fn cancellation_interrupts_background_model_calls_without_retry() {
        let root = tempfile::tempdir().unwrap();
        let config = Config {
            user_dir: root.path().join("user"),
            state_dir: root.path().join("state"),
            ..Default::default()
        };
        let lease = Background::acquire(&config).unwrap();
        let mut backend = crate::llm::provider::Backend::new(
            crate::llm::provider::ProviderKind::Local {
                model_dir: "unused".into(),
            },
            std::path::PathBuf::new(),
            5,
        );
        let external = Arc::new(AtomicBool::new(false));
        let mut calls = 0;
        std::thread::scope(|scope| {
            let flag = external.clone();
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(15));
                flag.store(true, Ordering::SeqCst);
            });
            let result: Result<()> =
                background_call_controlled(&config, &mut backend, Some(&external), |_, cancel| {
                    calls += 1;
                    let start = std::time::Instant::now();
                    while !cancel.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2)
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    ensure!(cancel.load(Ordering::SeqCst), "cancel was not forwarded");
                    Err(crate::llm::Interrupted.into())
                });
            assert!(
                result
                    .unwrap_err()
                    .downcast_ref::<crate::llm::Interrupted>()
                    .is_some()
            );
        });
        assert_eq!(calls, 1);
        drop(lease);
        assert!(!can_yield());
    }
}

#[cfg(test)]
mod unlocked_tests {
    use super::*;

    fn config(root: &Path) -> Config {
        Config {
            compute_locks: false,
            user_dir: root.join("user"),
            state_dir: root.join("state"),
            ..Config::default()
        }
    }

    #[test]
    fn disabled_background_ignores_shared_lock_and_requests_but_accounts_usage() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let lock = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
        lock.try_lock_exclusive().unwrap();
        let _foreground = Request::foreground(&config.user_dir).unwrap();
        // Even an owner record belonging to this process must survive the unlocked guard.
        register(
            &config.user_dir,
            "foreground",
            root.path(),
            root.path(),
            "fixture",
        )
        .unwrap();
        let owner = std::fs::read(config.user_dir.join("compute-owner.json")).unwrap();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (done, rx) = std::sync::mpsc::channel();
            let cancelled = &cancel;
            scope.spawn(move || {
                if rx.recv_timeout(Duration::from_secs(1)).is_err() {
                    cancelled.store(true, Ordering::SeqCst);
                }
            });
            let guard = Background::acquire_controlled(&config, Some(&cancel)).unwrap();
            done.send(()).unwrap();
            assert!(!can_yield());
            assert!(background_active());
            assert!(!config.user_dir.join("background-compute").exists());
            LEASED_SINCE.with(|since| {
                *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(3));
            });
            Background::yield_controlled(&config, Some(&cancel)).unwrap();
            assert_eq!(super::super::resources::used_today(&config.user_dir), 3);
            LEASED_SINCE.with(|since| {
                *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(2));
            });
            drop(guard);
            assert_eq!(super::super::resources::used_today(&config.user_dir), 5);
        });
        assert!(!background_active());
        assert!(!config.user_dir.join("background-compute").exists());
        assert_eq!(
            std::fs::read(config.user_dir.join("compute-owner.json")).unwrap(),
            owner
        );
        let probe = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
        assert_eq!(
            probe.try_lock_exclusive().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn disabled_background_creates_no_lock_request_or_owner() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let guard = Background::acquire(&config).unwrap();
        Background::yield_to_coding(&config).unwrap();
        drop(guard);
        for path in ["compute.lock", "background-compute", "compute-owner.json"] {
            assert!(!config.user_dir.join(path).exists(), "created {path}");
        }
    }

    #[test]
    fn disabled_background_obeys_pause_cancellation_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        std::fs::create_dir_all(&config.user_dir).unwrap();
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let pause = config.user_dir.join("background.pause");
        std::fs::write(&pause, "").unwrap();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::SeqCst);
            });
            assert!(Background::acquire_controlled(&config, Some(&cancel)).is_err());
        });
        assert!(!background_active());
        std::fs::remove_file(&pause).unwrap();
        let guard = Background::acquire(&config).unwrap();
        LEASED_SINCE.with(|since| {
            *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(4));
        });
        std::fs::write(&pause, "").unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
            });
            assert!(
                Background::yield_to_coding(&config)
                    .unwrap_err()
                    .to_string()
                    .contains("stopped")
            );
        });
        assert!(!background_active());
        assert_eq!(super::super::resources::used_today(&config.user_dir), 4);
        drop(guard);
        assert_eq!(super::super::resources::used_today(&config.user_dir), 4);
        assert!(Background::acquire(&config).is_err());
    }

    #[test]
    fn disabled_model_call_ignores_foreground_but_still_stops() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let _guard = Background::acquire(&config).unwrap();
        let _foreground = Request::foreground(&config.user_dir).unwrap();
        let mut backend = crate::llm::provider::Backend::new(
            crate::llm::provider::ProviderKind::Local {
                model_dir: "unused".into(),
            },
            std::path::PathBuf::new(),
            5,
        );
        let mut calls = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
            });
            let result: Result<()> = background_call(&config, &mut backend, |_, cancel| {
                calls += 1;
                let start = std::time::Instant::now();
                while !cancel.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                ensure!(cancel.load(Ordering::SeqCst), "stop was not forwarded");
                Err(crate::llm::Interrupted.into())
            });
            assert!(result.unwrap_err().to_string().contains("stopped"));
        });
        assert_eq!(calls, 1);
    }
}
