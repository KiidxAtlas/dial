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
    collections::{HashMap, VecDeque},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::Duration,
};

/// Shown once in a worker's status when its request waits for a provider slot. Covers every route
/// with a concurrency limit: a local server, or a remote provider capped by `provider_concurrency`.
pub(in crate::agent) const QUEUED_NOTICE: &str =
    "Waiting for a model request slot: this provider is at its concurrency limit.";

struct Waiter {
    ticket: u64,
    limit: usize,
    granted: Sender<()>,
}

#[derive(Default)]
struct RouteSlots {
    used: usize,
    next_ticket: u64,
    waiting: VecDeque<Waiter>,
}

impl RouteSlots {
    fn try_reserve(&mut self, limit: usize) -> bool {
        if self.used >= limit || !self.waiting.is_empty() {
            return false;
        }
        self.used += 1;
        true
    }

    fn enqueue(&mut self, limit: usize) -> (u64, Receiver<()>) {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let (granted, receiver) = mpsc::channel();
        self.waiting.push_back(Waiter {
            ticket,
            limit,
            granted,
        });
        (ticket, receiver)
    }

    fn release(&mut self, ticket: Option<u64>) {
        if let Some(i) = ticket.and_then(|id| self.waiting.iter().position(|w| w.ticket == id)) {
            self.waiting.remove(i); // Cancelled before the grant.
        } else {
            self.used -= 1; // Held, or cancelled after the grant raced the check.
        }
        // Reserve *before* waking: a fast try-acquire cannot steal a slow waiter's turn.
        while let Some(head) = self.waiting.front() {
            if self.used >= head.limit {
                break;
            }
            let waiter = self.waiting.pop_front().expect("observed the head");
            self.used += 1;
            // Even if the receiver just unwound, its Slot guard will release this grant.
            let _ = waiter.granted.send(());
        }
    }
}

// The registry protects only short queue transitions. Waiting, periodic cancellation checks,
// and user callbacks never hold it. Only the newly admitted requests are signalled.
static SLOTS: LazyLock<Mutex<HashMap<String, RouteSlots>>> = LazyLock::new(Mutex::default);

/// A held (or pending) request slot; released on drop, including callback unwinding.
pub(in crate::agent) struct Slot {
    key: Option<String>,
    ticket: Option<u64>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        let mut routes = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let route = routes.get_mut(&key).expect("a slot owns a route entry");
        route.release(self.ticket);
        if route.used == 0 {
            debug_assert!(route.waiting.is_empty());
            routes.remove(&key);
        }
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

/// Best-effort control-plane work must never queue behind busy inference.
pub(in crate::agent) fn try_acquire(config: &Config) -> Option<Slot> {
    let (key, limit) = route(config);
    if limit == 0 {
        Some(Slot {
            key: None,
            ticket: None,
        })
    } else {
        try_acquire_key(key, limit)
    }
}

/// Wait for a request slot while preserving the caller's admission failure cause. `on_queued`
/// runs once, when this request first finds the slot taken, so the caller can show the wait.
pub(in crate::agent) fn acquire_checked<E>(
    config: &Config,
    check: impl FnMut() -> Result<(), E>,
    on_queued: impl FnOnce(),
) -> Result<Slot, E> {
    let (key, limit) = route(config);
    acquire_key_checked_notify(key, limit, check, on_queued)
}

/// Nonblocking admission for whole workers. A controller waiting for a worker must
/// remain free to service approvals from already-running background workers.
pub(in crate::agent) fn try_acquire_key(key: String, limit: usize) -> Option<Slot> {
    if limit == 0 {
        return None;
    }
    let mut routes = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    let route = routes.entry(key.clone()).or_default();
    if !route.try_reserve(limit) {
        return None;
    }
    Some(Slot {
        key: Some(key),
        ticket: None,
    })
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
    check: impl FnMut() -> Result<(), E>,
) -> Result<Slot, E> {
    acquire_key_checked_notify(key, limit, check, || {})
}

fn acquire_key_checked_notify<E>(
    key: String,
    limit: usize,
    mut check: impl FnMut() -> Result<(), E>,
    on_queued: impl FnOnce(),
) -> Result<Slot, E> {
    check()?;
    if limit == 0 {
        return Ok(Slot {
            key: None,
            ticket: None,
        });
    }
    let mut routes = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    let route = routes.entry(key.clone()).or_default();
    if route.try_reserve(limit) {
        return Ok(Slot {
            key: Some(key),
            ticket: None,
        });
    }
    let (ticket, granted) = route.enqueue(limit);
    let mut slot = Slot {
        key: Some(key),
        ticket: Some(ticket),
    };
    drop(routes);
    on_queued();
    loop {
        check()?;
        match granted.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => {
                check()?;
                slot.ticket = None;
                return Ok(slot);
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => unreachable!("registered waiter lost its grant"),
        }
    }
}

#[cfg(test)]
mod tests;
