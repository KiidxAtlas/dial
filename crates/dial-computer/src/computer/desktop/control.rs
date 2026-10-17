//! Cancellation and exclusive task/operation ownership of desktop mutations.
//!
//! Explicit control keeps the kernel lease between operations. Revocation
//! stops fresh work immediately, but an in-flight operation retains ownership
//! until its held input and focus have been restored.
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};

use super::error::{CoreResult, DesktopError};

#[cfg(dial_desktop_linux)]
mod linux;
#[cfg(dial_desktop_macos)]
mod macos;
#[cfg(dial_desktop_windows)]
mod windows;
#[cfg(dial_desktop_macos)]
pub(crate) use macos::SYNTHETIC_EVENT_TAG;

#[derive(Default)]
struct CancellationState {
    generation: AtomicU64,
    state: Mutex<Option<Arc<ControlLease>>>,
    wake: Condvar,
    #[cfg(dial_desktop_linux)]
    async_wake: tokio::sync::Notify,
}

#[derive(Clone, Default)]
pub(crate) struct CancellationSource(Arc<CancellationState>);

impl CancellationSource {
    pub(crate) fn token(&self) -> OperationToken {
        OperationToken {
            source: self.clone(),
            generation: self.0.generation.load(Ordering::Acquire),
        }
    }

    /// Ends a successful run without surrendering an explicitly granted task
    /// lease.
    pub(crate) fn retire(&self) {
        let _state = self.0.state.lock();
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        self.0.wake.notify_all();
        #[cfg(dial_desktop_linux)]
        self.0.async_wake.notify_waiters();
    }

    /// Aborts current/queued work and revokes any explicit task ownership.
    pub(crate) fn cancel(&self) {
        let lease = {
            let mut state = self.0.state.lock();
            self.0.generation.fetch_add(1, Ordering::AcqRel);
            self.0.wake.notify_all();
            #[cfg(dial_desktop_linux)]
            self.0.async_wake.notify_waiters();
            state.take()
        };
        drop(lease);
    }

    pub(crate) fn acquire_control(&self, token: &OperationToken) -> CoreResult<()> {
        let mut state = self.0.state.lock();
        token.check()?;
        if !Arc::ptr_eq(&self.0, &token.source.0) {
            return Err(busy());
        }
        if state.is_none() {
            *state = Some(Arc::new(ControlLease::acquire(self)?));
        }
        Ok(())
    }

    pub(crate) fn release_control(&self) {
        // A release is also a generation boundary: queued work authorized by the
        // relinquished grant must never execute under a later acquisition.
        self.cancel();
    }

    pub(crate) fn control_active(&self) -> bool {
        self.0.state.lock().is_some()
    }
}

/// Weak callback ownership avoids a lease -> monitor -> source -> lease cycle.
#[derive(Clone)]
pub(super) struct EmergencyStop(Weak<CancellationState>);

impl EmergencyStop {
    fn cancel(&self) {
        if let Some(source) = self.0.upgrade() {
            CancellationSource(source).cancel();
        }
    }
}

#[derive(Clone)]
pub(crate) struct OperationToken {
    source: CancellationSource,
    generation: u64,
}

impl OperationToken {
    pub(crate) fn control_active(&self) -> bool {
        self.source.control_active()
    }

    pub(crate) fn enter(&self) -> OperationScope {
        let previous = CURRENT.with_borrow_mut(|current| current.replace(self.clone()));
        OperationScope {
            previous,
            _thread: PhantomData,
        }
    }

    pub(crate) fn check(&self) -> CoreResult<()> {
        if self.source.0.generation.load(Ordering::Acquire) == self.generation {
            Ok(())
        } else {
            Err(DesktopError::cancelled(
                "desktop operation cancelled; input may be partial; inspect before retrying",
            ))
        }
    }

    #[cfg(dial_desktop_linux)]
    pub(crate) async fn cancelled(&self) -> DesktopError {
        loop {
            let notified = self.source.0.async_wake.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if let Err(error) = self.check() {
                return error;
            }
            notified.await;
        }
    }

    pub(crate) fn wait(&self, duration: Duration) -> CoreResult<()> {
        let deadline = Instant::now() + duration;
        let mut state = self.source.0.state.lock();
        loop {
            self.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            self.source.0.wake.wait_for(&mut state, remaining);
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<OperationToken>> = const { RefCell::new(None) };
    static CLEANUP: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct OperationScope {
    previous: Option<OperationToken>,
    _thread: PhantomData<Rc<()>>,
}

impl Drop for OperationScope {
    fn drop(&mut self) {
        CURRENT.with_borrow_mut(|current| *current = self.previous.take());
    }
}

#[cfg(dial_desktop_linux)]
pub(crate) fn is_cleaning_up() -> bool {
    CLEANUP.get()
}

#[cfg(dial_desktop_linux)]
pub(crate) fn current_token() -> Option<OperationToken> {
    if CLEANUP.get() {
        None
    } else {
        CURRENT.with_borrow(Clone::clone)
    }
}

pub(crate) fn check() -> CoreResult<()> {
    if CLEANUP.get() {
        return Ok(());
    }
    CURRENT.with_borrow(|token| token.as_ref().map_or(Ok(()), OperationToken::check))
}

pub(crate) fn wait(duration: Duration) -> CoreResult<()> {
    if CLEANUP.get() {
        thread::sleep(duration);
        return Ok(());
    }
    CURRENT.with_borrow(|token| {
        if let Some(token) = token {
            token.wait(duration)
        } else {
            thread::sleep(duration);
            Ok(())
        }
    })
}

/// Cleanup must release already-held input even after cancellation. This does
/// not clear a token or authorize any later operation. Nested cleanup is safe.
pub(crate) fn cleanup<T>(action: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLEANUP.set(self.0);
        }
    }
    let _restore = Restore(CLEANUP.replace(true));
    action()
}

/// Owns one bounded button hold. An attempted press can be partially delivered,
/// so release is mandatory even when that press reports an error.
pub(crate) fn bounded_hold(
    duration: Duration,
    mut transition: impl FnMut(bool) -> CoreResult<()>,
) -> CoreResult<()> {
    check()?;
    let result = transition(true).and_then(|()| wait(duration));
    let released = cleanup(|| transition(false));
    match (result, released) {
        (Err(mut error), Err(release)) => {
            error.message.push_str("; input release also failed: ");
            error.message.push_str(&release.message);
            Err(error)
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub(crate) fn key_modifiers(keys: &[super::keys::KeyName]) -> super::backend::Modifiers {
    use super::keys::KeyName;
    super::backend::Modifiers {
        ctrl: keys.contains(&KeyName::Ctrl),
        alt: keys.contains(&KeyName::Alt),
        shift: keys.contains(&KeyName::Shift),
        meta: keys.contains(&KeyName::Meta),
    }
}

/// Merge modifier options into the already-owned arbitrary-key gesture list.
#[cfg(any(dial_desktop_macos, dial_desktop_windows))]
pub(crate) fn add_modifiers(
    keys: &mut Vec<super::keys::KeyName>,
    modifiers: super::backend::Modifiers,
) {
    use super::keys::KeyName;
    for (enabled, key) in [
        (modifiers.ctrl, KeyName::Ctrl),
        (modifiers.alt, KeyName::Alt),
        (modifiers.shift, KeyName::Shift),
        (modifiers.meta, KeyName::Meta),
    ] {
        if enabled && !keys.contains(&key) {
            keys.push(key);
        }
    }
}

#[cfg(test)]
pub(crate) fn with_token_for_test<T>(token: &OperationToken, action: impl FnOnce() -> T) -> T {
    let _scope = token.enter();
    action()
}

#[cfg(dial_desktop_macos)]
static USER_ACTIVITY: AtomicU64 = AtomicU64::new(0);

#[cfg(dial_desktop_macos)]
pub(crate) fn user_activity() -> u64 {
    USER_ACTIVITY.load(Ordering::Acquire)
}

static OWNED: AtomicBool = AtomicBool::new(false);

struct ProcessLease;
impl ProcessLease {
    fn acquire() -> CoreResult<Self> {
        OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| busy())?;
        Ok(Self)
    }
}
impl Drop for ProcessLease {
    fn drop(&mut self) {
        OWNED.store(false, Ordering::Release);
    }
}

fn busy() -> DesktopError {
    DesktopError::input_busy(
        "another desktop operation owns input/focus control; no input was sent",
    )
}

/// The kernel owner lives on its own thread: Windows mutex release must happen
/// on the same thread that acquired it, even when the session is disposed from
/// another host thread. Closing/crashing the process releases the OS resource.
struct ControlLease {
    #[cfg(dial_desktop_macos)]
    _escape: Option<macos::EscapeMonitor>,
    #[cfg(dial_desktop_windows)]
    _escape: Option<windows::EscapeMonitor>,
    #[cfg(dial_desktop_linux)]
    _escape: Option<linux::EscapeMonitor>,
    _kernel: KernelOwner,
    running: AtomicBool,
}

impl ControlLease {
    fn acquire(_source: &CancellationSource) -> CoreResult<Self> {
        let kernel = KernelOwner::acquire()?;
        #[cfg(dial_desktop_macos)]
        let escape = macos::EscapeMonitor::start(EmergencyStop(Arc::downgrade(&_source.0)))?;
        #[cfg(dial_desktop_windows)]
        let escape = windows::EscapeMonitor::start(EmergencyStop(Arc::downgrade(&_source.0)))?;
        #[cfg(dial_desktop_linux)]
        let escape = linux::EscapeMonitor::start(EmergencyStop(Arc::downgrade(&_source.0)))?;
        Ok(Self {
            #[cfg(any(dial_desktop_macos, dial_desktop_windows))]
            _escape: Some(escape),
            #[cfg(dial_desktop_linux)]
            _escape: escape,
            _kernel: kernel,
            running: AtomicBool::new(false),
        })
    }
}

struct KernelOwner {
    stop: Option<flume::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl KernelOwner {
    fn acquire() -> CoreResult<Self> {
        let (ready, receive) = flume::bounded(1);
        let (stop, stopped) = flume::bounded(1);
        let thread = thread::Builder::new()
            .name("desktop-owner".into())
            .spawn(move || {
                let leases = ProcessLease::acquire()
                    .and_then(|process| KernelLease::acquire().map(|kernel| (kernel, process)));
                match leases {
                    Ok(_leases) => {
                        let _ = ready.send(Ok(()));
                        let _ = stopped.recv();
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })
            .map_err(|error| {
                DesktopError::input_failed(format!("cannot start desktop owner: {error}"))
            })?;
        let owner = Self {
            stop: Some(stop),
            thread: Some(thread),
        };
        receive.recv().map_err(|_| {
            DesktopError::input_failed("desktop owner stopped before acquiring input")
        })??;
        Ok(owner)
    }
}

impl Drop for KernelOwner {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct InputLease {
    _scope: OperationScope,
    owner: Arc<ControlLease>,
    _thread: PhantomData<Rc<()>>,
}

impl InputLease {
    pub(crate) fn acquire(token: &OperationToken) -> CoreResult<Self> {
        token.check()?;
        let owner = {
            let state = token.source.0.state.lock();
            token.check()?;
            match state.as_ref() {
                Some(owner) => owner.clone(),
                None => Arc::new(ControlLease::acquire(&token.source)?),
            }
        };
        owner
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| busy())?;
        let lease = Self {
            _scope: token.enter(),
            owner,
            _thread: PhantomData,
        };
        token.check()?;
        Ok(lease)
    }
}

impl Drop for InputLease {
    fn drop(&mut self) {
        self.owner.running.store(false, Ordering::Release);
    }
}

#[cfg(unix)]
struct KernelLease(std::fs::File);

#[cfg(unix)]
impl KernelLease {
    fn acquire() -> CoreResult<Self> {
        Self::at(&Self::path())
    }

    /// A fixed per-login-user inode coordinates independently launched hosts.
    /// Never unlink it: unlinking a locked inode would create two lock domains.
    #[cfg(not(test))]
    fn path() -> std::path::PathBuf {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        std::path::PathBuf::from(format!("/tmp/dial-desktop-input-{uid}.lock"))
    }

    /// nextest runs every test in its own process; a per-process inode keeps
    /// concurrent test processes and a live host from contending for input.
    #[cfg(test)]
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dial-desktop-input-test-{}.lock",
            std::process::id()
        ))
    }

    fn at(path: &std::path::Path) -> CoreResult<Self> {
        use std::os::unix::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| {
                DesktopError::input_failed(format!("cannot acquire desktop ownership: {error}"))
            })?;
        let metadata = file.metadata().map_err(|error| {
            DesktopError::input_failed(format!("cannot inspect desktop ownership: {error}"))
        })?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err(DesktopError::input_failed(
                "desktop ownership file is not a private regular file",
            ));
        }
        // SAFETY: the descriptor remains owned by `file`; flock is nonblocking.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(busy());
            }
            return Err(DesktopError::input_failed(format!(
                "cannot lock desktop ownership: {error}"
            )));
        }
        Ok(Self(file))
    }
}

#[cfg(unix)]
impl Drop for KernelLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: this guard owns the live locked descriptor.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(windows)]
struct KernelLease(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl KernelLease {
    fn acquire() -> CoreResult<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{CreateMutexW, WaitForSingleObject},
        };
        let name = Self::name();
        // SAFETY: nul-terminated name and default security descriptor are valid.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(DesktopError::input_failed(
                "cannot open desktop ownership mutex",
            ));
        }
        // SAFETY: handle is live; zero timeout never queues or retries input.
        let status = unsafe { WaitForSingleObject(handle, 0) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            return Ok(Self(handle));
        }
        // SAFETY: the failed acquisition still owns its opened handle.
        unsafe {
            CloseHandle(handle);
        }
        if status == WAIT_TIMEOUT {
            return Err(busy());
        }
        Err(DesktopError::input_failed(
            "cannot acquire desktop ownership mutex",
        ))
    }

    /// Nul-terminated mutex name shared by every host in the login session.
    #[cfg(not(test))]
    fn name() -> Vec<u16> {
        "Local\\PiDesktopInput-v1\0".encode_utf16().collect()
    }

    /// nextest runs every test in its own process; a per-process mutex keeps
    /// concurrent test processes and a live host from contending for input.
    #[cfg(test)]
    fn name() -> Vec<u16> {
        format!("Local\\PiDesktopInput-test-{}\0", std::process::id())
            .encode_utf16()
            .collect()
    }
}

#[cfg(windows)]
impl Drop for KernelLease {
    fn drop(&mut self) {
        // SAFETY: this thread owns both the mutex and its handle.
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests;
