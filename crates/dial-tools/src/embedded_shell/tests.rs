use super::*;

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn invocation_writes_script_and_names_host_argument() {
    let dir = scratch();
    let (program, args) = invocation(dir.path(), "echo hi\n").unwrap();
    assert_eq!(program, host_exe().unwrap());
    assert_eq!(args[0], HOST_ARG);
    let path = PathBuf::from(&args[1]);
    assert_eq!(path.parent(), Some(dir.path()));
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(SCRIPT_PREFIX)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "echo hi\n");
}

#[test]
fn invocation_never_reuses_a_script_path() {
    let dir = scratch();
    let (_, a) = invocation(dir.path(), "a").unwrap();
    let (_, b) = invocation(dir.path(), "b").unwrap();
    assert_ne!(a[1], b[1]);
}

#[test]
fn host_exe_honours_override() {
    if !crate::tools::testkit::in_env_subprocess(
        "embedded_shell::tests::host_exe_honours_override",
        &[(HOST_EXE_ENV, "/opt/dial-host")],
    ) {
        return;
    }
    assert_eq!(host_exe().unwrap(), PathBuf::from("/opt/dial-host"));
}

#[cfg(feature = "embedded-shell")]
#[test]
fn run_host_reports_missing_script_as_usage_error() {
    let dir = scratch();
    assert_eq!(run_host(&dir.path().join("missing.sh")), 2);
}

#[test]
fn discard_script_removes_only_dial_script_files() {
    let dir = scratch();
    let (_, args) = invocation(dir.path(), "echo hi").unwrap();
    let path = PathBuf::from(&args[1]);
    assert!(path.exists());
    // A sandbox launcher may put other arguments before the host command line.
    discard_script(["bwrap", "--x"].map(String::from).into_iter().chain(args));
    assert!(!path.exists());

    let other = dir.path().join("keep.sh");
    fs::write(&other, "x").unwrap();
    discard_script([HOST_ARG.to_owned(), other.display().to_string()]);
    assert!(other.exists());
}

#[test]
fn saved_exports_keep_only_allowed_changes_against_the_inherited_baseline() {
    // A CI runner exports far more than the 128-entry state budget; only the shell's own
    // allowed changes are kept.
    let inherited: std::collections::BTreeMap<String, String> = (0..300)
        .map(|i| (format!("CI_VAR_{i}"), format!("value-{i}")))
        .collect();
    let exported = inherited.clone().into_iter().chain([
        ("CI_VAR_0".to_owned(), "changed".to_owned()),
        ("NAMED_VAR".to_owned(), "kept".to_owned()),
        ("GITHUB_TOKEN".to_owned(), "secret".to_owned()),
        ("HOME".to_owned(), "/elsewhere".to_owned()),
    ]);

    let delta = state::changed_exports(exported, &inherited);
    assert_eq!(
        delta.keys().map(String::as_str).collect::<Vec<_>>(),
        ["CI_VAR_0", "NAMED_VAR"]
    );
    assert_eq!(delta["CI_VAR_0"], "changed");
    let saved = state::State {
        schema_version: 1,
        completed: true,
        cwd: PathBuf::from("/"),
        exported: delta,
    };
    assert!(saved.validate().is_ok());
}
