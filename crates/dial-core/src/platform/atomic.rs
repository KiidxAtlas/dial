//! Atomic publication with distinct workspace, checkpoint and owner-private policies.
use crate::platform::fs as platform_fs;
use anyhow::{Context, Result, ensure};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Workspace replacement follows existing symlinks and preserves permissions. New files use
/// ordinary creation permissions (including the umask), not private-state permissions.
pub fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
pub fn write_checkpoint(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    platform_fs::set_mode(temp.path(), mode)?;
    temp.as_file().sync_all()?;
    publish(temp.as_file(), temp.path(), path, DirectorySync::BestEffort)
}

/// Validate ancestors without creating directories or changing permissions (also used for reads).
pub fn validate_private_path(dir: &Path) -> Result<()> {
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
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
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

/// Read-side twin of [`PRIVATE_LIMIT`]: what may be published may be read back.
const MAX_FILE_BYTES: u64 = PRIVATE_LIMIT as u64;

pub fn private_dir(dir: &Path) -> Result<()> {
    validate_private_path(dir)?;
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "private storage directory is not a regular directory"
            );
            ensure!(
                platform_fs::path_owned_by_current_user(dir)?,
                "private storage directory has another owner"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = dir.parent().context("private directory has no parent")?;
            if !parent.exists() {
                private_dir(parent)?;
            }
            match platform_fs::create_dir_owner_only(&fs::DirBuilder::new(), dir) {
                Ok(()) => {}
                // Another owner thread/process may create the shared store concurrently.
                // Re-enter validation; never treat an arbitrary pre-existing path as trusted.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return private_dir(dir);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(e) => return Err(e.into()),
    }
    platform_fs::set_owner_only(dir)?;
    Ok(())
}

pub fn check_file(file: &File) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && platform_fs::link_count(file)? == 1
            && platform_fs::owned_by_current_user(file)?,
        "private storage target must be an owner-owned regular file without hard links"
    );
    if !platform_fs::is_owner_only(file)? {
        platform_fs::set_owner_only_file(file)?;
    }
    Ok(())
}

pub fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    validate_private_path(path.parent().context("storage path has no parent")?)?;
    let file = match platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    check_file(&file)?;
    ensure!(
        file.metadata()?.len() <= MAX_FILE_BYTES,
        "private storage file exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "private storage file exceeds size limit"
    );
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests;
