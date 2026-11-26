use super::*;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;
use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

/// Canonicalize the parent, not the leaf: the backup need not exist. This also supplies a
/// verbatim path for installs exceeding MAX_PATH without relying on process-wide opt-in.
fn wide(path: &Path) -> Result<Vec<u16>> {
    let parent = path.parent().context("update path has no directory")?;
    let path = parent
        .canonicalize()?
        .join(path.file_name().context("update path has no name")?);
    let mut wide: Vec<_> = path.as_os_str().encode_wide().collect();
    ensure!(!wide.contains(&0), "update path contains a NUL");
    wide.push(0);
    Ok(wide)
}

/// Unlike copying over or unlinking a running .exe, renaming the old image to the backup
/// preserves existing processes' mappings. ReplaceFileW also preserves the installed DACL.
/// Never remove the active name to work around an access or sharing failure.
pub(super) fn replace(executable: &Path, staged: &Path, backup: &Path) -> Result<()> {
    let active = wide(executable)?;
    let new = wide(staged)?;
    let old = wide(backup)?;
    // SAFETY: all three path buffers are NUL-terminated UTF-16 and live through the call;
    // no flags are requested and both reserved pointers must be null.
    let ok = unsafe {
        ReplaceFileW(
            active.as_ptr(),
            new.as_ptr(),
            old.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok != 0 {
        return Ok(());
    }
    recover_failed_replace(io::Error::last_os_error(), executable, backup)
}

pub(super) fn recover_failed_replace(
    error: io::Error,
    executable: &Path,
    backup: &Path,
) -> Result<()> {
    // With a non-null backup, 1177 is the documented partial failure: Windows has moved the
    // original to the backup but could not publish the candidate. Other errors leave the two
    // original names intact. Do not overwrite a name that someone else created during recovery.
    if error.raw_os_error() == Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32) {
        if let Err(restore) = dial::platform::fs::rename_noreplace(backup, executable) {
            bail!(
                "replacing the executable failed ({error}); restoring it also failed ({restore}); the previous executable is kept at {}",
                backup.display()
            );
        }
        return Err(error).context("replacing the executable failed; previous version restored");
    }
    Err(error).with_context(|| format!("could not replace {}; check directory permissions and close other programs holding the executable or its backup before retrying", executable.display()))
}
