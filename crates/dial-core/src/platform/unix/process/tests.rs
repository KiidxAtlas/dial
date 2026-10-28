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
