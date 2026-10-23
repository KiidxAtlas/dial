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
