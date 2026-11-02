//! Actual-binary Unix terminal teardown with an app-owned relay, without model inference.
//! Port rebinding establishes listener closure, not independent proof of child reaping.
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use dial::platform::{process, terminal};
use std::{
    fs::File,
    io::{self, Read, Write},
    net::TcpListener,
    os::fd::AsRawFd,
    process::{Child, Command},
    time::{Duration, Instant},
};

const QUERY: &[u8] = b"\x1b[?u\x1b[c";
const PUSH: &[u8] = b"\x1b[>7u";
const POP: &[u8] = b"\x1b[<1u";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";

struct Session {
    child: Child,
    master: File,
    slave: File,
    original: libc::termios,
    output: Vec<u8>,
    answered: usize,
    // Keep all private state alive until after process cleanup.
    _root: tempfile::TempDir,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Kill before reaping so the leader's identity cannot be recycled while cleaning up
        // descendants. All behavioral assertions run before this fallback, including on error.
        let _ = process::kill_group(self.child.id());
        let _ = self.child.wait();
    }
}

fn termios(file: &File) -> io::Result<libc::termios> {
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: file owns a live terminal descriptor and value is a writable termios buffer.
    if unsafe { libc::tcgetattr(file.as_raw_fd(), value.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful tcgetattr initialized value.
    Ok(unsafe { value.assume_init() })
}

fn same_termios(original: &libc::termios, restored: &libc::termios) -> Result<()> {
    ensure!(original.c_iflag == restored.c_iflag, "c_iflag not restored");
    ensure!(original.c_oflag == restored.c_oflag, "c_oflag not restored");
    ensure!(original.c_cflag == restored.c_cflag, "c_cflag not restored");
    ensure!(
        original.c_lflag == restored.c_lflag,
        "c_lflag not restored: original={:#x}, restored={:#x}",
        original.c_lflag,
        restored.c_lflag
    );
    ensure!(original.c_cc == restored.c_cc, "c_cc not restored");
    // SAFETY: callers pass termios snapshots initialized by successful tcgetattr calls.
    let speeds_match = unsafe {
        libc::cfgetispeed(original) == libc::cfgetispeed(restored)
            && libc::cfgetospeed(original) == libc::cfgetospeed(restored)
    };
    ensure!(speeds_match, "terminal speeds not restored");
    Ok(())
}

fn poll(file: &File, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY deadline elapsed",
            ));
        }
        let mut fd = libc::pollfd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        };
        // Bound each event wait so exit can also be observed when no terminal bytes arrive.
        let millis = remaining.as_millis().clamp(1, 50) as libc::c_int;
        // SAFETY: one valid writable pollfd, with a bounded nonnegative timeout.
        let result = unsafe { libc::poll(&mut fd, 1, millis) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn occurrences(bytes: &[u8], needle: &[u8]) -> usize {
    bytes.windows(needle.len()).filter(|w| *w == needle).count()
}

impl Session {
    fn start(backend: &TcpListener) -> Result<Self> {
        let root = tempfile::tempdir()?;
        for name in ["home", "private", "project", "tmp", "cache", "config"] {
            std::fs::create_dir(root.path().join(name))?;
        }
        std::fs::write(
            root.path().join("private/config.toml"),
            "integrations = []\n[improve]\nenabled = false\n[memory]\nenabled = false\n[stats]\nenabled = false\n[sandbox]\nmode = \"off\"\nallow_network = false\n",
        )?;
        let (master, slave) = terminal::open(32, 140)?;
        let original = termios(&slave)?;
        same_termios(&original, &termios(&master)?)
            .context("PTY master must mirror the slave's terminal attributes")?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_dial"));
        command
            .env_clear()
            .env("HOME", root.path().join("home"))
            .env("DIAL_HOME", root.path().join("private"))
            .env("TMPDIR", root.path().join("tmp"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("TERM", "xterm-256color")
            .env("PATH", "/usr/bin:/bin")
            .current_dir(root.path().join("project"))
            .args([
                "--model",
                "scripted",
                "--endpoint",
                &format!("http://{}/v1", backend.local_addr()?),
            ])
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave.try_clone()?);
        terminal::controlling_child(&mut command);
        let child = process::spawn(&mut command)?;
        Ok(Self {
            child,
            master,
            slave,
            original,
            output: Vec::new(),
            answered: 0,
            _root: root,
        })
    }

    fn write(&mut self, mut bytes: &[u8], deadline: Instant) -> Result<()> {
        while !bytes.is_empty() {
            match self.master.write(bytes) {
                Ok(0) => anyhow::bail!("PTY write returned zero"),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    poll(&self.master, libc::POLLOUT, deadline)?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
            ensure!(Instant::now() < deadline, "PTY write timed out");
        }
        Ok(())
    }

    fn drain(&mut self, deadline: Instant) -> Result<()> {
        let mut buffer = [0; 8192];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => self.output.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            ensure!(
                self.output.len() < 4 * 1024 * 1024,
                "unbounded terminal output"
            );
            ensure!(Instant::now() < deadline, "terminal output did not quiesce");
        }
        // Reply to each complete crossterm inquiry, including inquiries split across reads.
        // The DA response is needed as well as the keyboard-flags response.
        let inquiries = occurrences(&self.output, QUERY);
        while self.answered < inquiries {
            self.write(b"\x1b[?0u\x1b[?1;2c", deadline)?;
            self.answered += 1;
        }
        Ok(())
    }

    fn until(&mut self, label: &str, predicate: impl Fn(&[u8]) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.drain(deadline)?;
            if predicate(&self.output) {
                return Ok(());
            }
            ensure!(!process::exited(&self.child)?, "Dial exited before {label}");
            poll(&self.master, libc::POLLIN, deadline)
                .with_context(|| format!("waiting for {label}"))?;
        }
    }

    fn check(&mut self, signal: libc::c_int) -> Result<()> {
        self.until("keyboard push and paste enable", |bytes| {
            occurrences(bytes, PUSH) > 0 && occurrences(bytes, PASTE_ON) > 0
        })?;
        ensure!(self.answered > 0, "keyboard inquiry was not negotiated");
        let active = termios(&self.slave)?;
        ensure!(
            active.c_lflag & libc::ICANON == 0,
            "Dial did not enter raw mode"
        );

        // Reserve a unique loopback address until just before the app binds. The app's actual
        // readiness notice (not a TCP probe) establishes ownership despite this small bind gap.
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        let address = reservation.local_addr()?;
        drop(reservation);
        self.write(
            format!("/relay start {address}\r").as_bytes(),
            Instant::now() + Duration::from_secs(5),
        )?;
        let ready = format!("Listening on {address}.");
        self.until("relay Listening readiness", |bytes| {
            occurrences(bytes, ready.as_bytes()) > 0
        })?;
        ensure!(
            TcpListener::bind(address).is_err(),
            "announced relay listener was not present"
        );

        // Signal only Dial, never the process group: otherwise a broken relay cleanup is hidden.
        // SAFETY: the unreaped owned child retains this PID; signal is one of the table entries.
        ensure!(unsafe { libc::kill(self.child.id() as libc::pid_t, signal) } == 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain(deadline)?;
            if process::exited(&self.child)? {
                self.drain(deadline)?;
                break;
            }
            poll(&self.master, libc::POLLIN, deadline).context("waiting for Dial exit")?;
        }

        // None of these checks may be satisfied by the Drop fallback's group kill.
        // Darwin revokes existing slave descriptors when the controlling-session leader exits.
        // The master still exposes the same terminal attributes (validated before spawning),
        // including unrestored raw-mode bits; reading it does not repair terminal state.
        let restored = termios(&self.master).context("reading restored termios via PTY master")?;
        let terminal_result = same_termios(&self.original, &restored);
        for (enable, disable, label) in [
            (PUSH, POP, "keyboard flags"),
            (PASTE_ON, PASTE_OFF, "paste"),
        ] {
            let mut balance = 0;
            for index in 0..self.output.len() {
                if self.output[index..].starts_with(enable) {
                    balance += 1;
                }
                if self.output[index..].starts_with(disable) {
                    balance -= 1;
                    ensure!(balance >= 0, "{label} disable preceded enable");
                }
            }
            ensure!(balance == 0, "unbalanced {label} enable/disable: {balance}");
        }
        let relay_result = TcpListener::bind(address).context("relay listener survived Dial exit");
        terminal_result?;
        let _closed = relay_result?;
        Ok(())
    }
}

#[test]
fn termination_restores_terminal_and_stops_owned_relay() {
    let mut failures = Vec::new();
    for (name, signal) in [
        ("TERM", libc::SIGTERM),
        ("HUP", libc::SIGHUP),
        ("QUIT", libc::SIGQUIT),
    ] {
        // A real local socket, but no inference server: any connection is a fixture violation.
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        backend.set_nonblocking(true).unwrap();
        let mut session = Session::start(&backend).unwrap();
        if let Err(error) = session.check(signal) {
            let tail = &session.output[session.output.len().saturating_sub(3_000)..];
            failures.push(format!(
                "{name}: {error:#}\nPTY tail: {:?}",
                String::from_utf8_lossy(tail)
            ));
        }
        match backend.accept() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            result => failures.push(format!("{name}: unexpected backend connection: {result:?}")),
        }
        // Per-case cleanup happens only after all observations were captured above.
        drop(session);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
