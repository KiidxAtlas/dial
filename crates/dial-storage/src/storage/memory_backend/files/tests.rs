use super::*;

#[test]
fn recursive_cleanup_snapshots_names_and_does_not_follow_symlinks() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), b"outside").unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let directory = PrivateDirectory::open(&layout, "cleanup").unwrap();
    let nested = directory.child("nested").unwrap();
    for n in 0..256 {
        nested.write(&format!("item-{n}"), b"private").unwrap();
    }
    nested
        .child("deeper")
        .unwrap()
        .write("item", b"private")
        .unwrap();
    std::os::unix::fs::symlink(outside.path(), nested.path.join("outside")).unwrap();
    drop(nested);
    directory.remove_tree("nested").unwrap();
    assert!(!directory.path.join("nested").exists());
    assert_eq!(
        std::fs::read(outside.path().join("keep")).unwrap(),
        b"outside"
    );
}

#[test]
fn child_directory_operations_refuse_links_and_stay_bound_to_the_open_parent() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let directory = PrivateDirectory::open(&layout, "directories").unwrap();
    symlink(outside.path(), directory.path.join("linked")).unwrap();
    std::fs::write(directory.path.join("regular"), b"not a directory").unwrap();
    for name in ["linked", "regular", "..", "../escape", "", "two/names"] {
        assert!(directory.child(name).is_err(), "create child {name:?}");
        assert!(
            directory.existing_child(name).is_err(),
            "open child {name:?}"
        );
    }
    assert!(!outside.path().join("escape").exists());

    let moved = directory.path.with_file_name("moved");
    std::fs::rename(&directory.path, &moved).unwrap();
    symlink(outside.path(), &directory.path).unwrap();
    directory.child("bound").unwrap();
    assert!(directory.existing_child("bound").is_ok());
    assert!(moved.join("bound").is_dir());
    assert_eq!(
        std::fs::metadata(moved.join("bound"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "new child remains owner-only"
    );
    assert!(!outside.path().join("bound").exists());
}
