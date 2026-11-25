use super::*;
use std::fs;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

#[cfg(unix)]
fn install(dir: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn the_bundled_directory_is_searched_after_every_existing_entry() {
    let joined = std::env::join_paths(["/first", "/second"]).unwrap();
    let path = appended(Some(&joined), Path::new("/bundled")).unwrap();
    let entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
    assert_eq!(
        entries,
        [
            PathBuf::from("/first"),
            PathBuf::from("/second"),
            PathBuf::from("/bundled")
        ]
    );
    let alone = appended(None, Path::new("/bundled")).unwrap();
    assert_eq!(alone, OsString::from("/bundled"));
}

#[cfg(unix)]
#[test]
fn an_environment_python3_is_never_replaced() {
    let (_dir, ws) = workspace();
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().as_os_str().to_owned();
    assert!(!resolves(&ws, Some(&path), "python3"));
    install(bin.path(), "python3");
    assert!(resolves(&ws, Some(&path), "python3"));
    assert!(!offered(&ws, Some(&path)));
    assert!(fallback(&ws, Some(&path)).is_none());
    // A launcher the sandbox hides does not count as available.
    let hidden = ws.with_protected_paths(&[bin.path().to_owned()]);
    assert!(!resolves(&hidden, Some(&path), "python3"));
}

#[test]
fn a_build_without_cpython_leaves_the_path_alone() {
    if bundled_python::available() {
        return;
    }
    let (_dir, ws) = workspace();
    let empty = tempfile::tempdir().unwrap();
    assert!(!offered(&ws, Some(empty.path().as_os_str())));
    assert!(fallback(&ws, Some(empty.path().as_os_str())).is_none());
}

/// Runs only in builds that embed CPython (`DIAL_BUNDLED_PYTHON_DIR`).
#[test]
fn a_shell_without_python3_gets_the_bundled_interpreter_under_both_names() {
    if !bundled_python::available() {
        return;
    }
    let (_dir, ws) = workspace();
    let empty = tempfile::tempdir().unwrap();
    let (runtime, path) = fallback(&ws, Some(empty.path().as_os_str())).expect("fallback");
    let last = std::env::split_paths(&path).last().unwrap();
    assert!(runtime.executable().starts_with(&last));
    for name in ["python3", "python"] {
        let program = if cfg!(windows) {
            last.join(format!("{name}.exe"))
        } else {
            last.join(name)
        };
        let out = std::process::Command::new(&program)
            .args([
                "-c",
                "import sys, sqlite3; print(sys.version_info[:2] >= (3, 8))",
            ])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{name}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "True");
    }
    // One extraction per process.
    let (again, _) = fallback(&ws, Some(empty.path().as_os_str())).unwrap();
    assert!(std::ptr::eq(runtime, again));
}
