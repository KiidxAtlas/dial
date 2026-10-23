//! Per-provider ceilings on in-flight subagent model requests.
//!
//! A slot brackets one model request only, never a child's whole run: a parent releases its slot
//! as soon as its response has streamed, so children it starts during tool execution can take
//! slots for their own requests. Holding slots across whole runs would deadlock any spawn tree
//! wider than the limit (parents waiting on children waiting on the parents' slots).
//!
//! Remote providers are unlimited unless `task.provider_concurrency.<provider>` sets a cap. A local
//! model server is one shared resource. Dial's own runtime always takes one request at a time; an
//! endpoint on this machine or the LAN does too unless `task.provider_concurrency.<provider>`
//! raises that, e.g. for a llama.cpp server started with several `--parallel` slots. Parallel
//! requests on a single-slot server would also evict each other's prompt cache.
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
pub(in crate::agent) struct Slot {
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
pub(in crate::agent) fn acquire(config: &Config, interrupt: &AtomicBool) -> Option<Slot> {
    let (key, limit) = route(config);
    acquire_key(key, limit, interrupt)
}

/// Wait for a request slot while preserving the caller's admission failure cause.
pub(in crate::agent) fn acquire_checked<E>(
    config: &Config,
    check: impl FnMut() -> Result<(), E>,
) -> Result<Slot, E> {
    let (key, limit) = route(config);
    acquire_key_checked(key, limit, check)
}

/// Nonblocking admission for whole workers. A controller waiting for a worker must
/// remain free to service approvals from already-running background workers.
pub(in crate::agent) fn try_acquire_key(key: String, limit: usize) -> Option<Slot> {
    let slots = slots();
    let mut used = slots.used.lock().unwrap_or_else(|e| e.into_inner());
    let count = used.entry(key.clone()).or_insert(0);
    if *count >= limit {
        return None;
    }
    *count += 1;
    Some(Slot { key: Some(key) })
}

pub(in crate::agent) fn acquire_key(
    key: String,
    limit: usize,
    interrupt: &AtomicBool,
) -> Option<Slot> {
    acquire_key_checked(key, limit, || {
        if interrupt.load(Ordering::SeqCst) {
            Err(())
        } else {
            Ok(())
        }
    })
    .ok()
}

fn acquire_key_checked<E>(
    key: String,
    limit: usize,
    mut check: impl FnMut() -> Result<(), E>,
) -> Result<Slot, E> {
    if limit == 0 {
        check()?;
        return Ok(Slot { key: None });
    }
    let slots = slots();
    let mut used = slots.used.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        check()?;
        let count = used.entry(key.clone()).or_insert(0);
        if *count < limit {
            *count += 1;
            return Ok(Slot { key: Some(key) });
        }
        used = slots
            .freed
            .wait_timeout(used, Duration::from_millis(50))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests;
