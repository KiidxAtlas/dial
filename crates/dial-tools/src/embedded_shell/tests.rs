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
    // Environment is process-global; only this test touches the variable.
    unsafe { std::env::set_var(HOST_EXE_ENV, "/opt/dial-host") };
    let resolved = host_exe();
    unsafe { std::env::remove_var(HOST_EXE_ENV) };
    assert_eq!(resolved.unwrap(), PathBuf::from("/opt/dial-host"));
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
