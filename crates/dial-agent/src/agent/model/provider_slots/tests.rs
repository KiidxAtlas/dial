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
