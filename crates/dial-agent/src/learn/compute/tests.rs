use super::*;

#[test]
fn request_and_role_cancellation_both_reach_generation_without_clobbering_each_other() {
    use std::sync::Arc;
    for accounted in [false, true] {
        for source in ["request", "role"] {
            let root = tempfile::tempdir().unwrap();
            let config = Config {
                user_dir: root.path().join("user"),
                state_dir: root.path().join("state"),
                compute_locks: false,
                ..Config::default()
            };
            let _lease = accounted.then(|| Background::acquire(&config).unwrap());
            if !accounted {
                // Linking foreground flags must not turn the call into background work or
                // attempt a lease handoff merely because background work is paused.
                std::fs::create_dir_all(&config.user_dir).unwrap();
                std::fs::write(config.user_dir.join("background.pause"), "paused").unwrap();
            }
            let request = Arc::new(AtomicBool::new(false));
            let role = Arc::new(AtomicBool::new(false));
            let mut backend = crate::llm::provider::Backend::new(
                crate::llm::provider::ProviderKind::Endpoint {
                    base_url: "http://127.0.0.1:1".into(),
                    model: "fixture".into(),
                    api_key: None,
                },
                std::path::PathBuf::new(),
                5,
            );
            let error = with_cancellation(request.clone(), || {
                background_call_controlled(
                    &config,
                    &mut backend,
                    Some(&role),
                    |_, cancel| -> Result<()> {
                        let (selected, unrelated) = if source == "request" {
                            (&request, &role)
                        } else {
                            (&role, &request)
                        };
                        selected.store(true, Ordering::SeqCst);
                        let deadline = std::time::Instant::now() + Duration::from_secs(2);
                        while !cancel.load(Ordering::SeqCst) {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "{source} cancellation was lost; accounted={accounted}"
                            );
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        assert!(
                            !unrelated.load(Ordering::SeqCst),
                            "{source} cancellation clobbered its sibling flag"
                        );
                        Err(crate::llm::Interrupted.into())
                    },
                )
            })
            .unwrap_err();
            assert!(
                error.is::<crate::llm::Interrupted>(),
                "{source}, accounted={accounted}: {error:#}"
            );
            assert_eq!(request.load(Ordering::SeqCst), source == "request");
            assert_eq!(role.load(Ordering::SeqCst), source == "role");
            assert!(
                check_cancelled().is_ok(),
                "request scope leaked to its caller"
            );
        }
    }
}

#[test]
fn request_cancellation_is_scoped_and_reaches_existing_generation() {
    use std::sync::Arc;
    let config = Config::default();
    let flag = Arc::new(AtomicBool::new(false));
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    with_cancellation(flag.clone(), || {
        background_call(&config, &mut backend, |_, cancel| {
            assert!(Arc::ptr_eq(&flag, &cancel));
            flag.store(true, Ordering::SeqCst);
            assert!(
                super::super::auto::check_stop(&config)
                    .unwrap_err()
                    .is::<crate::llm::Interrupted>()
            );
            Ok(())
        })?;
        let nested = with_cancellation(Arc::new(AtomicBool::new(false)), check_cancelled);
        assert!(
            nested.is_ok(),
            "nested request has independent cancellation"
        );
        assert!(check_cancelled().is_err(), "outer request flag is restored");
        Ok(())
    })
    .unwrap();
    assert!(
        check_cancelled().is_ok(),
        "request flag must not leak to the next request"
    );
    assert!(
        with_cancellation(flag, || -> Result<()> {
            panic!("already-cancelled work must not run")
        })
        .is_err()
    );
    assert!(check_cancelled().is_ok());
}
#[test]
fn background_generation_is_retried_after_foreground_handoff_without_scoring_a_partial_reply() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        ..Config::default()
    };
    let _lease = Background::acquire(&config).unwrap();
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let mut calls = 0;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            let request = Request::foreground(&config.user_dir).unwrap();
            let lock =
                super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
            lock.lock().unwrap();
            drop(request);
            std::thread::sleep(Duration::from_millis(100));
            std::fs::File::unlock(&lock).unwrap();
        });
        let reply = background_call(&config, &mut backend, |_, cancel| {
            calls += 1;
            if calls == 1 {
                let start = std::time::Instant::now();
                while !cancel.load(Ordering::SeqCst) {
                    assert!(start.elapsed() < Duration::from_secs(2));
                    std::thread::sleep(Duration::from_millis(5));
                }
                return Err(crate::llm::Interrupted.into());
            }
            Ok("complete retried reply")
        })
        .unwrap();
        assert_eq!(reply, "complete retried reply");
    });
    assert_eq!(calls, 2);
    assert!(can_yield());
}
#[test]
fn requests_are_live_and_crashed_markers_do_not_block() {
    let home = tempfile::tempdir().unwrap();
    let request = Request::foreground(home.path()).unwrap();
    assert!(foreground_pending(home.path()).unwrap());
    drop(request);
    assert!(!foreground_pending(home.path()).unwrap());
    std::fs::write(home.path().join("foreground-compute/stale"), "").unwrap();
    assert!(!foreground_pending(home.path()).unwrap());
}

#[test]
fn stop_interrupts_a_paused_training_wait_and_releases_its_lease() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let lease = Background::acquire(&config).unwrap();
    let _foreground = Request::foreground(&config.user_dir).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
        });
        assert!(
            Background::yield_to_coding(&config)
                .unwrap_err()
                .to_string()
                .contains("stopped")
        );
    });
    assert!(!can_yield());
    drop(lease);
    let lock = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
    lock.try_lock().map_err(std::io::Error::from).unwrap();
}

#[test]
fn lease_clock_distinguishes_midnight_from_wall_clock_corrections() {
    let midnight = 86_400 * 100;
    // start wall time, monotonic elapsed, sample wall time, current-day compute
    for (start, elapsed, at, expected) in [
        (midnight - 121, 120, midnight - 1, 120),
        (midnight - 120, 120, midnight, 0),
        (midnight - 90, 120, midnight + 30, 30),
        (midnight + 120, 120, midnight + 60, 120), // same-day backward correction
        (midnight + 120, 120, midnight + 2000, 120), // same-day forward jump
        (midnight - 30, 10, midnight + 300, 10),   // forward day jump cannot add compute
        (midnight + 120, 120, midnight - 30, 120), // reversed day fails conservative
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            user_dir: root.path().to_path_buf(),
            ..Default::default()
        };
        config.improve.daily_compute_minutes = 1;
        LEASE_DAY_CLOCK.with(|clock| {
            *clock.borrow_mut() = Some(std::sync::Arc::new(std::sync::Mutex::new(
                super::super::resources::UsageClock::new(start),
            )))
        });
        LEASED_SINCE.with(|since| {
            *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(elapsed));
        });
        let counted = lease_elapsed_today_secs_at(at);
        assert_eq!(counted, expected, "start={start}, at={at}");
        assert_eq!(
            super::super::auto::check_compute_budget_at(&config, counted, at).is_err(),
            expected >= 60
        );
        settle_usage_at(root.path(), at).unwrap();
        assert_eq!(
            super::super::resources::used_at_checked(root.path(), at).unwrap(),
            expected
        );
    }
}

#[test]
fn clock_correction_after_observed_midnight_keeps_monotonic_today_usage() {
    let midnight = 86_400 * 100;
    let mut clock = super::super::resources::UsageClock::new(midnight - 90);
    for (elapsed, at, expected) in [
        (120, midnight + 30, 30),
        (130, midnight + 5, 40),
        (140, midnight + 2000, 50),
        (150, midnight + 10, 60),
        (160, midnight - 10, 160),
        (170, midnight + 20, 80),
    ] {
        assert_eq!(clock.sample(elapsed, at), expected);
    }
}

#[test]
fn failed_settlement_survives_guard_teardown_and_blocks_new_admission_until_retried() {
    for compute_locks in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            user_dir: root.path().join("user"),
            state_dir: root.path().join("state"),
            compute_locks,
            ..Default::default()
        };
        let guard = Background::acquire(&config).unwrap();
        LEASED_SINCE.with(|since| {
            *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(120))
        });
        std::fs::create_dir_all(&config.user_dir).unwrap();
        let ledger = crate::config::paths::compute_usage_file(&config.user_dir);
        std::fs::write(&ledger, "corrupt").unwrap();
        assert!(Background::yield_to_coding(&config).is_err());
        assert!(
            !background_active(),
            "failed debt must not include time after work stopped"
        );
        drop(guard);
        let next = config.clone();
        std::thread::spawn(move || assert!(Background::acquire(&next).is_err()))
            .join()
            .unwrap();
        std::fs::remove_file(ledger).unwrap();
        config.improve.daily_compute_minutes = 1;
        assert!(
            Background::acquire(&config)
                .err()
                .unwrap()
                .is::<super::super::auto::BudgetReached>()
        );
        assert_eq!(super::super::resources::used_today(&config.user_dir), 120);
        config.improve.daily_compute_minutes = 0;
        let guard = Background::acquire(&config).unwrap();
        assert_eq!(super::super::resources::used_today(&config.user_dir), 120);
        drop(guard);
        assert_eq!(super::super::resources::used_today(&config.user_dir), 120);
    }
}

#[test]
fn unlimited_active_compute_ignores_corrupt_budget_ledger_but_honors_controls() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        compute_locks: false,
        ..Default::default()
    };
    let guard = Background::acquire(&config).unwrap();
    std::fs::create_dir_all(&config.user_dir).unwrap();
    let ledger = crate::config::paths::compute_usage_file(&config.user_dir);
    std::fs::write(&ledger, "corrupt").unwrap();
    super::super::auto::check_stop(&config).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(true));
    assert!(with_cancellation(cancel, || super::super::auto::check_stop(&config)).is_err());
    std::fs::write(config.user_dir.join("background.pause"), "").unwrap();
    assert!(should_yield(&config.user_dir).unwrap());
    std::fs::create_dir_all(&config.state_dir).unwrap();
    std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
    assert!(super::super::auto::check_stop(&config).is_err());
    std::fs::remove_file(ledger).unwrap();
    drop(guard);
}
