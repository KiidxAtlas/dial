use super::*;
use crate::platform::process;
use std::{
    io::{Read, Write},
    process::Stdio,
    time::{Duration, Instant},
};

fn read_until(master: &mut File, text: &mut Vec<u8>, needle: &[u8]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buffer = [0; 4096];
    while Instant::now() < deadline {
        match master.read(&mut buffer) {
            Ok(n) if n > 0 => text.extend_from_slice(&buffer[..n]),
            Ok(_) => return false,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return false,
        }
        if text.windows(needle.len()).any(|bytes| bytes == needle) {
            return true;
        }
    }
    false
}

#[test]
fn controlling_pty_preserves_colors_input_size_and_ctrl_c() {
    let (mut master, slave) = open(24, 80).unwrap();
    let mut command = Command::new("sh");
    command.args(["-c", r#"trap 'printf "int-handled\n"; exit 42' INT; printf '\033[32mready\033[0m\n'; read line; stty size; printf 'input:%s\n' "$line"; while :; do sleep 1; done"#])
        .stdin(slave.try_clone().unwrap()).stdout(slave.try_clone().unwrap()).stderr(slave);
    controlling_child(&mut command);
    let mut child = process::spawn(&mut command).unwrap();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut bytes = Vec::new();
    let ready = read_until(&mut master, &mut bytes, b"ready");
    let resized = resize(&master, 31, 99).is_ok();
    let sent = master.write_all(b"hello\n").is_ok();
    let echoed = read_until(&mut master, &mut bytes, b"input:hello");
    let interrupted = master.write_all(&[3]).is_ok();
    let handled = read_until(&mut master, &mut bytes, b"int-handled");
    let exited = process::wait_exited(&child, Duration::from_secs(5)).unwrap();
    // Keep the leader unreaped until cleanup, preventing group-id reuse.
    let _ = process::kill_group(child.id());
    drop(master);
    let status = child.wait().unwrap();
    assert!(
        ready && resized && sent && echoed && interrupted && handled && exited,
        "ready={ready} resized={resized} sent={sent} echoed={echoed} interrupted={interrupted} handled={handled} exited={exited} status={status:?}: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        bytes.windows(5).any(|part| part == b"31 99"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(bytes.windows(5).any(|part| part == b"\x1b[32m"));
    assert_eq!(status.code(), Some(42));
}

#[test]
fn raw_mode_restores_exact_attributes_without_consuming_pending_input() {
    let (mut master, slave) = open(24, 80).unwrap();
    let attributes = || {
        let mut value = std::mem::MaybeUninit::uninit();
        // SAFETY: live slave descriptor and valid output buffer.
        let result = unsafe { libc::tcgetattr(slave.as_raw_fd(), value.as_mut_ptr()) };
        assert_eq!(result, 0, "read PTY attributes");
        // SAFETY: successful tcgetattr initialized the value.
        unsafe { value.assume_init() }
    };
    let original = attributes();
    let mut mode = RawMode::from_terminal(slave.try_clone().unwrap()).unwrap();
    master.write_all(b"pending-input\n").unwrap();
    mode.restore().unwrap();
    let restored = attributes();
    assert_eq!(original.c_iflag, restored.c_iflag);
    assert_eq!(original.c_oflag, restored.c_oflag);
    assert_eq!(original.c_cflag, restored.c_cflag);
    assert_eq!(original.c_lflag, restored.c_lflag);
    assert_eq!(original.c_cc, restored.c_cc);
    super::super::io::set_nonblocking(&slave).unwrap();
    let mut bytes = Vec::new();
    let mut input = &slave;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !bytes.ends_with(b"\n") && Instant::now() < deadline {
        let mut buffer = [0; 64];
        match input.read(&mut buffer) {
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("pending input: {error}"),
        }
    }
    assert_eq!(bytes, b"pending-input\n");
}

// Signal dispositions are process-global: run this driver alone in a child harness.
#[test]
fn termination_signal_child() {
    if std::env::var_os("DIAL_TEST_TERMINATION_SIGNALS").is_none() {
        return;
    }
    use std::sync::atomic::{AtomicI32, Ordering};
    static PREVIOUS: AtomicI32 = AtomicI32::new(0);
    extern "C" fn previous(signal: i32) {
        PREVIOUS.store(signal, Ordering::SeqCst);
    }
    fn disposition(signal: i32) -> libc::sigaction {
        let mut action = std::mem::MaybeUninit::uninit();
        // SAFETY: valid output buffer and a known signal number.
        let result = unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) };
        assert_eq!(result, 0, "query signal {signal}");
        // SAFETY: successful sigaction initialized the output.
        unsafe { action.assume_init() }
    }
    fn raise(signal: i32) {
        // SAFETY: the child installed a nonterminating handler for every signal raised here.
        let result = unsafe { libc::raise(signal) };
        assert_eq!(result, 0, "raise signal {signal}");
    }
    let signals = [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGINT];
    let mut saved = Vec::new();
    for signal in signals {
        saved.push((signal, disposition(signal)));
        // SAFETY: zeroed sigaction is completed before passing it to libc.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = previous as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: valid writable mask; SIGUSR1 is a valid signal.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1);
        }
        // SAFETY: fully initialized action and a known signal number.
        let result = unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        assert_eq!(result, 0, "install previous handler for {signal}");
    }
    let before: Vec<_> = signals.into_iter().map(disposition).collect();
    assert_eq!(Signals::pending(), 0, "no bridge before installation");
    let guard = Signals::install_termination().unwrap();
    assert!(
        matches!(Signals::install_termination(), Err(e) if e.kind() == io::ErrorKind::AlreadyExists)
    );
    assert!(matches!(Signals::install(), Err(e) if e.kind() == io::ErrorKind::AlreadyExists));
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        raise(signal);
        assert_eq!(Signals::pending(), signal, "record signal {signal}");
        assert_eq!(
            Signals::pending(),
            signal,
            "observation must not consume {signal}"
        );
        assert_eq!(guard.take(), signal, "consume signal {signal}");
        assert_eq!(Signals::pending(), 0, "clear consumed signal {signal}");
        assert_eq!(
            PREVIOUS.load(Ordering::SeqCst),
            0,
            "previous handler must be scoped out"
        );
    }
    raise(libc::SIGINT);
    assert_eq!(
        PREVIOUS.load(Ordering::SeqCst),
        libc::SIGINT,
        "Ctrl-C retains its owner"
    );
    assert_eq!(
        Signals::pending(),
        0,
        "Ctrl-C is not a TUI termination signal"
    );
    raise(libc::SIGTERM);
    drop(guard);
    assert_eq!(
        Signals::pending(),
        0,
        "dropped bridge cannot leak a pending termination"
    );
    for (signal, before) in signals.into_iter().zip(before) {
        let after = disposition(signal);
        assert_eq!(
            after.sa_sigaction, before.sa_sigaction,
            "restore handler {signal}"
        );
        assert_eq!(after.sa_flags, before.sa_flags, "restore flags {signal}");
        for member in 1..32 {
            // SAFETY: both masks came from successful sigaction queries; member is valid on Unix.
            let (after, before) = unsafe {
                (
                    libc::sigismember(&after.sa_mask, member),
                    libc::sigismember(&before.sa_mask, member),
                )
            };
            assert_eq!(after, before, "restore mask member {member} for {signal}");
        }
        PREVIOUS.store(0, Ordering::SeqCst);
        raise(signal);
        assert_eq!(
            PREVIOUS.load(Ordering::SeqCst),
            signal,
            "restored handler runs for {signal}"
        );
    }
    let again = Signals::install_termination().unwrap();
    assert_eq!(
        Signals::pending(),
        0,
        "fresh bridge clears previous termination"
    );
    drop(again);
    for (signal, original) in saved {
        // SAFETY: restores the original disposition captured in this isolated child.
        let result = unsafe { libc::sigaction(signal, &original, std::ptr::null_mut()) };
        assert_eq!(result, 0, "restore original disposition {signal}");
    }
}

#[test]
fn termination_signals_are_scoped_and_restore_previous_handlers() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "platform::terminal::tests::termination_signal_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DIAL_TEST_TERMINATION_SIGNALS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process::spawn(&mut command).unwrap();
    let exited = process::wait_exited(&child, Duration::from_secs(5)).unwrap();
    if !exited {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        exited && output.status.success(),
        "signal child exited={exited}: {output:?}"
    );
}
