//! Cooperative local-compute handoff. Requests are live file locks, not stale PID markers.
use crate::config::types::Config;
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

/// Add the time of the current lease stretch to today's budget and restart the stretch.
fn settle_usage(user: &Path) {
    let secs = LEASED_SINCE.with(|since| {
        since
            .borrow_mut()
            .take()
            .map_or(0, |at| at.elapsed().as_secs())
    });
    let _ = super::resources::record_usage(user, secs);
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
            "pid":std::process::id(),"kind":kind,"project":project,"state":state,"model":model,
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
        if !can_yield() {
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
                    if should_yield(&config.user_dir).unwrap_or(false)
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

pub struct Background {
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    user: std::path::PathBuf,
}
impl Background {
    pub fn acquire(config: &Config) -> Result<Self> {
        Self::acquire_controlled(config, None)
    }
    pub fn acquire_controlled(config: &Config, cancel: Option<&AtomicBool>) -> Result<Self> {
        ensure!(!can_yield(), "background compute lease is already held");
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
        LEASED_SINCE.with(|since| *since.borrow_mut() = Some(std::time::Instant::now()));
        Ok(Self {
            _thread: std::marker::PhantomData,
            user: config.user_dir.clone(),
        })
    }
    /// Called only after the training subprocess exited with a committed checkpoint.
    pub fn yield_to_coding(config: &Config) -> Result<()> {
        Self::yield_controlled(config, None)
    }
    pub fn yield_controlled(config: &Config, cancel: Option<&AtomicBool>) -> Result<()> {
        let _request = Request::new(&config.user_dir, "background-compute")?;
        let file = BACKGROUND.with(|slot| slot.borrow_mut().take());
        let file =
            file.ok_or_else(|| anyhow::anyhow!("training has no background compute lease"))?;
        clear_owner(&config.user_dir);
        settle_usage(&config.user_dir);
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
        if let Some(file) = BACKGROUND.with(|slot| slot.borrow_mut().take()) {
            clear_owner(&self.user);
            settle_usage(&self.user);
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
        let mut config = Config::default();
        config.user_dir = root.path().join("user");
        config.state_dir = root.path().join("state");
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
        let mut config = Config::default();
        config.user_dir = root.path().join("user");
        config.state_dir = root.path().join("state");
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
