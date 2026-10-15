//! Whole-file replacement that a crash or a concurrent reader cannot observe half-written.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Replaces `path` with `bytes`. The bytes go to a synced temp file in the target's directory,
/// which is then renamed over the target, so readers see the old file or the new one, never a
/// truncated one. A symlink is followed, so the link itself survives. An existing file keeps its
/// permissions; a new file gets the default mode (the umask applies), not the 0600 that
/// `tempfile` creates.
pub(crate) fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mode = fs::metadata(&target).ok().map(|meta| meta.permissions());
    let temp = temp_path(&target);
    let result = write_temp_then_rename(&temp, &target, bytes, mode);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    // Persist the rename itself. Directory fsync is not available everywhere; a failure here
    // does not undo a rename that already succeeded.
    if let Ok(dir_handle) = File::open(dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

fn write_temp_then_rename(
    temp: &Path,
    target: &Path,
    bytes: &[u8],
    mode: Option<fs::Permissions>,
) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(bytes)?;
    if let Some(mode) = mode {
        file.set_permissions(mode)?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(temp, target)
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
