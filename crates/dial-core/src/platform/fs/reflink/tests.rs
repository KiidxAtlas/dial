use super::*;
use std::fs;

fn file(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn a_clone_that_succeeds_is_reported_and_no_byte_copy_is_made() {
    let dir = tempfile::tempdir().unwrap();
    let source = file(dir.path(), "source", "contents");
    let dest = dir.path().join("dest");
    let method = copy_with(&source, &dest, |_, to| fs::write(to, "cloned")).unwrap();
    assert_eq!(method, CopyMethod::Cloned);
    assert_eq!(fs::read_to_string(&dest).unwrap(), "cloned");
}

#[test]
fn any_clone_failure_falls_back_to_a_byte_copy() {
    let dir = tempfile::tempdir().unwrap();
    let source = file(dir.path(), "source", "contents");
    for kind in [
        io::ErrorKind::Unsupported,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::CrossesDevices,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::AlreadyExists,
    ] {
        let dest = dir.path().join(format!("dest-{kind:?}"));
        let method = copy_with(&source, &dest, |_, _| Err(kind.into())).unwrap();
        assert_eq!(method, CopyMethod::Copied, "{kind:?}");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "contents");
    }
}

#[test]
fn a_clone_that_left_a_partial_destination_is_overwritten_by_the_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let source = file(dir.path(), "source", "contents");
    let dest = dir.path().join("dest");
    let method = copy_with(&source, &dest, |_, to| {
        fs::write(to, "")?;
        Err(io::ErrorKind::Unsupported.into())
    })
    .unwrap();
    assert_eq!(method, CopyMethod::Copied);
    assert_eq!(fs::read_to_string(&dest).unwrap(), "contents");
}

#[test]
fn the_fallback_reports_the_real_error_when_the_copy_cannot_succeed_either() {
    let dir = tempfile::tempdir().unwrap();
    let error = copy_with(
        &dir.path().join("missing"),
        &dir.path().join("dest"),
        |_, _| Err(io::ErrorKind::Unsupported.into()),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn native_copy_has_the_source_contents_and_never_shares_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let source = file(dir.path(), "source", "original");
    let dest = dir.path().join("dest");
    let before = copy_counts();
    copy_on_write(&source, &dest).unwrap();
    assert_eq!(fs::read_to_string(&dest).unwrap(), "original");
    fs::write(&dest, "changed").unwrap();
    assert_eq!(fs::read_to_string(&source).unwrap(), "original");
    let after = copy_counts();
    assert!(after.cloned + after.copied > before.cloned + before.copied);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_temp_volume_clones_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = file(dir.path(), "source", "original");
    let dest = dir.path().join("dest");
    assert_eq!(
        copy_with(&source, &dest, native_clone).unwrap(),
        CopyMethod::Cloned,
        "temp directory is expected to be on APFS"
    );
}

#[test]
fn an_existing_destination_is_replaced_and_keeps_the_source_mode() {
    let dir = tempfile::tempdir().unwrap();
    let source = file(dir.path(), "source", "new");
    let dest = file(dir.path(), "dest", "older and longer");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();
    }
    copy_on_write(&source, &dest).unwrap();
    assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }
}
