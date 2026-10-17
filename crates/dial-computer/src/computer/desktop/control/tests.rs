use super::*;

static OWNERSHIP_TEST: Mutex<()> = Mutex::new(());

/// Real ownership without OS event monitoring: lifecycle regressions must
/// not depend on an interactive desktop or Accessibility permissions.
fn grant_for_test(source: &CancellationSource) {
    let owner = ControlLease {
        #[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
        _escape: None,
        _kernel: KernelOwner::acquire().expect("test kernel owner"),
        running: AtomicBool::new(false),
    };
    *source.0.state.lock() = Some(Arc::new(owner));
}

/// Removes this process's private kernel lock (see `KernelLease::path`).
fn remove_test_lock() {
    #[cfg(unix)]
    let _ = std::fs::remove_file(KernelLease::path());
}

#[test]
fn computer_task_lease_survives_retirement_but_abort_and_release_revoke_it() {
    let _serial = OWNERSHIP_TEST.lock();
    let source = CancellationSource::default();
    grant_for_test(&source);
    source
        .acquire_control(&source.token())
        .expect("idempotent acquisition");
    let old = source.token();
    {
        let _operation = InputLease::acquire(&old).expect("reuse task ownership");
        assert!(InputLease::acquire(&source.token()).is_err());
        assert!(InputLease::acquire(&CancellationSource::default().token()).is_err());
    }
    source.retire();
    assert!(source.control_active());
    assert!(old.check().is_err());
    let fresh = source.token();
    let running = InputLease::acquire(&fresh).expect("fresh run reuses ownership");
    source.cancel();
    assert!(!source.control_active());
    assert!(fresh.check().is_err());
    assert!(
        KernelOwner::acquire().is_err(),
        "cleanup still owns the kernel"
    );
    drop(running);
    drop(KernelOwner::acquire().expect("cleanup releases ownership"));
    grant_for_test(&source);
    let queued = source.token();
    source.release_control();
    assert!(!source.control_active());
    assert!(queued.check().is_err());
    assert!(
        source.acquire_control(&queued).is_err(),
        "stale acquisition cannot grant ownership"
    );
    assert!(!source.control_active());
    grant_for_test(&source);
    assert!(
        queued.check().is_err(),
        "reacquiring cannot revive queued input"
    );
    drop((old, fresh, queued));
    drop(source);
    drop(KernelOwner::acquire().expect("session destruction releases ownership"));
    remove_test_lock();
}

#[test]
fn computer_emergency_stop_revokes_idle_task_control_after_retirement() {
    let _serial = OWNERSHIP_TEST.lock();
    let source = CancellationSource::default();
    grant_for_test(&source);
    let emergency = EmergencyStop(Arc::downgrade(&source.0));
    source.retire();
    let fresh = source.token();
    emergency.cancel();
    assert!(!source.control_active());
    assert!(fresh.check().is_err());
    drop(KernelOwner::acquire().expect("idle Escape releases kernel ownership"));
    remove_test_lock();
}

#[test]
fn computer_bounded_button_hold_releases_on_success_cancel_and_partial_press() {
    let mut events = Vec::new();
    bounded_hold(Duration::ZERO, |down| {
        events.push(down);
        Ok(())
    })
    .unwrap();
    assert_eq!(events, [true, false]);

    events.clear();
    let source = CancellationSource::default();
    let token = source.token();
    let result = with_token_for_test(&token, || {
        bounded_hold(Duration::from_secs(100), |down| {
            events.push(down);
            if down {
                source.cancel();
            }
            Ok(())
        })
    });
    assert!(result.is_err());
    assert_eq!(events, [true, false]);
    assert!(token.check().is_err());

    events.clear();
    let result = bounded_hold(Duration::from_secs(100), |down| {
        events.push(down);
        Err(DesktopError::input_failed(if down {
            "partial press"
        } else {
            "release failed"
        }))
    });
    let message = result.unwrap_err().message;
    assert!(message.contains("partial press") && message.contains("release failed"));
    assert_eq!(events, [true, false]);
}

#[test]
fn computer_cancellation_wakes_a_bounded_hold_without_polling() {
    let source = CancellationSource::default();
    let token = source.token();
    let (ready, waiting) = flume::bounded(1);
    let worker = thread::spawn(move || {
        ready.send(()).unwrap();
        token.wait(Duration::from_secs(100))
    });
    waiting.recv().unwrap();
    source.cancel();
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn computer_cancellation_never_revives_queued_generations() {
    let source = CancellationSource::default();
    let running = source.token();
    let queued = source.token();
    source.cancel();
    let later = source.token();
    assert!(running.check().is_err());
    assert!(queued.check().is_err());
    assert!(later.check().is_ok());
    source.cancel();
    assert!(later.check().is_err());
    assert!(source.token().check().is_ok());
    assert!(running.check().is_err());
}

#[test]
fn computer_newer_scope_cannot_revive_an_older_operation() {
    let source = CancellationSource::default();
    let old = source.token();
    let _scope = old.enter();
    source.cancel();
    {
        let _later = source.token().enter();
        assert!(check().is_ok());
    }
    assert!(check().is_err());
}

#[test]
fn computer_cleanup_does_not_clear_cancellation() {
    let source = CancellationSource::default();
    CURRENT.with_borrow_mut(|current| *current = Some(source.token()));
    source.cancel();
    cleanup(|| {
        assert!(check().is_ok());
        cleanup(|| assert!(check().is_ok()));
    });
    assert!(check().is_err());
    CURRENT.with_borrow_mut(|current| *current = None);
}

#[test]
fn computer_process_ownership_fails_closed_and_releases() {
    let _serial = OWNERSHIP_TEST.lock();
    let first = ProcessLease::acquire().expect("first lease");
    assert!(ProcessLease::acquire().is_err());
    drop(first);
    assert!(ProcessLease::acquire().is_ok());
}

#[cfg(unix)]
#[test]
fn computer_kernel_child_probe() {
    let Some(path) = std::env::var_os("DIAL_CONTROL_TEST_LOCK") else {
        return;
    };
    let acquired = KernelLease::at(std::path::Path::new(&path));
    assert_eq!(
        acquired.is_ok(),
        std::env::var_os("DIAL_CONTROL_TEST_FREE").is_some()
    );
    if acquired.is_ok() {
        // Deliberately skip Rust destructors, as on a crashed host.
        std::process::exit(0);
    }
}

#[cfg(unix)]
#[test]
fn computer_independent_processes_contend_and_release() {
    let path = std::env::temp_dir().join(format!(
        "dial-control-process-test-{}.lock",
        std::process::id()
    ));
    let first = KernelLease::at(&path).expect("parent lease");
    let child = |free: bool| {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--exact",
                "computer::desktop::control::tests::computer_kernel_child_probe",
            ])
            .env("DIAL_CONTROL_TEST_LOCK", &path)
            .env_remove("DIAL_CONTROL_TEST_FREE");
        if free {
            command.env("DIAL_CONTROL_TEST_FREE", "1");
        }
        assert!(command.status().expect("child probe").success());
    };
    child(false);
    drop(first);
    child(true);
    let recovered =
        KernelLease::at(&path).expect("kernel releases ownership after abrupt child exit");
    drop(recovered);
    std::fs::remove_file(path).expect("remove process test lock");
}

#[cfg(unix)]
#[test]
fn computer_independent_kernel_handles_contend_and_release() {
    let path = std::env::temp_dir().join(format!("dial-control-test-{}.lock", std::process::id()));
    let first = KernelLease::at(&path).expect("first kernel lease");
    assert!(KernelLease::at(&path).is_err());
    drop(first);
    let second = KernelLease::at(&path).expect("lease after release");
    drop(second);
    std::fs::remove_file(path).expect("remove test lock");
}
