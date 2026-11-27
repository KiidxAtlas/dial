use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn ordinary_spawn_waits_for_pipe_creation_guard() {
    let guard = SPAWN.lock().unwrap();
    let (starting, started) = mpsc::channel();
    let (finished, completed) = mpsc::channel();
    let spawner = std::thread::spawn(move || {
        starting.send(()).unwrap();
        let mut child = spawn(
            Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .stdout(std::process::Stdio::piped()),
        )
        .unwrap();
        let status = child.wait().unwrap();
        finished.send(status.success()).unwrap();
    });
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    let escaped = completed.recv_timeout(Duration::from_millis(100));
    drop(guard);
    // Always release the guard and join before asserting, even when the spawn escaped it.
    spawner.join().unwrap();
    assert!(
        matches!(escaped, Err(mpsc::RecvTimeoutError::Timeout)),
        "ordinary spawn must not create pipes while another spawn owns the guard"
    );
    assert!(completed.recv_timeout(Duration::from_secs(5)).unwrap());
}

#[test]
fn spawn_error_preserves_the_callers_command_and_later_spawns_work() {
    let mut command = Command::new("/definitely-not-a-dial-executable");
    command
        .args(["first", "second"])
        .env("DIAL_TEST_SPAWN", "preserve");
    assert!(spawn(&mut command).is_err());
    assert_eq!(command.get_program(), "/definitely-not-a-dial-executable");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["first", "second"]);
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "DIAL_TEST_SPAWN"
                && value == Some(std::ffi::OsStr::new("preserve")))
    );
    assert!(
        spawn(Command::new("/bin/sh").args(["-c", "exit 0"]))
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn children_outlive_the_short_lived_thread_that_spawned_them() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 30"]);
    // The same parent-death signal bubblewrap's `--die-with-parent` arms in the sandbox.
    // SAFETY: prctl is async-signal-safe and touches only the forked child.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (command, mut child) = std::thread::spawn(move || {
        let child = spawn(&mut command).unwrap();
        (command, child)
    })
    .join()
    .unwrap();
    // The requesting thread has exited; a signal tied to it would have killed the child.
    let exited = wait_exited(&child, Duration::from_millis(500)).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        !exited,
        "a child must not die with the tool thread that requested it"
    );
    assert_eq!(
        command.get_program(),
        "/bin/sh",
        "the caller keeps its command"
    );
}

#[test]
fn spawn_group_limited_applies_rlimits_in_child() {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "ulimit -n; ulimit -f"])
        .stdout(std::process::Stdio::piped());
    let limits = crate::platform::process::ResourceLimits {
        max_open_files: Some(64),
        max_file_size_bytes: Some(1024 * 1024),
        ..Default::default()
    };
    let child = spawn_group_limited(&mut command, limits).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let nofile: u64 = lines.next().unwrap_or("").trim().parse().unwrap();
    assert!(nofile <= 64, "expected RLIMIT_NOFILE <= 64, got {nofile}");
    // POSIX `ulimit -f` reports 512-byte blocks (1 MiB == 2048 blocks).
    let fsize: u64 = lines.next().unwrap_or("").trim().parse().unwrap();
    assert!(fsize <= 2048, "expected RLIMIT_FSIZE <= 2048 blocks, got {fsize}");
}
