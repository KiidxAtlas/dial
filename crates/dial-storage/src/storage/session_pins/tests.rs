use super::*;

#[test]
fn rejected_publication_preserves_the_previous_pin_bytes_and_cleans_staging() {
    let dir = tempfile::tempdir().unwrap();
    toggle(dir.path(), "original").unwrap();
    let original = std::fs::read(file(dir.path())).unwrap();
    let oversized = vec!["x".repeat(8 * 1024 * 1024)];
    assert!(write(dir.path(), &oversized).is_err());
    assert_eq!(std::fs::read(file(dir.path())).unwrap(), original);
    let before = std::fs::read_dir(dir.path()).unwrap().count();
    let bad_target = file(dir.path());
    std::fs::remove_file(&bad_target).unwrap();
    std::fs::create_dir(&bad_target).unwrap();
    assert!(write(dir.path(), &["replacement".into()]).is_err());
    assert!(bad_target.is_dir());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before);
}

#[cfg(unix)]
#[test]
fn pin_publication_refuses_symlink_targets_without_changing_the_referent() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("original.json");
    std::fs::write(&outside, b"[\"original\"]\n").unwrap();
    std::os::unix::fs::symlink(&outside, file(dir.path())).unwrap();
    assert!(toggle(dir.path(), "new").is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"[\"original\"]\n");
    assert!(
        std::fs::symlink_metadata(file(dir.path()))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn toggle_round_trips_and_sorts_pinned_first() {
    let dir = tempfile::tempdir().unwrap();
    assert!(toggle(dir.path(), "b").unwrap());
    assert!(toggle(dir.path(), "a").unwrap());
    assert_eq!(load(dir.path()).unwrap(), vec!["b", "a"]);
    assert!(!toggle(dir.path(), "b").unwrap());
    assert_eq!(load(dir.path()).unwrap(), vec!["a"]);
    unpin(dir.path(), "a").unwrap();
    assert!(load(dir.path()).unwrap().is_empty());

    let pinned = vec!["c".to_string()];
    let sorted = sort_pinned_first(vec!["a", "c", "b"], &pinned, |s| s);
    assert_eq!(sorted, vec!["c", "a", "b"]);
}
