use super::*;

#[test]
fn deserialized_invalid_identities_are_never_live() {
    for pid in [i32::MIN, -1, 0, 1] {
        let id: ProcId = serde_json::from_value(serde_json::json!({"pid":pid,"start":1})).unwrap();
        assert!(!id.same_process(), "invalid pid {pid}");
        assert!(!id.alive(), "invalid pid {pid}");
        assert!(!id.signal_group(0), "invalid pid {pid}");
        #[cfg(unix)]
        assert!(!id.signal(0), "invalid pid {pid}");
    }
}

#[cfg(unix)]
#[test]
fn identity_tracks_exit_and_reaping() {
    let mut child =
        crate::platform::process::spawn(std::process::Command::new("/bin/sleep").arg("30"))
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
    // SAFETY: id names our unreaped child; kill takes only scalar arguments.
    unsafe { libc::kill(id.pid, libc::SIGKILL) };
    // Until reaped the child is a zombie, which must not count as alive. Block until it exits
    // without reaping it (WNOWAIT), so the zombie state is what `alive()` observes.
    // SAFETY: siginfo_t contains C scalar/union fields for which all-zero bits are valid.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: info is writable siginfo_t storage and child is still ours and unreaped;
    // WNOWAIT observes its exit without relinquishing that ownership.
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
