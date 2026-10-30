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
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), value.as_mut_ptr()) },
            0
        );
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
