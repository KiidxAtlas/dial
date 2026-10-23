//! Project-wide active worker ceiling, shared by foreground and background launches.
//! Serial descendants borrow their calling worker's slot while that worker waits.
use crate::config::types::Config;
use std::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

thread_local! { static DEPTH: Cell<usize> = const { Cell::new(0) }; }
/// Tracks this thread's nesting depth, so it must be dropped on the thread that acquired it.
pub(in crate::agent) struct Slot {
    _permit: Option<crate::agent::model::provider_slots::Slot>,
    _thread_bound: std::marker::PhantomData<*const ()>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}
pub(in crate::agent) fn acquire(config: &Config, interrupt: &AtomicBool) -> Option<Slot> {
    if interrupt.load(Ordering::SeqCst) {
        return None;
    }
    let permit = if DEPTH.with(Cell::get) == 0 {
        Some(crate::agent::model::provider_slots::try_acquire_key(
            format!(
                "workers:{}:{}",
                config.state_dir.display(),
                config.project_root.display()
            ),
            config.task.max_concurrency.max(1),
        )?)
    } else {
        None
    };
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    Some(Slot {
        _permit: permit,
        _thread_bound: std::marker::PhantomData,
    })
}

#[cfg(test)]
mod tests;
