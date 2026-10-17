use super::*;

#[cfg(unix)]
#[test]
fn identity_tracks_exit_and_reaping() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let id = ProcId::of(child.id() as i32).expect("live child identity");
    assert!(id.alive());
    assert!(
        !ProcId {
            start: id.start.wrapping_add(1),
            ..id
        }
        .alive(),
        "a different start time is a different process"
    );
    unsafe { libc::kill(id.pid, libc::SIGKILL) };
    // Until reaped the child is a zombie, which must not count as alive. Block until it exits
    // without reaping it (WNOWAIT), so the zombie state is what `alive()` observes.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    assert_eq!(rc, 0, "waiting for the killed child to exit");
    assert!(!id.alive());
    child.wait().unwrap();
    assert!(!id.alive());
    assert!(
        !id.signal(libc::SIGTERM),
        "a dead identity is never signalled"
    );
    assert!(ProcId::current().is_some_and(|me| me.alive()));
    assert!(ProcId::of(1).is_none());
}

#[cfg(windows)]
#[test]
fn windows_group_kill_requires_matching_identity() {
    let mut child = std::process::Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let id = ProcId::of(child.id() as i32).expect("live child identity");
    let wrong = ProcId {
        start: id.start.wrapping_add(1),
        ..id
    };
    assert!(
        !wrong.signal_group(SIGKILL),
        "a different start time is never killed"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "a mismatched identity must not kill the process"
    );
    assert!(id.signal_group(SIGKILL));
    child.wait().unwrap();
    assert!(
        !id.signal_group(SIGKILL),
        "a dead identity is never signalled"
    );
}
