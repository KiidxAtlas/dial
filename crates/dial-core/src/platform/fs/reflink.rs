//! Per-file copy-on-write cloning for isolated workspace copies.
//!
//! A clone shares the file's data blocks with the source until either side is written, so copying
//! a large tree costs metadata only. macOS uses `clonefile(2)` (APFS); Linux opens a new
//! destination and issues the `FICLONE` ioctl (btrfs, XFS with reflinks, bcachefs, ...). Every
//! failure of the clone path, whatever its cause (unsupported filesystem, different devices,
//! existing destination, permissions), falls back to an ordinary byte copy, which then reports
//! the real error if the copy cannot succeed either. The result is identical either way: the
//! destination has the source's contents and permissions and never shares later writes.
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static CLONED: AtomicU64 = AtomicU64::new(0);
static COPIED: AtomicU64 = AtomicU64::new(0);

/// How one file reached its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyMethod {
    /// Copy-on-write clone.
    Cloned,
    /// Ordinary read and write, after the clone path was unavailable or failed.
    Copied,
}

/// Files [`copy_on_write`] has placed in this process, by method. A debug aid for judging whether
/// isolation copies are actually cheap on the current filesystem; take two readings and subtract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyCounts {
    pub cloned: u64,
    pub copied: u64,
}

/// Current process-wide [`CopyCounts`].
pub fn copy_counts() -> CopyCounts {
    CopyCounts {
        cloned: CLONED.load(Ordering::Relaxed),
        copied: COPIED.load(Ordering::Relaxed),
    }
}

/// Native clone where supported; ordinary copying is the portable fallback. An existing
/// destination is replaced.
pub fn copy_on_write(source: &Path, dest: &Path) -> io::Result<()> {
    copy_with(source, dest, native_clone).map(|_| ())
}

fn copy_with(
    source: &Path,
    dest: &Path,
    clone: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<CopyMethod> {
    if clone(source, dest).is_ok() {
        CLONED.fetch_add(1, Ordering::Relaxed);
        return Ok(CopyMethod::Cloned);
    }
    std::fs::copy(source, dest)?;
    COPIED.fetch_add(1, Ordering::Relaxed);
    Ok(CopyMethod::Copied)
}

#[cfg(target_os = "macos")]
fn native_clone(source: &Path, dest: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let target = CString::new(dest.as_os_str().as_bytes())?;
    // SAFETY: both pointers come from live NUL-terminated `CString`s that outlive the call, and
    // `clonefile` only reads them. It fails with EEXIST rather than replacing `dest`.
    if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn native_clone(source: &Path, dest: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let input = std::fs::File::open(source)?;
    // `create_new` refuses an existing destination, so the fallback copy replaces it instead. A
    // refused clone leaves this new empty file for that copy to fill.
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    // SAFETY: both descriptors are open and owned by this function for the whole call, and
    // `FICLONE` takes the source descriptor by value without reading or writing memory.
    if unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, input.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    std::fs::set_permissions(dest, input.metadata()?.permissions())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_clone(_source: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests;
