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
