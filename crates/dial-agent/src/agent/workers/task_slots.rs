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

/// The calling thread's slot nesting, handed to a thread that runs one descendant while this
/// thread blocks waiting for it. A scoped thread starts with no depth of its own, so without this
/// a serial child of a worker could not run when the project ceiling is already full: its parent
/// holds the only slot and is idle until the child ends. Only hand it to a thread whose parent
/// waits for it alone; concurrent siblings must each take their own slot.
pub(in crate::agent) struct Lineage(usize);

pub(in crate::agent) fn lineage() -> Lineage {
    Lineage(DEPTH.with(Cell::get))
}

impl Lineage {
    /// Borrow the parent's slot on this thread until the guard drops.
    pub(in crate::agent) fn enter(self) -> Borrowed {
        DEPTH.with(|depth| depth.set(depth.get() + self.0));
        Borrowed {
            depth: self.0,
            _thread_bound: std::marker::PhantomData,
        }
    }
}

pub(in crate::agent) struct Borrowed {
    depth: usize,
    _thread_bound: std::marker::PhantomData<*const ()>,
}

impl Drop for Borrowed {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(self.depth)));
    }
}

#[cfg(test)]
mod tests;
