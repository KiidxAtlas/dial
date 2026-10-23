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
fn queued_cancellation_exits_before_release_without_leaking_capacity() {
    let key = "test-provider-checked-cancel".to_owned();
    let held = acquire_key(key.clone(), 1, &AtomicBool::new(false)).unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (queued_tx, queued_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter_key = key.clone();
        let cancelled = &cancelled;
        scope.spawn(move || {
            let result = acquire_key_checked(waiter_key, 1, || {
                queued_tx.send(()).unwrap();
                if cancelled.load(Ordering::SeqCst) {
                    Err("operation cancelled")
                } else {
                    Ok(())
                }
            });
            done_tx.send(result.err()).unwrap();
        });
        queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        cancelled.store(true, Ordering::SeqCst);
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        // Always release the holder even on failure, so the scoped thread can exit.
        assert!(try_acquire_key(key.clone(), 1).is_none());
        drop(held);
        assert_eq!(result.unwrap(), Some("operation cancelled"));
    });
    let next = try_acquire_key(key.clone(), 1).expect("cancelled admission leaked capacity");
    assert!(try_acquire_key(key.clone(), 1).is_none());
    drop(next);
    assert!(try_acquire_key(key, 1).is_some());
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
