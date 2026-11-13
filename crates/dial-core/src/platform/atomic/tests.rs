use super::*;

#[test]
fn policies_replace_without_leaving_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    write_private(&path, b"old").unwrap();
    write_private(&path, b"new").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new");
    assert!(platform_fs::is_owner_only(&File::open(&path).unwrap()).unwrap());
    write_checkpoint(&path, b"restored", 0o644).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"restored");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

/// Verifying an existing private file through a read-only handle must still be able to restrict
/// it: on Windows that handle has no WRITE_DAC (the bundled Node runtime check failed with
/// "Access is denied").
#[test]
fn check_file_restricts_a_file_opened_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime");
    write_checkpoint(&path, b"bytes", 0o644).unwrap();
    let file =
        platform_fs::open_nofollow_nonblocking(fs::OpenOptions::new().read(true), &path).unwrap();
    check_file(&file).unwrap();
    drop(file);
    assert!(platform_fs::is_owner_only(&File::open(&path).unwrap()).unwrap());
}

#[test]
fn deferred_checkpoint_writes_are_published_before_their_flush() {
    let dir = tempfile::tempdir().unwrap();
    // Checkpoint blobs start at files/0: a one-unit name exercises Windows' native buffer minimum.
    let path = dir.path().join("0");
    write_checkpoint(&path, b"old", 0o600).unwrap();
    let pending = write_checkpoint_deferred(&path, b"new", 0o600).unwrap();
    // Readers (and a restarted process) already see the complete record; no staging file remains.
    assert_eq!(fs::read(&path).unwrap(), b"new");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(pending.path(), path);
    pending.finish().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new");
    // Flushing needs write access on Windows (`FlushFileBuffers`); a read-only handle is refused.
    platform_fs::sync_file(&fs::OpenOptions::new().write(true).open(&path).unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn workspace_preserves_symlinks_but_private_storage_rejects_links() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    fs::write(&target, "old").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(write_private(&link, b"forbidden").is_err());
    write_replacing(&link, b"new").unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), b"new");
    let hard = dir.path().join("hard");
    fs::hard_link(&target, &hard).unwrap();
    assert!(write_private(&hard, b"forbidden").is_err());
    assert_eq!(fs::read(&target).unwrap(), b"new");
}

#[cfg(unix)]
#[test]
fn workspace_and_checkpoint_modes_remain_distinct_from_private_state() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("script");
    fs::write(&path, "old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
    write_replacing(&path, b"new").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o751
    );
    write_checkpoint(&path, b"restored", 0o640).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    write_private(&path, b"private").unwrap();
    assert!(platform_fs::is_owner_only(&File::open(&path).unwrap()).unwrap());
}

#[cfg(unix)]
#[test]
fn workspace_publication_does_not_require_directory_read_permission() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o300)).unwrap();
    let result = write_replacing(&path, b"published");
    // Restore before asserting so the fixture remains removable even on failure.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"published");
}

#[cfg(unix)]
#[test]
fn private_parent_symlinks_and_failed_publication_leave_targets_intact() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert!(write_private(&alias.join("record"), b"forbidden").is_err());
    assert!(!real.join("record").exists());
    assert!(write_replacing(&real, b"not a directory").is_err());
    assert!(real.is_dir());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}
