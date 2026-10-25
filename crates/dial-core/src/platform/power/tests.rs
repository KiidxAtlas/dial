use super::IdleSleepGuard;

#[test]
fn guard_can_move_between_worker_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<IdleSleepGuard>();
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    assert!(std::mem::needs_drop::<IdleSleepGuard>());
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates real macOS power assertions; requires a running power-management service"]
fn native_assertions_are_independent_and_released_on_drop() {
    let first = IdleSleepGuard::acquire().expect("first native assertion");
    let second = IdleSleepGuard::acquire().expect("second native assertion");
    let first_id = first._request.id();
    let second_id = second._request.id();
    assert_ne!(first_id, second_id);
    assert!(super::imp::assertion_exists(first_id));
    assert!(super::imp::assertion_exists(second_id));
    // Release on another thread: requests must not depend on the acquiring thread.
    std::thread::spawn(move || drop(first))
        .join()
        .expect("release thread");
    assert!(!super::imp::assertion_exists(first_id));
    assert!(super::imp::assertion_exists(second_id));
    drop(second);
    assert!(!super::imp::assertion_exists(second_id));
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates a real macOS power assertion; requires a running power-management service"]
fn native_assertion_is_released_during_unwind() {
    let guard = IdleSleepGuard::acquire().expect("native assertion");
    let id = guard._request.id();
    let result = std::panic::catch_unwind(move || {
        let _guard = guard;
        panic!("exercise RAII unwind cleanup");
    });
    assert!(result.is_err());
    assert!(!super::imp::assertion_exists(id));
}
