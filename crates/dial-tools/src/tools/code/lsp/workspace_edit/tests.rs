use super::Snapshot;
use crate::tools::Workspace;
use std::fs;

#[test]
fn skipped_dependency_tree_does_not_block_source_but_cannot_be_an_edit_target() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("node_modules")).unwrap();
    fs::write(temp.path().join("node_modules/dependency.js"), "old").unwrap();
    fs::write(temp.path().join("source.rs"), "old").unwrap();
    let ws = Workspace::new(temp.path()).unwrap();
    let snapshot = Snapshot::capture(&ws).unwrap();
    let source = ws.root().join("source.rs");
    let dependency = ws.root().join("node_modules/dependency.js");
    assert!(snapshot.validate(&ws, &source).is_ok());
    assert!(snapshot.validate(&ws, &dependency).is_err());
    assert!(
        snapshot
            .validate_absent(&ws, &ws.root().join("new.rs"))
            .is_ok()
    );
    assert!(
        snapshot
            .validate_absent(&ws, &ws.root().join("node_modules/new.js"))
            .is_err()
    );
    fs::write(&source, "changed").unwrap();
    assert!(snapshot.validate(&ws, &source).is_err());
}

#[test]
fn incomplete_directory_never_proves_absence() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("existing.rs"), "source").unwrap();
    let ws = Workspace::new(temp.path()).unwrap();
    let snapshot = Snapshot::capture_with_limit(&ws, 0).unwrap();
    assert!(
        snapshot
            .validate(&ws, &ws.root().join("existing.rs"))
            .is_err()
    );
    assert!(
        snapshot
            .validate_absent(&ws, &ws.root().join("new.rs"))
            .is_err()
    );
}

#[test]
fn absent_witness_rejects_replaced_parent_and_disappeared_entries() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("dir")).unwrap();
    fs::write(temp.path().join("dir/old.rs"), "old").unwrap();
    let ws = Workspace::new(temp.path()).unwrap();
    let snapshot = Snapshot::capture(&ws).unwrap();
    let parent = ws.root().join("dir");
    assert!(
        snapshot
            .validate_absent(&ws, &parent.join("new.rs"))
            .is_ok()
    );
    fs::rename(&parent, ws.root().join("original-dir")).unwrap();
    fs::create_dir(&parent).unwrap();
    assert!(
        snapshot
            .validate_absent(&ws, &parent.join("new.rs"))
            .is_err()
    );
    assert!(
        snapshot
            .validate_absent(&ws, &parent.join("old.rs"))
            .is_err()
    );
}
