use super::*;

fn stamp_of(path: &Path) -> crate::agent::workers::isolation::FileStamp {
    stamp(path).unwrap().unwrap()
}

#[test]
fn remove_subtree_drops_the_name_and_its_children_only() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    fs::write(&file, "x").unwrap();
    let mut snapshot = Snapshot::new();
    for key in ["a", "a/x.txt", "ab/y.txt", "b.txt"] {
        snapshot.insert(PathBuf::from(key), stamp_of(&file));
    }
    remove_subtree(&mut snapshot, Path::new("a"));
    let keys: Vec<_> = snapshot
        .keys()
        .map(|key| key.display().to_string())
        .collect();
    assert_eq!(keys, vec!["ab/y.txt", "b.txt"]);
}

#[test]
fn changed_snapshot_layers_upper_files_over_the_base() {
    let upper = tempfile::tempdir().unwrap();
    fs::write(upper.path().join("new.txt"), "new").unwrap();
    let base = tempfile::tempdir().unwrap();
    let kept = base.path().join("kept.txt");
    fs::write(&kept, "kept").unwrap();
    let mut before = Snapshot::new();
    before.insert(PathBuf::from("kept.txt"), stamp_of(&kept));
    let after = changed_snapshot(upper.path(), &before, &TaskConfig::default()).unwrap();
    let keys: Vec<_> = after.keys().map(|key| key.display().to_string()).collect();
    assert_eq!(keys, vec!["kept.txt", "new.txt"]);
}

#[test]
fn changed_snapshot_is_bounded_by_the_entry_budget() {
    let upper = tempfile::tempdir().unwrap();
    fs::write(upper.path().join("a.txt"), "a").unwrap();
    fs::write(upper.path().join("b.txt"), "b").unwrap();
    let limits = TaskConfig {
        isolation_source_files: 1,
        ..TaskConfig::default()
    };
    let error = changed_snapshot(upper.path(), &Snapshot::new(), &limits).unwrap_err();
    assert!(format!("{error:#}").contains("more than 1 changed entries"));
}

#[cfg(unix)]
#[test]
fn fuse_whiteout_names_map_back_to_the_hidden_entry() {
    assert_eq!(
        fuse_whiteout_target(OsStr::new(".wh.gone.txt")),
        Some(OsString::from("gone.txt"))
    );
    assert_eq!(
        fuse_whiteout_target(OsStr::new(".wh.dir")),
        Some(OsString::from("dir"))
    );
    assert_eq!(fuse_whiteout_target(OsStr::new(FUSE_OPAQUE_MARKER)), None);
    assert_eq!(fuse_whiteout_target(OsStr::new(".wh.")), None);
    assert_eq!(fuse_whiteout_target(OsStr::new(".whiteboard")), None);
    assert_eq!(fuse_whiteout_target(OsStr::new("plain.txt")), None);
}

#[cfg(unix)]
#[test]
fn fuse_whiteouts_and_opaque_markers_shape_the_change_set() {
    let upper = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let kept = base.path().join("kept.txt");
    fs::write(&kept, "kept").unwrap();
    fs::write(upper.path().join(".wh.gone.txt"), "").unwrap();
    fs::write(upper.path().join(".wh.olddir"), "").unwrap();
    fs::create_dir(upper.path().join("replaced")).unwrap();
    fs::write(upper.path().join("replaced").join(FUSE_OPAQUE_MARKER), "").unwrap();
    fs::write(upper.path().join("replaced/new.txt"), "new").unwrap();
    let mut before = Snapshot::new();
    for key in ["gone.txt", "olddir/x.txt", "replaced/old.txt", "kept.txt"] {
        before.insert(PathBuf::from(key), stamp_of(&kept));
    }
    let after = changed_snapshot(upper.path(), &before, &TaskConfig::default()).unwrap();
    let keys: Vec<_> = after.keys().map(|key| key.display().to_string()).collect();
    assert_eq!(keys, vec!["kept.txt", "replaced/new.txt"]);
}

#[test]
fn mount_path_rejects_separators_that_mount_options_cannot_carry() {
    assert!(mount_path(Path::new("/tmp/project")).is_ok());
    assert!(
        mount_path(Path::new("/tmp/a,b"))
            .unwrap_err()
            .contains("cannot represent")
    );
    assert!(
        mount_path(Path::new("/tmp/a:b"))
            .unwrap_err()
            .contains("cannot represent")
    );
}

#[test]
fn excluded_entries_are_the_top_most_secret_matches() {
    let lower = tempfile::tempdir().unwrap();
    let root = lower.path();
    fs::write(root.join(".env"), "secret").unwrap();
    fs::write(root.join(".env.example"), "ok").unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/.env.local"), "secret").unwrap();
    fs::write(root.join("sub/keep.txt"), "keep").unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join(".dial")).unwrap();
    let mut found = excluded_entries(root, 1000).unwrap();
    found.sort();
    assert_eq!(
        found,
        vec![
            PathBuf::from(".dial"),
            PathBuf::from(".env"),
            PathBuf::from(".git"),
            PathBuf::from("sub/.env.local"),
        ]
    );
}

#[test]
fn excluded_entries_do_not_enter_dependency_dirs() {
    let lower = tempfile::tempdir().unwrap();
    let root = lower.path();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), "dep").unwrap();
    fs::write(root.join("node_modules/.env"), "not masked: not entered").unwrap();
    fs::create_dir_all(root.join("target/deep")).unwrap();
    fs::write(root.join("target/deep/out.bin"), "bin").unwrap();
    // Two top-level entries fit the budget; walking into either directory would not.
    assert_eq!(excluded_entries(root, 2).unwrap(), Vec::<PathBuf>::new());
}

#[test]
fn excluded_entries_are_bounded_by_the_entry_budget() {
    let lower = tempfile::tempdir().unwrap();
    fs::write(lower.path().join("a.txt"), "a").unwrap();
    fs::write(lower.path().join("b.txt"), "b").unwrap();
    let error = excluded_entries(lower.path(), 1).unwrap_err();
    assert!(matches!(error, ScanError::TooMany(1)), "{error}");
}

#[test]
fn probe_scan_skips_overlay_when_the_project_is_over_budget() {
    let lower = tempfile::tempdir().unwrap();
    fs::write(lower.path().join("a.txt"), "a").unwrap();
    fs::write(lower.path().join("b.txt"), "b").unwrap();
    assert_eq!(
        scan_for_probe(lower.path(), 1),
        Err("too many entries to scan for secrets".to_owned())
    );
    assert_eq!(scan_for_probe(lower.path(), 2), Ok(()));
}

#[test]
fn fuse_masks_are_wh_files_beside_each_excluded_entry() {
    let upper = tempfile::tempdir().unwrap();
    let entries = [
        PathBuf::from(".env"),
        PathBuf::from("sub/.env.local"),
        PathBuf::from(".ssh"),
    ];
    hide_entries(upper.path(), &entries, Driver::Fuse).unwrap();
    assert!(upper.path().join(".wh..env").is_file());
    assert!(upper.path().join("sub/.wh..env.local").is_file());
    assert!(upper.path().join(".wh..ssh").is_file());
    assert!(!upper.path().join(".env").exists());
}

#[test]
fn mask_paths_match_each_encoding() {
    assert_eq!(
        mask_path(Path::new("/u"), Path::new("sub/.env"), Driver::Kernel),
        PathBuf::from("/u/sub/.env")
    );
    assert_eq!(
        mask_path(Path::new("/u"), Path::new("sub/.env"), Driver::Fuse),
        PathBuf::from("/u/sub/.wh..env")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_masks_are_char_devices_when_mknod_is_allowed() {
    let upper = tempfile::tempdir().unwrap();
    match hide_entries(upper.path(), &[PathBuf::from("sub/.env")], Driver::Kernel) {
        Ok(()) => assert!(is_whiteout(
            &fs::symlink_metadata(upper.path().join("sub/.env")).unwrap()
        )),
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "unexpected error: {error}"
        ),
    }
}

#[test]
fn select_driver_moves_past_a_driver_that_cannot_hide_entries() {
    let chosen = select_driver(true, true, |driver| match driver {
        Driver::Kernel => Err("cannot hide excluded files: mknod denied".into()),
        Driver::Fuse => Ok(()),
    });
    assert_eq!(chosen, Ok(Driver::Fuse));
}

#[test]
fn select_driver_reports_the_mask_failure_when_no_driver_can_hide() {
    let error = select_driver(true, true, |_| {
        Err("cannot hide excluded files: denied".into())
    })
    .unwrap_err();
    assert!(error.contains("cannot hide excluded files"), "{error}");
    assert!(error.contains("kernel overlayfs"), "{error}");
    assert!(error.contains("fuse-overlayfs"), "{error}");
}

#[test]
fn select_driver_skips_absent_drivers_without_testing_them() {
    let chosen = select_driver(false, true, |driver| {
        assert_eq!(
            driver,
            Driver::Fuse,
            "an absent kernel overlay is not tested"
        );
        Ok(())
    });
    assert_eq!(chosen, Ok(Driver::Fuse));
}

#[test]
fn select_driver_reports_absence_when_nothing_is_present() {
    let error = select_driver(false, false, |_| Ok(())).unwrap_err();
    assert!(
        error.contains("kernel overlayfs is not available"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn merge_back_ignores_masks_and_dependency_writes() {
    let upper = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let kept = base.path().join("kept.txt");
    fs::write(&kept, "kept").unwrap();
    let masks = [PathBuf::from(".env"), PathBuf::from("sub/.env.local")];
    hide_entries(upper.path(), &masks, Driver::Fuse).unwrap();
    fs::write(upper.path().join(".wh.gone.txt"), "").unwrap();
    fs::create_dir_all(upper.path().join("node_modules")).unwrap();
    fs::write(upper.path().join("node_modules/new.js"), "dep").unwrap();
    fs::create_dir_all(upper.path().join("sub")).unwrap();
    fs::write(upper.path().join("sub/new.txt"), "new").unwrap();
    let mut before = Snapshot::new();
    for key in ["gone.txt", "kept.txt", "sub/keep.txt", "target/out.bin"] {
        before.insert(PathBuf::from(key), stamp_of(&kept));
    }
    let after = changed_snapshot(upper.path(), &before, &TaskConfig::default()).unwrap();
    let keys: Vec<_> = after.keys().map(|key| key.display().to_string()).collect();
    assert_eq!(
        keys,
        vec!["kept.txt", "sub/keep.txt", "sub/new.txt", "target/out.bin"]
    );
}
