use super::*;
use std::sync::{Arc, atomic::AtomicUsize};

#[test]
fn a_limit_bounds_concurrent_holders_and_releases_on_drop() {
    let key = "test-provider-limit-2".to_owned();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let interrupt = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        for _ in 0..6 {
            let (key, active, peak, interrupt) =
                (key.clone(), active.clone(), peak.clone(), interrupt.clone());
            scope.spawn(move || {
                let slot = acquire_key(key, 2, &interrupt).unwrap();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                active.fetch_sub(1, Ordering::SeqCst);
                drop(slot);
            });
        }
    });
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[test]
fn interruption_abandons_the_wait() {
    let key = "test-provider-interrupt".to_owned();
    let held = acquire_key(key.clone(), 1, &AtomicBool::new(false)).unwrap();
    let interrupt = AtomicBool::new(true);
    assert!(acquire_key(key, 1, &interrupt).is_none());
    drop(held);
    assert!(acquire_key("unlimited".into(), 0, &AtomicBool::new(false)).is_some());
}

fn endpoint_config(base_url: &str) -> Config {
    let mut config = Config {
        provider: Some("ollama".into()),
        model: "qwen3:8b".into(),
        ..Default::default()
    };
    config.endpoint = Some(crate::config::types::EndpointConfig {
        headers: None,
        base_url: base_url.into(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        account_id: None,
    });
    config
}

#[test]
fn local_endpoints_take_one_request_at_a_time_unless_configured() {
    for local in [
        "http://127.0.0.1:11434",
        "http://localhost:1234/v1",
        "http://192.168.1.20:8080/",
    ] {
        let (key, limit) = route(&endpoint_config(local));
        assert_eq!(limit, 1, "{local}");
        assert!(key.starts_with("local:"), "{local}");
    }
    let mut raised = endpoint_config("http://127.0.0.1:8080");
    raised.task.provider_concurrency.insert("ollama".into(), 4);
    assert_eq!(route(&raised).1, 4);
    let (key, limit) = route(&endpoint_config("https://api.example.com/v1"));
    assert_eq!((key.as_str(), limit), ("ollama", 0));
}

#[test]
fn explicitly_limited_remote_provider_announces_its_queue_once() {
    let mut config = endpoint_config("https://api.example.com/v1");
    config.task.provider_concurrency.insert("ollama".into(), 1);
    let (key, limit) = route(&config);
    assert_eq!((key.as_str(), limit), ("ollama", 1));
    let held = acquire_key(key.clone(), limit, &AtomicBool::new(false)).unwrap();
    let notices = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (queued_tx, queued_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (config, notices) = (&config, &notices);
        scope.spawn(move || {
            let slot = acquire_checked(
                config,
                || Ok::<(), ()>(()),
                || {
                    notices.fetch_add(1, Ordering::SeqCst);
                    queued_tx.send(()).unwrap();
                },
            );
            done_tx.send(slot.is_ok()).unwrap();
        });
        queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    });
    assert_eq!(notices.load(Ordering::SeqCst), 1);
    assert!(try_acquire_key(key, limit).is_some());
}

#[test]
fn cancellation_hands_capacity_to_the_next_waiter_even_if_it_races_a_grant() {
    for cancel_after_grant in [false, true] {
        let key = format!("test-provider-cancel-handoff-{cancel_after_grant}");
        let interrupt = AtomicBool::new(false);
        let mut held = try_acquire_key(key.clone(), 1);
        assert!(held.is_some());
        std::thread::scope(|scope| {
            let (queued_tx, queued_rx) = mpsc::channel();
            let (resume, paused) = mpsc::channel();
            let waiter_key = key.clone();
            let cancel = &interrupt;
            let waiter = scope.spawn(move || {
                acquire_key_checked_notify(
                    waiter_key,
                    1,
                    || {
                        if cancel.load(Ordering::SeqCst) {
                            Err("operation cancelled")
                        } else {
                            Ok(())
                        }
                    },
                    || {
                        queued_tx.send(()).unwrap();
                        paused.recv_timeout(Duration::from_secs(2)).unwrap();
                    },
                )
                .err()
            });
            queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let (queued_tx, queued_rx) = mpsc::channel();
            let next_key = key.clone();
            let next = scope.spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                acquire_key_checked_notify(
                    next_key,
                    1,
                    || {
                        (std::time::Instant::now() < deadline)
                            .then_some(())
                            .ok_or(())
                    },
                    || queued_tx.send(()).unwrap(),
                )
                .ok()
            });
            queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if cancel_after_grant {
                drop(held.take());
            }
            interrupt.store(true, Ordering::SeqCst);
            resume.send(()).unwrap();
            assert_eq!(waiter.join().unwrap(), Some("operation cancelled"));
            drop(held.take());
            let granted = next
                .join()
                .unwrap()
                .expect("cancelled waiter blocked its successor");
            assert!(try_acquire_key(key.clone(), 1).is_none());
            drop(granted);
            assert!(try_acquire_key(key.clone(), 1).is_some());
        });
    }
}

#[test]
fn queued_deadline_exits_before_holder_releases() {
    let key = "test-provider-checked-deadline".to_owned();
    let held = acquire_key(key.clone(), 1, &AtomicBool::new(false)).unwrap();
    std::thread::scope(|scope| {
        let (queued_tx, queued_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let key = key.clone();
        scope.spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            let result = acquire_key_checked(key, 1, || {
                queued_tx.send(()).unwrap();
                if std::time::Instant::now() >= deadline {
                    Err("operation expired")
                } else {
                    Ok(())
                }
            });
            done_tx.send(result.err()).unwrap();
        });
        queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        drop(held);
        assert_eq!(result.unwrap(), Some("operation expired"));
    });
    assert!(try_acquire_key(key, 1).is_some());
}

#[test]
fn checked_wait_wakes_normally_when_holder_releases() {
    let key = "test-provider-checked-wake".to_owned();
    let held = acquire_key(key.clone(), 1, &AtomicBool::new(false)).unwrap();
    std::thread::scope(|scope| {
        let (queued_tx, queued_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let result = acquire_key_checked(key, 1, || {
                queued_tx.send(()).unwrap();
                Ok::<_, ()>(())
            });
            done_tx.send(result.is_ok()).unwrap();
        });
        queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(held);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    });
}

#[test]
fn checked_admission_rejects_cancelled_free_and_unlimited_routes() {
    for limit in [0, 1] {
        let key = format!("test-provider-checked-reject-{limit}");
        let result = acquire_key_checked(key.clone(), limit, || Err("cancel cause"));
        assert_eq!(result.err(), Some("cancel cause"));
        assert!(try_acquire_key(key, 1).is_some());
    }
}

#[test]
fn queued_notice_fires_once_only_when_the_slot_is_taken() {
    let key = "test-provider-queued-notice".to_owned();
    let notices = std::sync::atomic::AtomicUsize::new(0);
    let free = acquire_key_checked_notify(
        key.clone(),
        1,
        || Ok::<(), ()>(()),
        || {
            notices.fetch_add(1, Ordering::SeqCst);
        },
    )
    .unwrap();
    assert_eq!(
        notices.load(Ordering::SeqCst),
        0,
        "a free slot must not announce a wait"
    );
    drop(free);

    let held = acquire_key(key.clone(), 1, &AtomicBool::new(false)).unwrap();
    std::thread::scope(|scope| {
        let (queued_tx, queued_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter_key = key.clone();
        let notices = &notices;
        scope.spawn(move || {
            let slot = acquire_key_checked_notify(
                waiter_key,
                1,
                || Ok::<(), ()>(()),
                || {
                    notices.fetch_add(1, Ordering::SeqCst);
                    queued_tx.send(()).unwrap();
                },
            );
            done_tx.send(slot.is_ok()).unwrap();
        });
        queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    });
    assert_eq!(
        notices.load(Ordering::SeqCst),
        1,
        "the wait is announced exactly once"
    );
    assert!(try_acquire_key(key, 1).is_some());
}

#[test]
fn fifo_handoff_cannot_be_overtaken_by_another_waiter_or_a_probe() {
    let key = "test-provider-fifo".to_owned();
    let held = try_acquire_key(key.clone(), 1).unwrap();
    std::thread::scope(|scope| {
        let (grants, acquired) = mpsc::channel();
        let mut resumes = Vec::new();
        for id in 0..3 {
            let key = key.clone();
            let grants = grants.clone();
            let (queued, ready) = mpsc::channel();
            let (resume, paused) = mpsc::channel();
            resumes.push(resume);
            scope.spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                let slot = acquire_key_checked_notify(
                    key,
                    1,
                    || {
                        (std::time::Instant::now() < deadline)
                            .then_some(())
                            .ok_or(())
                    },
                    || {
                        queued.send(()).unwrap();
                        paused.recv_timeout(Duration::from_secs(2)).unwrap();
                    },
                )
                .unwrap();
                grants.send((id, slot)).unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        drop(held);
        // No waiter has returned from its notice yet, but only the oldest owns the grant.
        assert!(try_acquire_key(key.clone(), 1).is_none());
        assert!(try_acquire_key("test-provider-fifo-independent".into(), 1).is_some());
        for resume in resumes.into_iter().rev() {
            resume.send(()).unwrap();
        }
        for expected in 0..3 {
            let (id, slot) = acquired.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(id, expected);
            drop(slot);
        }
        assert!(try_acquire_key(key.clone(), 1).is_some());
    });
}
