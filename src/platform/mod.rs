//! Operating-system layer: the primitives whose implementation differs between Unix and Windows.
//!
//! Unix implementations are exactly the syscalls Dial has always issued (`O_NOFOLLOW` opens,
//! `openat` walks, process groups and `killpg`, `O_NONBLOCK` pipes with `poll`). Windows
//! implementations are real equivalents, never no-ops that pretend to succeed:
//!
//! * symlink-safe opens use `FILE_FLAG_OPEN_REPARSE_POINT` and refuse name-surrogate reparse points
//!   (symbolic links, junctions); walks beneath a root use handle-relative `NtCreateFile`;
//! * owner/permission checks read the file's owner SID and DACL; "owner-only" installs a protected
//!   DACL granting only the current user;
//! * process groups are Job Objects (children are created suspended, assigned, then resumed, so no
//!   descendant can escape before containment);
//! * nonblocking pipe reads peek the pipe (`PeekNamedPipe`), so they work for overlapped and
//!   synchronous anonymous pipes alike.
//!
//! See `docs/windows.md` for user-facing behavior.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use imp::pty::{NativePtyChild, spawn_conpty};
#[cfg(windows)]
use windows as imp;

/// Device and file number: equal identities mean the same file object (`st_dev`/`st_ino`, or the
/// Windows volume serial number and 64-bit file index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Filesystem safety primitives.
pub mod fs {
    pub use super::FileIdentity;
    use std::fs::{DirBuilder, File, Metadata, OpenOptions};
    use std::io;
    use std::path::Path;

    /// Native clone where supported; ordinary copying is the portable fallback.
    pub fn copy_on_write(source: &Path, dest: &Path) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};
            let source = CString::new(source.as_os_str().as_bytes())?;
            let target = CString::new(dest.as_os_str().as_bytes())?;
            if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0 {
                return Ok(());
            }
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let input = std::fs::File::open(source)?;
            // An existing destination is replaced, as on macOS: `std::fs::copy` below overwrites.
            if let Ok(output) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)
                && unsafe {
                    libc::ioctl(
                        output.as_raw_fd(),
                        0x40049409 as libc::c_ulong,
                        input.as_raw_fd(),
                    )
                } == 0
            {
                std::fs::set_permissions(dest, input.metadata()?.permissions())?;
                return Ok(());
            }
        }
        std::fs::copy(source, dest).map(|_| ())
    }

    /// Open `path` with `options`, refusing a symbolic link (or, on Windows, a junction or other
    /// name-surrogate reparse point) as the final component. Unix: `O_NOFOLLOW | O_CLOEXEC`.
    pub fn open_nofollow(options: &OpenOptions, path: &Path) -> io::Result<File> {
        super::imp::fs::open_nofollow(options, path, false)
    }

    /// [`open_nofollow`] that also never blocks on a FIFO or device (Unix adds `O_NONBLOCK`; Windows
    /// has no filesystem FIFOs). Callers must still check that the result is a regular file.
    pub fn open_nofollow_nonblocking(options: &OpenOptions, path: &Path) -> io::Result<File> {
        super::imp::fs::open_nofollow(options, path, true)
    }

    /// Open the directory `path` read-only without following a final symlink
    /// (Unix: `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`).
    pub fn open_dir_nofollow(path: &Path) -> io::Result<File> {
        super::imp::fs::open_dir_nofollow(path)
    }

    #[cfg(windows)]
    pub fn open_dir_nofollow_write(path: &Path) -> io::Result<File> {
        super::imp::fs::open_dir_nofollow_write(path)
    }

    /// Open `relative` beneath the directory `root` one component at a time, never following a
    /// symlink anywhere along the way (Unix: an `openat` walk with `O_NOFOLLOW`; Windows:
    /// handle-relative `NtCreateFile` with `FILE_OPEN_REPARSE_POINT`). `relative` must consist of
    /// plain names only. Intermediate components must be directories; the last one must be a
    /// directory when `directory` is set. Files are opened read-only and non-blocking.
    pub fn open_beneath(root: &Path, relative: &Path, directory: bool) -> io::Result<File> {
        super::imp::fs::open_beneath(root, relative, directory)
    }
    /// Create a new file relative to an already-open directory handle; fails if the name exists.
    #[cfg(windows)]
    pub fn create_new_beneath(root: &File, relative: &Path, owner_only: bool) -> io::Result<File> {
        super::imp::fs::create_new_beneath(root, relative, owner_only)
    }

    #[cfg(windows)]
    pub fn link_beneath(root: &File, existing: &Path, new: &Path) -> io::Result<()> {
        super::imp::fs::link_beneath(root, existing, new)
    }

    #[cfg(windows)]
    pub fn rename_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
        super::imp::fs::rename_beneath(root, from, to)
    }

    #[cfg(windows)]
    pub fn replace_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
        super::imp::fs::replace_beneath(root, from, to)
    }

    #[cfg(windows)]
    pub fn replace_beneath_file(
        root: &File,
        from: &Path,
        expected: &File,
        to: &Path,
    ) -> io::Result<()> {
        super::imp::fs::replace_beneath_file(root, from, expected, to)
    }

    #[cfg(windows)]
    pub fn remove_beneath(root: &File, relative: &Path) -> io::Result<()> {
        super::imp::fs::remove_beneath(root, relative)
    }

    #[cfg(windows)]
    pub fn create_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
        super::imp::fs::create_dir_beneath(root, relative)
    }

    #[cfg(windows)]
    pub fn remove_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
        super::imp::fs::remove_dir_beneath(root, relative)
    }

    /// Open a plain name relative to an already-open directory handle without following a final
    /// symlink or reparse point. Read-only and non-blocking.
    pub fn open_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
        super::imp::fs::open_beneath_handle(root, relative)
    }

    #[cfg(windows)]
    pub fn open_dir_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
        super::imp::fs::open_dir_beneath_handle(root, relative)
    }

    #[cfg(windows)]
    pub fn open_dir_beneath_handle_write(root: &File, relative: &Path) -> io::Result<File> {
        super::imp::fs::open_dir_beneath_handle_write(root, relative)
    }

    #[cfg(windows)]
    pub fn open_file_beneath(root: &File, relative: &Path, write: bool) -> io::Result<File> {
        super::imp::fs::open_file_beneath(root, relative, write)
    }

    #[cfg(windows)]
    pub fn read_dir_beneath(root: &File) -> io::Result<Vec<std::ffi::OsString>> {
        super::imp::fs::read_dir_beneath(root)
    }

    /// Whether the open file belongs to the current user (Unix: `st_uid == geteuid()`; Windows:
    /// the owner SID is the token user or the token's default owner).
    pub fn owned_by_current_user(file: &File) -> io::Result<bool> {
        super::imp::fs::owned_by_current_user(file)
    }

    /// [`owned_by_current_user`] for a path, without following a final symlink.
    pub fn path_owned_by_current_user(path: &Path) -> io::Result<bool> {
        super::imp::fs::path_owned_by_current_user(path)
    }

    /// Owned by the operating system (Unix root; Windows LocalSystem/Administrators), without
    /// following a final symlink. Such aliases (macOS `/var`) are trusted path ancestors.
    pub fn path_owned_by_system(path: &Path) -> io::Result<bool> {
        super::imp::fs::path_owned_by_system(path)
    }

    /// Hard-link count of the open file.
    pub fn link_count(file: &File) -> io::Result<u64> {
        super::imp::fs::link_count(file)
    }

    /// Identity of the open file.
    pub fn identity(file: &File) -> io::Result<FileIdentity> {
        super::imp::fs::identity(file)
    }

    /// Identity of `path` itself (a final symlink is not followed, like `lstat`).
    pub fn path_identity(path: &Path) -> io::Result<FileIdentity> {
        super::imp::fs::path_identity(path)
    }

    /// Whether nobody but the owner can access the open file (Unix: `mode & 0o077 == 0`; Windows:
    /// every allow ACE names the current user, SYSTEM or Administrators).
    pub fn is_owner_only(file: &File) -> io::Result<bool> {
        super::imp::fs::is_owner_only(file)
    }

    /// Restrict `path` to its owner: Unix `0o600` for files and `0o700` for directories (symlinks
    /// are followed, as `chmod` does); Windows a protected DACL granting only the current user
    /// (inherited by children of a directory).
    pub fn set_owner_only(path: &Path) -> io::Result<()> {
        super::imp::fs::set_owner_only(path)
    }

    /// Restrict permissions on an already-open file or directory handle without reopening its path.
    pub fn set_owner_only_file(file: &File) -> io::Result<()> {
        super::imp::fs::set_owner_only_file(file)
    }

    /// Apply a POSIX mode. Windows has no group/other bits: a mode without them installs the
    /// owner-only DACL; a mode without the owner write bit sets the read-only attribute; execute
    /// bits have no Windows equivalent (executability comes from the file extension).
    pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        super::imp::fs::set_mode(path, mode)
    }

    /// POSIX permission bits of `meta`. Windows synthesizes them: `0o444`/`0o666` from the
    /// read-only attribute, plus `0o111` for directories.
    pub fn mode(meta: &Metadata) -> u32 {
        super::imp::fs::mode(meta)
    }

    /// Whether `path` is an executable regular file (Unix: any execute bit; Windows: an extension
    /// listed in `PATHEXT`).
    pub fn is_executable(path: &Path) -> bool {
        super::imp::fs::is_executable(path)
    }

    /// Open `path`, creating it owner-only if it does not exist (Unix: `mode(0o600)`; Windows: the
    /// owner-only DACL is installed on a file this call created).
    pub fn open_owner_only(options: &OpenOptions, path: &Path) -> io::Result<File> {
        super::imp::fs::open_owner_only(options, path)
    }

    /// Create directories like `builder.create(path)`, the new ones owner-only (Unix: `mode(0o700)`;
    /// Windows: the owner-only DACL on `path` when this call created it).
    pub fn create_dir_owner_only(builder: &DirBuilder, path: &Path) -> io::Result<()> {
        super::imp::fs::create_dir_owner_only(builder, path)
    }
}

/// Process groups: a leader and every descendant, signalled and killed together.
pub mod process {
    use std::io;
    use std::process::{Child, Command, ExitStatus};

    /// Spawn `command` as the leader of a new process group (Unix: `process_group(0)`; Windows: a
    /// new console process group inside a Job Object assigned before the child runs any code).
    /// On Windows this sets the command's creation flags; do not set them separately.
    pub fn spawn_group(command: &mut Command) -> io::Result<Child> {
        super::imp::process::spawn_group(command)
    }

    /// Kill the group led by `leader` immediately (Unix: `killpg(SIGKILL)`; Windows:
    /// `TerminateJobObject`, or a creation-time-checked process-tree kill for a group spawned by
    /// another Dial process that has since exited).
    pub fn kill_group(leader: u32) -> io::Result<()> {
        super::imp::process::kill_group(leader)
    }

    /// Ask the group led by `leader` to stop (Unix: `killpg(SIGTERM)`; Windows: `CTRL_BREAK` to
    /// the console process group, falling back to [`kill_group`] when no console is shared).
    pub fn terminate_group(leader: u32) -> io::Result<()> {
        super::imp::process::terminate_group(leader)
    }

    /// Terminate an identity-validated Windows handle without reopening its PID. An owned job
    /// is terminated only if this exact process belongs to it.
    #[cfg(windows)]
    pub fn kill_checked(process: std::os::windows::io::BorrowedHandle<'_>) -> io::Result<()> {
        super::imp::process::kill_checked(process)
    }

    #[cfg(test)]
    /// Whether `pid` names a live process this user may signal (Unix: `kill(pid, 0) == 0`).
    pub fn alive(pid: u32) -> bool {
        super::imp::process::alive(pid)
    }

    /// Whether `pid` leads its own process group (Unix: `getpgid(pid) == pid`; Windows: `pid` is
    /// inside the Job Object Dial created for it).
    pub fn is_group_leader(pid: u32) -> bool {
        super::imp::process::is_group_leader(pid)
    }

    /// True once `child` has exited, without reaping it, so its PID stays reserved (Unix:
    /// `waitid(WNOWAIT)`; Windows: the process handle, held by `Child`, is signalled).
    pub fn exited(child: &Child) -> io::Result<bool> {
        super::imp::process::exited(child)
    }

    /// Blocks until `child` exits or `timeout` elapses; the child is not reaped. Returns whether
    /// it exited. This is the event-driven wait used in place of a polling loop.
    pub fn wait_exited(child: &Child, timeout: std::time::Duration) -> io::Result<bool> {
        super::imp::process::wait_exited(child, timeout)
    }

    /// The signal that terminated the process, if any (always `None` on Windows).
    pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
        super::imp::process::exit_signal(status)
    }

    /// Release bookkeeping for a reaped group leader (Windows: the Job Object handle).
    pub fn forget_group(leader: u32) {
        super::imp::process::forget_group(leader)
    }
}

pub(crate) mod atomic;
pub mod interrupt;
pub mod process_record;
pub mod procinfo;
/// Event-driven waits: pipe readiness, child exit, and cancellation wakeups.
pub mod wake;

/// Nonblocking pipe I/O.
pub mod io {
    use std::io::{self, Read};
    use std::time::Duration;

    /// A pipe or terminal handle: `AsFd` on Unix, `AsHandle` on Windows.
    #[cfg(unix)]
    pub trait Pipe: std::os::fd::AsFd {}
    #[cfg(unix)]
    impl<T: std::os::fd::AsFd + ?Sized> Pipe for T {}
    /// A pipe or terminal handle: `AsFd` on Unix, `AsHandle` on Windows.
    #[cfg(windows)]
    pub trait Pipe: std::os::windows::io::AsHandle {}
    #[cfg(windows)]
    impl<T: std::os::windows::io::AsHandle + ?Sized> Pipe for T {}

    /// Restore blocking mode on an adopted output descriptor (Unix: clear `O_NONBLOCK`; Windows:
    /// the handle mode is not changed).
    pub fn set_blocking(pipe: &impl Pipe) -> io::Result<()> {
        super::imp::io::set_blocking(pipe)
    }

    /// Prepare `pipe` for [`read_nonblocking`] (Unix: `O_NONBLOCK`; Windows: verifies the handle is
    /// a pipe, whose reads are then bounded by `PeekNamedPipe`).
    pub fn set_nonblocking(pipe: &impl Pipe) -> io::Result<()> {
        super::imp::io::set_nonblocking(pipe)
    }

    /// Read what is available without blocking: `Ok(0)` is EOF and `WouldBlock` means no data yet.
    /// `reader` must have been passed to [`set_nonblocking`].
    pub fn read_nonblocking<R: Read + Pipe>(
        reader: &mut R,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        super::imp::io::read_nonblocking(reader, buffer)
    }

    /// Wait up to `timeout` until `pipe` has data or reached EOF (Unix: `poll(POLLIN)`). Returns
    /// whether it became readable; an interrupted wait returns `false`.
    pub fn wait_readable(pipe: &impl Pipe, timeout: Duration) -> io::Result<bool> {
        super::imp::io::wait_readable(pipe, timeout)
    }

    /// Wait up to `timeout` until a pipe can accept a write. Windows anonymous pipes expose no
    /// writable readiness signal; this returns `Unsupported` rather than reporting false readiness.
    pub fn wait_writable(pipe: &impl Pipe, timeout: Duration) -> io::Result<bool> {
        super::imp::io::wait_writable(pipe, timeout)
    }

    /// A bounded asynchronous writer for synchronous Windows pipe handles. The worker owns the
    /// writer and performs blocking writes; callers only enqueue owned bytes.
    #[cfg(windows)]
    pub fn duplicate_stdout() -> io::Result<std::fs::File> {
        super::imp::io::duplicate_stdout()
    }
    /// A duplicated stdin handle with console-aware nonblocking reads.
    #[cfg(windows)]
    pub use super::imp::console::duplicate_stdin_reader;
    #[cfg(windows)]
    pub struct QueuedWriter<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> {
        state: std::sync::Arc<(
            std::sync::Mutex<WriterState>,
            std::sync::Condvar,
            std::sync::atomic::AtomicBool,
        )>,
        worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
        capacity: usize,
        _writer: std::marker::PhantomData<W>,
    }
    #[cfg(windows)]
    struct WriterState {
        queue: std::collections::VecDeque<Vec<u8>>,
        bytes: usize,
        delivered: usize,
        closed: bool,
        failure: Option<(io::ErrorKind, String)>,
    }
    #[cfg(windows)]
    impl<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> QueuedWriter<W> {
        pub fn new(mut writer: W, capacity: usize) -> Self {
            let capacity = capacity.max(1);
            let state = std::sync::Arc::new((
                std::sync::Mutex::new(WriterState {
                    queue: std::collections::VecDeque::new(),
                    bytes: 0,
                    delivered: 0,
                    closed: false,
                    failure: None,
                }),
                std::sync::Condvar::new(),
                std::sync::atomic::AtomicBool::new(false),
            ));
            let worker_state = state.clone();
            let worker = std::thread::spawn(move || {
                loop {
                    let (lock, ready, abort) = &*worker_state;
                    let mut current = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while current.queue.is_empty() && !current.closed {
                        current = ready.wait(current).unwrap_or_else(|p| p.into_inner());
                    }
                    if current.queue.is_empty() && current.closed {
                        return;
                    }
                    let bytes = current.queue.pop_front().unwrap();
                    drop(current);
                    let mut delivered = 0;
                    let result = loop {
                        if delivered == bytes.len() {
                            break Ok(());
                        }
                        if abort.load(std::sync::atomic::Ordering::Acquire) {
                            break Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "queued pipe writer cancelled",
                            ));
                        }
                        match writer.write(&bytes[delivered..]) {
                            Ok(0) => {
                                break Err(io::Error::new(
                                    io::ErrorKind::WriteZero,
                                    "queued pipe writer wrote zero bytes",
                                ));
                            }
                            Ok(n) => delivered += n,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => break Err(e),
                        }
                    };
                    let mut current = lock.lock().unwrap_or_else(|p| p.into_inner());
                    current.delivered += delivered;
                    current.bytes -= bytes.len();
                    if let Err(error) = result {
                        current.failure = Some((error.kind(), error.to_string()));
                        current.bytes -= current.queue.iter().map(Vec::len).sum::<usize>();
                        current.queue.clear();
                        current.closed = true;
                    }
                    ready.notify_all();
                }
            });
            Self {
                state,
                worker: std::sync::Mutex::new(Some(worker)),
                capacity,
                _writer: std::marker::PhantomData,
            }
        }
        /// Enqueues up to the available bounded capacity. Returned bytes are accepted, not yet delivered.
        pub fn try_write(&self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            let (lock, ready, _) = &*self.state;
            let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((kind, message)) = &state.failure {
                return Err(io::Error::new(*kind, message.clone()));
            }
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "queued pipe writer is closed",
                ));
            }
            let count = bytes.len().min(self.capacity.saturating_sub(state.bytes));
            if count == 0 || state.queue.len() >= 128 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            state.queue.push_back(bytes[..count].to_vec());
            state.bytes += count;
            ready.notify_one();
            Ok(count)
        }
        /// Number of bytes successfully written to the underlying writer.
        pub fn delivered_bytes(&self) -> usize {
            self.state
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .delivered
        }
        /// Waits for accepted bytes to be delivered; returns false on timeout.
        pub fn wait_until_drained(&self, timeout: Duration) -> io::Result<bool> {
            let deadline = std::time::Instant::now() + timeout;
            let (lock, ready, _) = &*self.state;
            let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if let Some((kind, message)) = &state.failure {
                    return Err(io::Error::new(*kind, message.clone()));
                }
                if state.bytes == 0 {
                    return Ok(true);
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Ok(false);
                }
                let (next, result) = ready
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|p| p.into_inner());
                state = next;
                if result.timed_out() && state.bytes != 0 {
                    return Ok(false);
                }
            }
        }
        /// Closes the queue, drains accepted bytes, then drops the owned writer to signal EOF.
        pub fn finish(&self, timeout: Duration) -> io::Result<bool> {
            let (lock, ready, _) = &*self.state;
            {
                let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                state.closed = true;
                ready.notify_all();
            }
            if !self.wait_until_drained(timeout)? {
                self.cancel_worker();
                return Ok(false);
            }
            self.join_worker();
            Ok(true)
        }
        fn cancel_worker(&self) {
            use std::{os::windows::io::AsRawHandle, time::Instant};
            self.state
                .2
                .store(true, std::sync::atomic::Ordering::Release);
            {
                let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
                state.closed = true;
                state.bytes -= state.queue.iter().map(Vec::len).sum::<usize>();
                state.queue.clear();
            }
            self.state.1.notify_all();
            let mut worker = self.worker.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(handle) = worker.as_ref() {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !handle.is_finished() && Instant::now() < deadline {
                    unsafe {
                        windows_sys::Win32::System::IO::CancelSynchronousIo(
                            handle.as_raw_handle() as _
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if handle.is_finished() {
                    if let Some(handle) = worker.take() {
                        let _ = handle.join();
                    }
                }
            }
        }
        fn join_worker(&self) {
            if let Some(worker) = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = worker.join();
            }
        }
    }
    /// Nonblocking `Write` integration: accepted bytes are queued; `flush` returns `WouldBlock`
    /// until the worker has delivered them, allowing callers to enforce their own deadline.
    #[cfg(windows)]
    impl<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> std::io::Write
        for QueuedWriter<W>
    {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.try_write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.wait_until_drained(Duration::ZERO)? {
                Ok(())
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
    }
    #[cfg(windows)]
    impl<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> Drop
        for QueuedWriter<W>
    {
        fn drop(&mut self) {
            let (lock, ready, _) = &*self.state;
            let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
            state.bytes -= state.queue.iter().map(Vec::len).sum::<usize>();
            state.queue.clear();
            drop(state);
            ready.notify_all();
            self.cancel_worker();
        }
    }
}

/// Time without a date library.
pub mod time {
    /// Seconds since the Unix epoch for an exact `YYYY-MM-DDTHH:MM:SS` UTC timestamp.
    pub fn parse_utc(text: &str) -> Option<i64> {
        let bytes = text.as_bytes();
        if bytes.len() != 19
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
        {
            return None;
        }
        let field = |range: std::ops::Range<usize>| -> Option<i64> {
            let part = text.get(range)?;
            part.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| part.parse().ok())?
        };
        let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
        let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
        if !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
    }

    fn days_in_month(year: i64, month: i64) -> i64 {
        match month {
            2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        }
    }

    /// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    /// Format the current local time. Supports `%Y %m %d %H %M %S %%`, the subset Dial uses; any
    /// other conversion makes the result empty rather than silently wrong.
    #[cfg(windows)]
    pub fn local_strftime(format: &str) -> String {
        super::imp::time::local_strftime(format)
    }

    #[cfg(test)]
    mod tests;
}

/// Host facts and process-wide signal handling.
pub mod sys {
    use std::io;

    /// The host name (Unix: `gethostname`; Windows: the physical DNS host name).
    pub fn hostname() -> io::Result<String> {
        super::imp::sys::hostname()
    }

    /// Run `handler` on every interactive interrupt (Unix: `SIGINT` via `sigaction`; Windows:
    /// Ctrl+C / Ctrl+Break console events). `handler` runs in signal context on Unix and on a
    /// console-control thread on Windows: it must only touch atomics or call [`exit_now`].
    pub fn on_interrupt(handler: fn()) {
        super::imp::sys::on_interrupt(handler)
    }

    /// Terminate immediately without running destructors or exit handlers (`_exit`/`ExitProcess`).
    pub fn exit_now(code: i32) -> ! {
        super::imp::sys::exit_now(code)
    }
}
