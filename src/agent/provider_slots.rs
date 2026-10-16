//! Per-provider ceilings on in-flight subagent model requests.
//!
//! A slot brackets one model request only, never a child's whole run: a parent releases its slot
//! as soon as its response has streamed, so children it starts during tool execution can take
//! slots for their own requests. Holding slots across whole runs would deadlock any spawn tree
//! wider than the limit (parents waiting on children waiting on the parents' slots).
//!
//! Remote providers are unlimited unless `task.provider_concurrency.<provider>` sets a cap. A local
//! model server (Dial's own runtime, or an endpoint on this machine or the LAN) is one shared
//! resource: it takes one request at a time unless `task.provider_concurrency.<provider>` raises
//! that, e.g. for a llama.cpp server started with several `--parallel` slots. Parallel requests on
//! a single-slot server would also evict each other's prompt cache.
use crate::config::types::Config;
use std::{
    collections::HashMap,
    sync::{
        Condvar, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Slots {
    used: Mutex<HashMap<String, usize>>,
    freed: Condvar,
}

static SLOTS: LazyLock<Slots> = LazyLock::new(|| Slots {
    used: Mutex::new(HashMap::new()),
    freed: Condvar::new(),
});

fn slots() -> &'static Slots {
    &SLOTS
}

/// A held request slot; released on drop.
pub(super) struct Slot {
    key: Option<String>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        let slots = slots();
        let mut used = slots.used.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = used.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                used.remove(&key);
            }
        }
        drop(used);
        slots.freed.notify_all();
    }
}

/// The slot key and its limit (0 = unlimited) for requests made with `config`.
fn route(config: &Config) -> (String, usize) {
    let Some(endpoint) = &config.endpoint else {
        return (format!("local:{}", config.model), 1);
    };
    let provider = config.selection().provider;
    let configured = config.task.provider_concurrency.get(&provider).copied();
    if config.local_main_model() {
        let server = endpoint.base_url.trim_end_matches('/');
        return (format!("local:{server}"), configured.unwrap_or(1));
    }
    (provider, configured.unwrap_or(0))
}

/// Wait for a request slot, or `None` once `interrupt` is set.
pub(super) fn acquire(config: &Config, interrupt: &AtomicBool) -> Option<Slot> {
    let (key, limit) = route(config);
    acquire_key(key, limit, interrupt)
}

/// Nonblocking admission for whole workers. A controller waiting for a worker must
/// remain free to service approvals from already-running background workers.
pub(super) fn try_acquire_key(key: String, limit: usize) -> Option<Slot> {
    let slots = slots();
    let mut used = slots.used.lock().unwrap_or_else(|e| e.into_inner());
    let count = used.entry(key.clone()).or_insert(0);
    if *count >= limit {
        return None;
    }
    *count += 1;
    Some(Slot { key: Some(key) })
}

pub(super) fn acquire_key(key: String, limit: usize, interrupt: &AtomicBool) -> Option<Slot> {
    if limit == 0 {
        return (!interrupt.load(Ordering::SeqCst)).then_some(Slot { key: None });
    }
    let slots = slots();
    let mut used = slots.used.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return None;
        }
        let count = used.entry(key.clone()).or_insert(0);
        if *count < limit {
            *count += 1;
            return Some(Slot { key: Some(key) });
        }
        used = slots
            .freed
            .wait_timeout(used, Duration::from_millis(50))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests {
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
}
