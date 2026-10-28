//! Retained pool-slot state. A slot keeps one backend and the conversation its last worker moved out,
//! so consecutive pool items reuse both. Each item still runs on its own bounded thread; that thread
//! locks the slot store for the whole run, so the next item on the slot waits for the previous one.

use crate::agent::workers::subagents::ChildConversation;
use crate::config::types::Config;
use crate::llm::provider::Backend;
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// Work for one item, run with exclusive access to its slot's retained state.
pub(super) type SlotWork = Box<dyn FnOnce(&mut SlotState) + Send>;

/// Logical ownership of one pool slot's retained state, shared by every item routed to that slot.
pub(super) type SlotStore = Arc<Mutex<Option<SlotState>>>;

/// What a slot keeps between items: its backend, the config that backend was built from, and the
/// conversation its last worker moved out.
#[derive(Default)]
pub(super) struct SlotState {
    backend: Option<Backend>,
    /// Serialized config the retained backend was built from; an item with another config never reuses it.
    fingerprint: Option<serde_json::Value>,
    /// Moved out of the finished run, never copied; the next continuation takes it.
    pub(super) carry: Option<ChildConversation>,
}

/// Serialized form of a config. A config that cannot be serialized yields `None`, which never matches,
/// so such an item always rebuilds rather than reusing a backend it cannot prove compatible.
fn fingerprint(role: &Config) -> Option<serde_json::Value> {
    serde_json::to_value(role).ok()
}

impl SlotState {
    /// Whether the retained backend and carried conversation can serve an item with `role`.
    pub(super) fn compatible(&self, role: &Config) -> bool {
        self.backend.is_some()
            && self.fingerprint.is_some()
            && self.fingerprint == fingerprint(role)
    }

    /// The slot's backend for `role`. A matching backend is reused; otherwise the replacement is built and
    /// validated first, and only then does it replace the old backend, which is released. The carried
    /// conversation belongs to the old config, so a rebuild drops it.
    pub(super) fn backend_for(&mut self, role: &Config) -> Result<&mut Backend> {
        if !self.compatible(role) {
            let fresh = crate::agent::make_backend(role)?;
            if let Some(mut previous) = self.backend.replace(fresh) {
                previous.release();
            }
            self.fingerprint = fingerprint(role);
            self.carry = None;
        }
        match self.backend.as_mut() {
            Some(backend) => Ok(backend),
            None => bail!("slot backend unavailable"),
        }
    }

    /// Releases the backend so the next item builds a fresh one from its own role.
    pub(super) fn retire_backend(&mut self) {
        self.fingerprint = None;
        if let Some(mut backend) = self.backend.take() {
            backend.release();
        }
    }
}

impl Drop for SlotState {
    fn drop(&mut self) {
        self.retire_backend();
    }
}

/// Routes one item onto a pool slot. `continuation` is the assignment that continues the slot's
/// carried conversation when one exists; otherwise the item starts fresh on the same slot.
pub(super) struct SlotRoute {
    pub(super) store: SlotStore,
    pub(super) continuation: Option<String>,
}

/// Runs `work` on a bounded thread against `store`. The thread holds the store lock until `work`
/// returns, so an item never observes a slot state its predecessor is still writing.
pub(super) fn run_on(
    store: SlotStore,
    work: SlotWork,
    name: String,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name).spawn(move || {
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        work(guard.get_or_insert_with(SlotState::default));
    })
}
