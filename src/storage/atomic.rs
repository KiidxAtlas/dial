//! Atomic publication with distinct workspace, checkpoint and owner-private policies.
use crate::platform::fs as platform_fs;
use anyhow::{Context, Result, ensure};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Workspace replacement follows existing symlinks and preserves permissions. New files use
/// ordinary creation permissions (including the umask), not private-state permissions.
pub(crate) fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = fs::metadata(&target).ok().map(|meta| meta.permissions());
    let temp = temp_path(&target);
    // Do not remove a pre-existing staging path if exclusive creation fails.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        if let Some(mode) = mode {
            file.set_permissions(mode)?;
        }
        file.sync_all()?;
        publish(&file, &temp, &target, DirectorySync::BestEffort).map_err(io::Error::other)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Checkpoint restores use the recorded mode and create missing parents. The caller owns path
/// confinement; unlike private state this may replace workspace files with ordinary permissions.
pub(crate) fn write_checkpoint(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    platform_fs::set_mode(temp.path(), mode)?;
    temp.as_file().sync_all()?;
    publish(temp.as_file(), temp.path(), path, DirectorySync::BestEffort)
}

/// Validate ancestors without creating directories or changing permissions (also used for reads).
pub(crate) fn validate_private_path(dir: &Path) -> Result<()> {
    ensure!(
        !dir.components()
            .any(|part| part == std::path::Component::ParentDir),
        "private storage path must not contain parent traversal"
    );
    // System-owned aliases such as macOS /var are trusted; user-owned aliases are not.
    for ancestor in dir.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(ancestor) {
            ensure!(
                !meta.file_type().is_symlink() || platform_fs::path_owned_by_system(ancestor)?,
                "private storage path contains a user-owned symlink"
            );
        }
    }
    Ok(())
}

/// Owner-private state larger than this is refused. Callers that can defer work (such as stream
/// reclamation) check it before publishing, so a refusal never corrupts or latches their state.
pub(crate) const PRIVATE_LIMIT: usize = 8 * 1024 * 1024;

/// Owner-private publication rejects links and foreign owners. It never creates parent directories
/// or acquires a lock: the transaction owner must prepare the private directory and hold its lock.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= PRIVATE_LIMIT,
        "private storage file exceeds size limit"
    );
    let dir = path.parent().context("storage path has no parent")?;
    validate_private_path(dir)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "unsafe private storage target"
            );
            let target =
                platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path)?;
            ensure!(
                platform_fs::link_count(&target)? == 1
                    && platform_fs::owned_by_current_user(&target)?,
                "unsafe private storage target"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    platform_fs::set_owner_only(temp.path())?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    publish(temp.as_file(), temp.path(), path, DirectorySync::Required)
}

enum DirectorySync {
    BestEffort,
    Required,
}

/// Only publication mechanics are shared: callers retain their durability contracts too.
fn publish(staged: &File, temp: &Path, target: &Path, sync: DirectorySync) -> Result<()> {
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(unix)]
    {
        let _ = staged;
        fs::rename(temp, target).with_context(|| format!("replacing {}", target.display()))?;
        let result = File::open(dir).and_then(|directory| directory.sync_all());
        if matches!(sync, DirectorySync::Required) {
            result?;
        }
    }
    #[cfg(windows)]
    {
        let _ = sync;
        let parent = platform_fs::open_dir_nofollow_write(dir)?;
        platform_fs::replace_beneath_file(
            &parent,
            Path::new(temp.file_name().context("temporary file has no name")?),
            staged,
            Path::new(target.file_name().context("storage file has no name")?),
        )?;
        // Windows guarantees atomic reader publication of the flushed file, but has no documented
        // unprivileged directory flush/write-through rename: namespace crash durability is weaker.
    }
    Ok(())
}

fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    dir.join(format!(".{name}.{}.{nanos}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
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
}
