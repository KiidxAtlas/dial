//! Project-wide active worker ceiling, shared by foreground and background launches.
//! Serial descendants borrow their calling worker's slot while that worker waits.
use crate::config::types::Config;
use std::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

thread_local! { static DEPTH: Cell<usize> = const { Cell::new(0) }; }
pub(super) struct Slot {
    _permit: Option<super::provider_slots::Slot>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}
pub(super) fn acquire(config: &Config, interrupt: &AtomicBool) -> Option<Slot> {
    if interrupt.load(Ordering::SeqCst) {
        return None;
    }
    let permit = if DEPTH.with(Cell::get) == 0 {
        Some(super::provider_slots::try_acquire_key(
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
    Some(Slot { _permit: permit })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saturated_project_refuses_new_branches_without_blocking_the_owner() {
        let mut config = Config {
            project_root: "slot-admission-test".into(),
            ..Default::default()
        };
        config.task.max_concurrency = 1;
        let held = acquire(&config, &AtomicBool::new(false)).unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| assert!(acquire(&config, &AtomicBool::new(false)).is_none()))
                .join()
                .unwrap();
        });
        drop(held);
        assert!(acquire(&config, &AtomicBool::new(false)).is_some());
    }
    #[test]
    fn serial_descendants_do_not_deadlock_a_single_slot() {
        let mut config = Config::default();
        config.task.max_concurrency = 1;
        let interrupt = AtomicBool::new(false);
        let parent = acquire(&config, &interrupt).unwrap();
        let child = acquire(&config, &interrupt).unwrap();
        drop(child);
        drop(parent);
        assert_eq!(DEPTH.with(Cell::get), 0);
    }
}
