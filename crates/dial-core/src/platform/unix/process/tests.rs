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
