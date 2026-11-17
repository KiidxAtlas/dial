use crate::tools::testkit;

#[test]
fn ownership_moves_across_threads_but_not_workspace_domains() {
    let (_root, workspace) = testkit::workspace();
    let (other_root, _) = testkit::workspace();
    let clone = workspace.clone();
    let isolated = workspace.rebased(other_root.path()).unwrap();
    let guard = workspace.serial_operation();
    assert!(clone.try_serial_operation().is_none());
    assert!(isolated.try_serial_operation().is_some());
    std::thread::spawn(move || drop(guard)).join().unwrap();
    assert!(clone.try_serial_operation().is_some());
}

#[test]
fn failed_admission_after_acquisition_releases_ownership() {
    let (_root, workspace) = testkit::workspace();
    let mut checks = 0;
    let result = workspace.serial_operation_checked(|| {
        checks += 1;
        if checks == 2 {
            Err("cancelled")
        } else {
            Ok(())
        }
    });
    assert_eq!(result.unwrap_err(), "cancelled");
    assert!(workspace.try_serial_operation().is_some());
}
