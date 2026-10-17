use super::*;

/// Every portal caller (libei input init and PipeWire capture) must borrow
/// one persistent runtime; a regression to per-call runtimes would return
/// distinct instances and re-open the orphaned-connection bug (#7886).
#[test]
fn portal_runtime_is_shared_across_calls() {
    let first = portal_runtime().expect("portal runtime builds");
    let second = portal_runtime().expect("portal runtime builds");
    assert!(
        std::ptr::eq(first, second),
        "portal_runtime must hand back one long-lived runtime, not a fresh per-call instance"
    );
}

/// The orphaned RemoteDesktop token written by pre-#7884 builds must be
/// removed, and a second removal on the now-missing file must stay a no-op.
#[test]
fn removes_orphaned_remote_desktop_token() {
    let dir = std::env::temp_dir().join(format!("dial-token-test-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create token test dir");
    let token = dir.join(ORPHANED_REMOTE_DESKTOP_TOKEN);
    fs::write(&token, "cafef00d").expect("plant orphaned token");
    remove_token_in(&dir);
    assert!(!token.exists(), "orphaned token must be removed");
    remove_token_in(&dir);
    let _ = fs::remove_dir_all(&dir);
}
