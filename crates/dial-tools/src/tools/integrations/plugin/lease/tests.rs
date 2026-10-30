use super::*;

#[test]
fn execution_and_publication_leases_exclude_each_other() {
    let root = tempfile::tempdir().unwrap();
    let first = shared(root.path(), "demo").unwrap();
    let second = shared(root.path(), "demo").unwrap();
    // Model a descriptor inherited by another thread's fork before its CLOEXEC runs.
    let inherited = first.0.try_clone().unwrap();
    assert!(exclusive(root.path(), "demo").is_err());
    drop((first, second));
    let publishing = exclusive(root.path(), "demo").unwrap();
    assert!(shared(root.path(), "demo").is_err());
    assert!(exclusive(root.path(), "demo").is_err());
    drop(publishing);
    assert!(shared(root.path(), "demo").is_ok());
    drop(inherited);
}
