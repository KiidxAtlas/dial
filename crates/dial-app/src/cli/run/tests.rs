use super::*;
use clap::Parser;

#[derive(Parser)]
struct Harness {
    #[command(flatten)]
    args: RunArgs,
}

#[test]
fn executable_arguments_are_literal_and_command_is_required() {
    let args = Harness::try_parse_from([
        "run", "--name", "web", "--", "pnpm", "start", "--port", "3000",
    ])
    .unwrap()
    .args;
    assert_eq!(args.name, "web");
    assert_eq!(
        args.command,
        ["pnpm", "start", "--port", "3000"].map(OsString::from)
    );
    assert!(Harness::try_parse_from(["run", "--name", "web"]).is_err());
    let default = Harness::try_parse_from(["run", "--", "pnpm", "start"])
        .unwrap()
        .args;
    assert_eq!(default.name, "app");
    assert_eq!(default.command, ["pnpm", "start"].map(OsString::from));
    assert!(Harness::try_parse_from(["run"]).is_err());
}

#[cfg(unix)]
#[test]
fn pipe_mode_captures_both_streams_returns_exit_and_marks_spawn_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let layout = Layout::with_user_dir(&root, dir.path().join("private")).unwrap();
    let mut capture = Capture::start(&layout, "web").unwrap();
    let mut reader = crate::storage::process_capture::Reader::attach(&layout, "web").unwrap();
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "printf 'stdout-marker\\n'; printf 'stderr-marker\\n' >&2; exit 7",
    ]);
    assert_eq!(unix::execute(&mut command, &mut capture, false).unwrap(), 7);
    capture.finish(None).unwrap();
    let (bytes, closed) = reader.poll(4096).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("stdout-marker") && text.contains("stderr-marker"));
    assert!(closed);
    drop(capture);
    let args = RunArgs {
        name: "missing".into(),
        command: vec!["/dial-nonexistent-executable".into()],
    };
    assert!(run(&layout, &args).is_err());
    assert!(crate::storage::process_capture::Reader::attach(&layout, "missing").is_err());
}

// Execute the actual wrapper pump in an isolated process with its own controlling terminal.
#[cfg(unix)]
#[test]
fn pty_wrapper_child() {
    use std::os::fd::AsRawFd;
    let Some(root) = std::env::var_os("DIAL_TEST_RUN_PTY_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let layout = Layout::with_user_dir(&root.join("project"), root.join("private")).unwrap();
    if std::env::var_os("DIAL_TEST_RUN_PIPE").is_some() {
        let args = RunArgs {
            name: "mixed".into(),
            command: vec![
                "sh".into(),
                "-c".into(),
                "read line; printf 'mixed-received:%s\\n' \"$line\"".into(),
            ],
        };
        assert_eq!(run(&layout, &args).unwrap(), 0);
        return;
    }
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: stdin is the test driver's live controlling PTY; output buffer is valid.
    assert_eq!(
        unsafe { libc::tcgetattr(io::stdin().as_raw_fd(), original.as_mut_ptr()) },
        0
    );
    // SAFETY: successful tcgetattr initialized the value.
    let original = unsafe { original.assume_init() };
    let mut capture = Capture::start(&layout, "interactive").unwrap();
    capture.append(b"history-before-attach\n").unwrap();
    let mut command = Command::new("sh");
    command.args(["-c", r#"trap 'exit 42' INT; printf '\033[32mwrapper-ready\033[0m\n'; read line; stty size; printf 'received:%s\n' "$line"; while :; do sleep 1; done"#]);
    assert_eq!(unix::execute(&mut command, &mut capture, true).unwrap(), 42);
    capture.finish(None).unwrap();
    let mut restored = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: same live PTY and valid output buffer.
    assert_eq!(
        unsafe { libc::tcgetattr(io::stdin().as_raw_fd(), restored.as_mut_ptr()) },
        0
    );
    // SAFETY: successful tcgetattr initialized the value.
    let restored = unsafe { restored.assume_init() };
    assert_eq!(original.c_iflag, restored.c_iflag);
    assert_eq!(original.c_oflag, restored.c_oflag);
    assert_eq!(original.c_cflag, restored.c_cflag);
    assert_eq!(original.c_lflag, restored.c_lflag);
    assert_eq!(original.c_cc, restored.c_cc);
    println!("wrapper-terminal-restored");
}

#[cfg(unix)]
#[test]
fn pty_wrapper_forwards_input_resize_ctrl_c_and_restores_terminal() {
    use crate::platform::terminal;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("project")).unwrap();
    let (mut master, slave) = terminal::open(24, 80).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "cli::run::tests::pty_wrapper_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DIAL_TEST_RUN_PTY_ROOT", dir.path())
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    terminal::controlling_child(&mut command);
    let mut child = process::spawn(&mut command).unwrap();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut bytes = Vec::new();
    let mut until = |needle: &[u8], master: &mut std::fs::File| {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut buffer = [0; 4096];
        while Instant::now() < deadline {
            match master.read(&mut buffer) {
                Ok(0) => return false,
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => return false,
            }
            if bytes.windows(needle.len()).any(|b| b == needle) {
                return true;
            }
        }
        false
    };
    let ready = until(b"wrapper-ready", &mut master);
    // Reader and wrapper are distinct OS processes: exercise the actual lifetime flock
    // and generation protocol, not only independent objects in the same process.
    let layout =
        Layout::with_user_dir(&dir.path().join("project"), dir.path().join("private")).unwrap();
    let first = crate::storage::process_capture::Reader::attach(&layout, "interactive");
    let second = crate::storage::process_capture::Reader::attach(&layout, "interactive");
    let resized = terminal::resize(&master, 31, 99).is_ok();
    // Give the wrapper's bounded resize poll time to update the child's PTY before stty runs.
    std::thread::sleep(Duration::from_millis(150));
    let sent = master.write_all(b"hello\n").is_ok();
    let received = until(b"received:hello", &mut master);
    let interrupted = master.write_all(&[3]).is_ok();
    let restored = until(b"wrapper-terminal-restored", &mut master);
    let exited = process::wait_exited(&child, Duration::from_secs(5)).unwrap();
    let _ = process::kill_group(child.id());
    drop(master);
    let status = child.wait().unwrap();
    assert!(
        ready
            && resized
            && sent
            && received
            && interrupted
            && restored
            && exited
            && status.success(),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(bytes.windows(5).any(|b| b == b"31 99"));
    assert!(bytes.windows(5).any(|b| b == b"\x1b[32m"));
    let (first_bytes, first_closed) = first.unwrap().poll(4096).unwrap();
    let (second_bytes, second_closed) = second.unwrap().poll(4096).unwrap();
    assert_eq!(first_bytes, second_bytes);
    assert!(first_closed && second_closed);
    let captured = String::from_utf8_lossy(&first_bytes);
    assert!(captured.contains("received:hello"), "{captured}");
    assert!(!captured.contains("history-before-attach"), "{captured}");
}

#[cfg(unix)]
#[test]
fn redirected_stdout_relays_terminal_stdin_without_sigttin() {
    use crate::platform::terminal;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("project")).unwrap();
    let output = dir.path().join("stdout");
    let (mut master, slave) = terminal::open(24, 80).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "cli::run::tests::pty_wrapper_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DIAL_TEST_RUN_PTY_ROOT", dir.path())
        .env("DIAL_TEST_RUN_PIPE", "1")
        .stdin(slave.try_clone().unwrap())
        .stdout(std::fs::File::create(&output).unwrap())
        .stderr(slave);
    terminal::controlling_child(&mut command);
    let mut child = process::spawn(&mut command).unwrap();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    master.write_all(b"hello\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut exited = false;
    let mut bytes = [0; 4096];
    while Instant::now() < deadline {
        // Drain the outer terminal's cooked echo while waiting for the redirected wrapper.
        let _ = master.read(&mut bytes);
        if process::exited(&child).unwrap() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = process::kill_group(child.id());
    drop(master);
    let status = child.wait().unwrap();
    let output = std::fs::read_to_string(output).unwrap();
    assert!(exited && status.success(), "{status:?}: {output}");
    assert!(output.contains("mixed-received:hello"), "{output}");
}

#[cfg(windows)]
#[test]
fn windows_refuses_before_creating_capture_or_starting_child() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let layout = Layout::with_user_dir(&root, dir.path().join("private")).unwrap();
    let args = RunArgs {
        name: "app".into(),
        command: vec!["cmd".into(), "/C".into(), "exit 0".into()],
    };
    assert!(
        run(&layout, &args)
            .unwrap_err()
            .to_string()
            .contains("only on Unix")
    );
    assert!(!layout.state_dir.join("runs").exists());
}
