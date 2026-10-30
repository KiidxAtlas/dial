use super::*;
use crate::tools::testkit::workspace;

#[test]
fn normalized_paths_share_stable_handles_and_clone_registry() {
    let (_dir, ws) = workspace();
    std::fs::create_dir(ws.root().join("src")).unwrap();
    assert_eq!(ws.existing_file_handle("a.rs"), None);
    let cloned = ws.clone();
    assert_eq!(ws.file_handle("src/../a.rs").unwrap(), "@f1");
    assert_eq!(cloned.file_handle("./a.rs").unwrap(), "@f1");
    assert_eq!(cloned.file_handle("b.rs").unwrap(), "@f2");
    assert_eq!(ws.file_handle_path("@f2").unwrap(), "b.rs");
    assert_eq!(ws.existing_file_handle("./a.rs"), Some("@f1".into()));
    assert_eq!(ws.existing_file_handle("unknown"), None);
    assert_eq!(ws.resolve("@f1").unwrap(), ws.root().join("a.rs"));
}

#[test]
fn expansion_is_exact_and_preserves_selectors_without_interpolation() {
    let (_dir, ws) = workspace();
    ws.file_handle("src.rs").unwrap();
    for suffix in [":1-9", ":raw:1-3", ":outline", ":-20", ":2+3", ":1-3,6-9"] {
        assert_eq!(
            ws.expand_file_handle(&format!("@f1{suffix}")).unwrap(),
            format!("src.rs{suffix}")
        );
        assert!(ws.resolve(&format!("@f1{suffix}")).is_err());
        assert!(ws.file_handle_path(&format!("@f1{suffix}")).is_err());
    }
    for literal in [
        "./@f1",
        "@f1/x",
        "prefix @f1",
        "cat @f1",
        "@f1(path)",
        "@f1x",
        "@file",
        "a/@f1",
    ] {
        assert_eq!(ws.expand_file_handle(literal).unwrap(), literal);
    }
    for unknown in [
        "@f0",
        "@f01",
        "@f2",
        "@f9999999999999999999999999999",
        "@f2:1-9",
    ] {
        assert!(ws.expand_file_handle(unknown).is_err(), "{unknown}");
        assert!(ws.resolve(unknown).is_err(), "{unknown}");
    }
}

#[test]
fn reserved_literal_names_remain_accessible_explicitly() {
    let (_dir, ws) = workspace();
    std::fs::write(ws.root().join("@f1"), "literal").unwrap();
    assert!(ws.resolve("@f1").is_err());
    assert_eq!(ws.file_handle("./@f1").unwrap(), "@f1");
    assert_eq!(ws.expand_file_handle("@f1").unwrap(), "./@f1");
    assert_eq!(ws.resolve("@f1").unwrap(), ws.resolve("./@f1").unwrap());
}

#[test]
fn reroot_retires_old_identifiers_without_affecting_old_clones() {
    let (_dir, ws) = workspace();
    let (other, _) = workspace();
    ws.file_handle("old.rs").unwrap();
    let old_clone = ws.clone();
    let moved = ws.reroot(other.path()).unwrap();
    assert!(moved.resolve("@f1").is_err());
    assert_eq!(moved.file_handle("new.rs").unwrap(), "@f2");
    assert!(moved.resolve("@f1").is_err());
    assert_eq!(old_clone.file_handle_path("@f1").unwrap(), "old.rs");
    let isolated = ws.rebased(other.path()).unwrap();
    assert!(isolated.resolve("@f1").is_err());
    assert_eq!(isolated.file_handle("copy.rs").unwrap(), "@f1");
    assert_eq!(ws.file_handle_path("@f1").unwrap(), "old.rs");
}

#[test]
fn snapshots_merge_compatible_prefixes_and_reject_conflicts_transactionally() {
    let (_dir, ws) = workspace();
    ws.file_handle("one").unwrap();
    let short = ws.file_handle_snapshot();
    ws.file_handle("two").unwrap();
    let long = ws.file_handle_snapshot();
    let serialized = serde_json::to_string(&long).unwrap();
    let restored = Workspace::new(ws.root()).unwrap();
    restored.restore_file_handles(&short).unwrap();
    let clone = restored.clone();
    clone
        .restore_file_handles(&serde_json::from_str(&serialized).unwrap())
        .unwrap();
    restored.restore_file_handles(&short).unwrap();
    assert_eq!(restored.file_handle_snapshot(), long);
    assert_eq!(restored.file_handle("three").unwrap(), "@f3");
    let before = restored.file_handle_snapshot();
    let mut bad = long.clone();
    bad.paths[0] = "different".into();
    assert!(restored.restore_file_handles(&bad).is_err());
    assert_eq!(restored.file_handle_snapshot(), before);
    let (other, _) = workspace();
    assert!(
        restored
            .reroot(other.path())
            .unwrap()
            .restore_file_handles(&long)
            .is_err()
    );
}

#[test]
fn snapshots_validate_all_paths_bounds_duplicates_and_ids() {
    let (_dir, ws) = workspace();
    ws.file_handle("safe").unwrap();
    let valid = ws.file_handle_snapshot();
    for paths in [
        vec!["../escape".into()],
        vec!["./safe".into()],
        vec!["same".into(), "same".into()],
        vec!["a".repeat(MAX_PATH_BYTES + 1)],
        vec!["a".into(); MAX_HANDLES + 1],
    ] {
        let mut bad = valid.clone();
        bad.paths = paths;
        assert!(ws.restore_file_handles(&bad).is_err());
        assert_eq!(ws.file_handle_snapshot(), valid);
    }
    for first_id in [0, u64::MAX] {
        let mut bad = valid.clone();
        bad.first_id = first_id;
        assert!(ws.restore_file_handles(&bad).is_err());
    }
}

#[test]
fn reroot_highwater_survives_restart() {
    let (_dir, ws) = workspace();
    let (other, _) = workspace();
    ws.file_handle("old").unwrap();
    let moved = ws.reroot(other.path()).unwrap();
    let resumed = Workspace::new(other.path()).unwrap();
    resumed
        .restore_file_handles(&moved.file_handle_snapshot())
        .unwrap();
    assert!(resumed.resolve("@f1").is_err());
    assert_eq!(resumed.file_handle("new").unwrap(), "@f2");
}

#[test]
fn registry_exhaustion_never_evicts_or_reassigns() {
    let (_dir, ws) = workspace();
    for i in 0..MAX_HANDLES {
        assert_eq!(
            ws.file_handle(&format!("file-{i}")).unwrap(),
            format!("@f{}", i + 1)
        );
    }
    assert!(ws.file_handle("overflow").is_err());
    assert_eq!(ws.file_handle("file-0").unwrap(), "@f1");
    assert_eq!(ws.file_handle_path("@f1").unwrap(), "file-0");
    assert_eq!(ws.resolve("overflow").unwrap(), ws.root().join("overflow"));
}

#[test]
fn attached_roots_and_protected_paths_remain_authoritative() {
    let (_dir, ws) = workspace();
    let (attached, _) = workspace();
    assert!(
        ws.file_handle(attached.path().join("file").to_str().unwrap())
            .is_err()
    );
    let mut ws = ws
        .with_additional_roots(&[attached.path().to_path_buf()])
        .unwrap();
    let target = attached.path().join("file");
    let handle = ws.file_handle(target.to_str().unwrap()).unwrap();
    assert_eq!(
        ws.file_handle_path(&handle).unwrap(),
        target.to_str().unwrap()
    );
    ws.remove_root(attached.path()).unwrap();
    assert!(ws.resolve(&handle).is_err());
    assert!(ws.expand_file_handle(&handle).is_err());
    let protected = ws.root().join("private");
    let handle = ws.file_handle("private/key").unwrap();
    let ws = ws.with_protected_paths(&[protected]);
    assert!(ws.resolve(&handle).is_err());
    assert!(ws.file_handle("private/key").is_err());
    assert!(ws.file_handle("../escape").is_err());
}

#[cfg(unix)]
#[test]
fn symlink_changes_cannot_turn_a_handle_into_an_escape() {
    let (_dir, ws) = workspace();
    let (outside, _) = workspace();
    let handle = ws.file_handle("link/file").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.root().join("link")).unwrap();
    assert!(ws.resolve(&handle).is_err());
    assert!(ws.expand_file_handle(&handle).is_err());
    assert!(ws.file_handle("link/file").is_err());
}

#[test]
fn explicit_session_reset_does_not_retarget_existing_clones() {
    let (_dir, mut ws) = workspace();
    ws.file_handle("old-session").unwrap();
    let old = ws.clone();
    ws.reset_file_handles();
    assert!(ws.resolve("@f1").is_err());
    assert_eq!(ws.file_handle("new-session").unwrap(), "@f1");
    assert_eq!(old.file_handle_path("@f1").unwrap(), "old-session");
    assert_eq!(ws.file_handle_path("@f1").unwrap(), "new-session");
}

#[test]
fn restoration_preserves_inaccessible_identities_without_restoring_grants() {
    let (_dir, ws) = workspace();
    let (attached, _) = workspace();
    let mut granted = ws
        .with_additional_roots(&[attached.path().to_path_buf()])
        .unwrap();
    let token = granted
        .file_handle(attached.path().join("secret.txt").to_str().unwrap())
        .unwrap();
    let snapshot = granted.file_handle_snapshot();
    granted.remove_root(attached.path()).unwrap();
    let restored = Workspace::new(granted.root()).unwrap();
    restored.restore_file_handles(&snapshot).unwrap();
    assert_eq!(restored.file_handle_snapshot(), snapshot);
    assert!(restored.expand_file_handle(&token).is_err());
    assert_eq!(restored.file_handle("new.txt").unwrap(), "@f2");
}

#[cfg(unix)]
#[test]
fn restoring_after_a_symlink_escape_does_not_block_the_session() {
    let (_dir, ws) = workspace();
    let (outside, _) = workspace();
    ws.file_handle("link/file").unwrap();
    let snapshot = ws.file_handle_snapshot();
    std::os::unix::fs::symlink(outside.path(), ws.root().join("link")).unwrap();
    let restored = Workspace::new(ws.root()).unwrap();
    restored.restore_file_handles(&snapshot).unwrap();
    assert!(restored.resolve("@f1").is_err());
    assert_eq!(restored.file_handle("safe").unwrap(), "@f2");
}
