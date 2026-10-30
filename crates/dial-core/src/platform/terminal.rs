//! Unix PTY allocation shared by foreground wrappers and managed background jobs.
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::Command,
};

/// Raw stdin with an exact saved termios snapshot, including platform extension flags.
/// Snapshot before changing terminal state; restore every saved flag, including BSD state bits.
pub struct RawMode {
    stdin: File,
    original: libc::termios,
    active: bool,
}

impl RawMode {
    pub fn enter() -> io::Result<Self> {
        // SAFETY: stdin is borrowed only for duplication; the guard owns the duplicate.
        let stdin =
            File::from(unsafe { std::os::fd::BorrowedFd::borrow_raw(0) }.try_clone_to_owned()?);
        Self::from_terminal(stdin)
    }

    fn from_terminal(stdin: File) -> io::Result<Self> {
        let mut original = std::mem::MaybeUninit::uninit();
        // SAFETY: live descriptor and a writable termios buffer.
        if unsafe { libc::tcgetattr(stdin.as_raw_fd(), original.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful tcgetattr initialized the buffer.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // SAFETY: cfmakeraw updates a valid termios value.
        unsafe { libc::cfmakeraw(&mut raw) };
        let mut mode = Self {
            stdin,
            original,
            active: true,
        };
        // SAFETY: the descriptor remains owned by mode and raw is initialized.
        if unsafe { libc::tcsetattr(mode.stdin.as_raw_fd(), libc::TCSANOW, &raw) } == -1 {
            let error = io::Error::last_os_error();
            let _ = mode.restore();
            return Err(error);
        }
        Ok(mode)
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if self.active {
            // SAFETY: live owned descriptor and the exact initialized snapshot.
            if unsafe { libc::tcsetattr(self.stdin.as_raw_fd(), libc::TCSANOW, &self.original) }
                == -1
            {
                return Err(io::Error::last_os_error());
            }
            // Darwin sets PENDIN when restoring ICANON. FIONREAD asks its line discipline
            // to reprocess pending input without consuming it, clearing the synthesized
            // state bit. A second tcsetattr alone does not clear it on macOS.
            #[cfg(target_os = "macos")]
            if self.original.c_lflag & libc::PENDIN == 0 {
                let mut available: libc::c_int = 0;
                // SAFETY: live descriptor and correctly sized writable count buffer.
                if unsafe { libc::ioctl(self.stdin.as_raw_fd(), libc::FIONREAD, &mut available) }
                    == -1
                {
                    return Err(io::Error::last_os_error());
                }
            }
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub fn open(rows: u16, cols: u16) -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: valid output pointers and a valid size; null name/termios request defaults.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned two distinct owned descriptors.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: ownership of slave is transferred exactly once.
    let slave = unsafe { File::from_raw_fd(slave) };
    for file in [&master, &slave] {
        // SAFETY: fcntl operates on a live descriptor; no pointer arguments.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        if flags == -1
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
    }
    super::io::set_nonblocking(&master)?;
    Ok((master, slave))
}

/// Configure a child with the slave already installed as stdin/stdout/stderr.
pub fn controlling_child(command: &mut Command) {
    // SAFETY: this pre-exec closure performs only async-signal-safe syscalls and does not
    // access parent locks or allocate. Stdin is the slave descriptor installed by Command.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            // A wrapper launched by a background shell may inherit ignored SIGINT/job-control
            // signals. Exec preserves ignored dispositions; a fresh controlling session must not.
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            for signal in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
            ] {
                if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &action.sa_mask, std::ptr::null_mut()) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn resize(file: &File, rows: u16, cols: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: live PTY descriptor and valid input size; kernel sends SIGWINCH to foreground group.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Wait briefly for terminal I/O without changing the caller's stdin flags.
pub fn ready(
    input: bool,
    master: &(impl AsRawFd + ?Sized),
    writing: bool,
) -> io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd {
            fd: if input { 0 } else { -1 },
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: master.as_raw_fd(),
            events: if writing { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        },
    ];
    // SAFETY: valid pollfd array, bounded timeout.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 50) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok((
        fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0,
        fds[1].revents != 0,
    ))
}

static SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn record_signal(signal: i32) {
    SIGNAL.store(signal, std::sync::atomic::Ordering::Relaxed);
}

/// Scoped signal bridge for the single foreground wrapper. Handlers only set an atomic.
pub struct Signals(Vec<(i32, libc::sigaction)>);
impl Signals {
    pub fn install() -> io::Result<Self> {
        if INSTALLED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "foreground signal bridge already installed",
            ));
        }
        SIGNAL.store(0, std::sync::atomic::Ordering::Relaxed);
        let mut guard = Self(Vec::new());
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: zeroed sigaction is initialized below and used only with sigaction.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: old action is a valid output buffer; empty mask is initialized by libc.
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = record_signal as *const () as usize;
            // SAFETY: valid action and output pointers; handler uses only lock-free atomic store.
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            if unsafe { libc::sigaction(signal, &action, &mut old) } == -1 {
                return Err(io::Error::last_os_error());
            }
            guard.0.push((signal, old));
        }
        Ok(guard)
    }
    pub fn take(&self) -> i32 {
        SIGNAL.swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, action) in &self.0 {
            // SAFETY: restores the action captured for this signal while the guard was installed.
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
        INSTALLED.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
