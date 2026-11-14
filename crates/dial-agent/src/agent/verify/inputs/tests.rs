use super::*;

fn declared(paths: &[&str]) -> VerificationInputs {
    VerificationInputs {
        paths: paths.iter().map(|s| s.to_string()).collect(),
        environment: vec![],
    }
}

#[test]
fn content_and_directory_membership_not_unrelated_files_determine_identity() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/a"), "one").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let inputs = declared(&["src"]);
    let original = capture(&workspace, &inputs).unwrap();
    fs::write(root.path().join("README.md"), "docs").unwrap();
    assert_eq!(capture(&workspace, &inputs), Some(original.clone()));
    fs::write(root.path().join("src/a"), "two").unwrap();
    assert_ne!(capture(&workspace, &inputs), Some(original.clone()));
    fs::write(root.path().join("src/a"), "one").unwrap();
    assert_eq!(capture(&workspace, &inputs), Some(original.clone()));
    // Hidden and ignored inputs are not omitted by source-search policy.
    fs::write(root.path().join("src/.ignored"), "fixture").unwrap();
    assert_ne!(capture(&workspace, &inputs), Some(original.clone()));
    fs::remove_file(root.path().join("src/.ignored")).unwrap();
    assert_eq!(capture(&workspace, &inputs), Some(original.clone()));
    fs::rename(root.path().join("src/a"), root.path().join("src/b")).unwrap();
    assert_ne!(capture(&workspace, &inputs), Some(original));
}

#[test]
fn unknown_missing_unsafe_and_oversized_inputs_have_no_identity() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    for paths in [vec![], vec!["missing"], vec!["../outside"], vec!["."]] {
        assert_eq!(capture(&workspace, &declared(&paths)), None);
    }
    let file = fs::File::create(root.path().join("large")).unwrap();
    file.set_len(MAX_BYTES + 1).unwrap();
    assert_eq!(capture(&workspace, &declared(&["large"])), None);
}

#[test]
fn environment_selection_and_empty_directories_are_part_of_identity() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("inputs")).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut inputs = declared(&["inputs"]);
    let before = capture(&workspace, &inputs).unwrap();
    inputs.environment.push("PATH".into());
    assert_ne!(capture(&workspace, &inputs), Some(before.clone()));
    inputs.environment.clear();
    fs::create_dir(root.path().join("inputs/empty")).unwrap();
    assert_ne!(capture(&workspace, &inputs), Some(before));
}

#[cfg(unix)]
#[test]
fn symlinks_in_files_directories_or_parent_components_are_unknown() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("real")).unwrap();
    fs::write(root.path().join("real/file"), "data").unwrap();
    std::os::unix::fs::symlink("real", root.path().join("link")).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    assert_eq!(capture(&workspace, &declared(&["link/file"])), None);
    std::os::unix::fs::symlink("file", root.path().join("real/alias")).unwrap();
    assert_eq!(capture(&workspace, &declared(&["real"])), None);
}

#[cfg(unix)]
#[test]
fn installed_runtime_identity_detects_replacement_with_preserved_size_and_mtime() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("runtime");
    fs::write(&path, "one").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let snapshot = || {
        capture_excluding(
            &workspace,
            &declared(&["runtime"]),
            BTreeSet::new(),
            IdentityKind::InstalledRuntime,
        )
        .unwrap()
    };
    let before = snapshot();
    assert_eq!(snapshot(), before);
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let replacement = root.path().join("replacement");
    fs::write(&replacement, "two").unwrap();
    fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert_ne!(snapshot(), before);
}

#[cfg(unix)]
#[test]
fn installed_runtime_members_keep_symlink_exclusion_and_metadata_rules() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("lib/pkg")).unwrap();
    fs::create_dir_all(root.path().join("lib/site-packages")).unwrap();
    fs::write(root.path().join("lib/pkg/mod.py"), "one").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let excluded: BTreeSet<String> = ["lib/site-packages".to_owned()].into();
    let snapshot = || {
        capture_excluding(
            &workspace,
            &declared(&["lib"]),
            excluded.clone(),
            IdentityKind::InstalledRuntime,
        )
    };
    let before = snapshot().unwrap();
    // Excluded trees are skipped; a nested member's metadata is part of the identity.
    fs::write(root.path().join("lib/site-packages/extra.py"), "x").unwrap();
    assert_eq!(snapshot(), Some(before.clone()));
    fs::write(root.path().join("lib/pkg/mod.py"), "longer").unwrap();
    assert_ne!(snapshot(), Some(before));
    // A symlink anywhere below the declared directory still makes the identity unknown.
    std::os::unix::fs::symlink("mod.py", root.path().join("lib/pkg/alias.py")).unwrap();
    assert_eq!(snapshot(), None);
}

#[test]
fn project_directory_members_still_pass_protected_storage_policy() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("src/private")).unwrap();
    fs::write(root.path().join("src/private/state"), "secret").unwrap();
    fs::write(root.path().join("src/a"), "one").unwrap();
    let inputs = declared(&["src"]);
    let open = Workspace::new(root.path()).unwrap();
    assert!(capture(&open, &inputs).is_some());
    let protected = Workspace::new(root.path())
        .unwrap()
        .with_protected_paths(&[std::path::PathBuf::from("src/private")]);
    assert_eq!(capture(&protected, &inputs), None);
}

#[test]
fn an_exhausted_time_budget_is_unknown_never_a_snapshot() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a"), "one").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    assert!(capture(&workspace, &declared(&["a"])).is_some());
    TIME_OVERRIDE.with(|forced| forced.set(Some(Duration::ZERO)));
    let exhausted = capture(&workspace, &declared(&["a"]));
    TIME_OVERRIDE.with(|forced| forced.set(None));
    assert!(exhausted.is_none());
}

#[test]
fn a_scoped_workspace_budget_reaches_other_threads_and_isolates_roots() {
    let scoped = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    for root in [&scoped, &other] {
        fs::write(root.path().join("a"), "one").unwrap();
    }
    let scoped_ws = Workspace::new(scoped.path()).unwrap();
    let other_ws = Workspace::new(other.path()).unwrap();
    let _budget = scope_workspace_budget(scoped_ws.root(), Duration::ZERO);
    // A zero allowance fails closed on the worker thread that actually captures the inputs.
    let root = scoped_ws.root().to_path_buf();
    let on_worker = std::thread::spawn(move || {
        let workspace = Workspace::new(&root).unwrap();
        capture(&workspace, &declared(&["a"]))
    })
    .join()
    .unwrap();
    assert_eq!(on_worker, None);
    assert_eq!(capture(&scoped_ws, &declared(&["a"])), None);
    // Other workspaces keep the production allowance.
    assert!(capture(&other_ws, &declared(&["a"])).is_some());
}

#[test]
fn a_scoped_workspace_budget_is_restored_on_drop_and_thread_override_wins() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a"), "one").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let budget = scope_workspace_budget(workspace.root(), Duration::ZERO);
    assert_eq!(capture(&workspace, &declared(&["a"])), None);
    // The thread-local override is consulted before any scoped allowance.
    TIME_OVERRIDE.with(|forced| forced.set(Some(Duration::from_secs(3600))));
    let overridden = capture(&workspace, &declared(&["a"]));
    TIME_OVERRIDE.with(|forced| forced.set(None));
    assert!(overridden.is_some());
    drop(budget);
    assert!(capture(&workspace, &declared(&["a"])).is_some());
}
