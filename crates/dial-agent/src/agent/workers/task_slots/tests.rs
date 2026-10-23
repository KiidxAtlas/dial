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
#[test]
fn a_serial_child_thread_borrows_its_waiting_parents_slot_but_siblings_do_not() {
    let mut config = Config {
        project_root: "slot-lineage-test".into(),
        ..Default::default()
    };
    config.task.max_concurrency = 1;
    let interrupt = AtomicBool::new(false);
    let parent = acquire(&config, &interrupt).unwrap();
    let lineage = lineage();
    std::thread::scope(|scope| {
        // A fresh thread has no depth, so it cannot take the full ceiling's slot...
        scope
            .spawn(|| assert!(acquire(&config, &interrupt).is_none()))
            .join()
            .unwrap();
        // ...but the one child the parent waits for runs under the parent's slot.
        scope
            .spawn(|| {
                let borrowed = lineage.enter();
                let grandchild = acquire(&config, &interrupt).expect("borrowed slot");
                drop(grandchild);
                drop(borrowed);
                assert_eq!(
                    DEPTH.with(Cell::get),
                    0,
                    "borrowing ends on the child thread"
                );
                assert!(
                    acquire(&config, &interrupt).is_none(),
                    "the parent still owns the permit"
                );
            })
            .join()
            .unwrap();
    });
    drop(parent);
    assert_eq!(DEPTH.with(Cell::get), 0);
}
