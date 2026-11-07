pub use super::FileIdentity;
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

mod reflink;
mod rename;
pub use reflink::{CopyCounts, copy_counts, copy_on_write};
pub use rename::rename_noreplace;

/// Flush `file`'s data and metadata with the platform's ordinary `fsync`. Apple's
/// `File::sync_all` issues `F_FULLFSYNC`, which also drains the drive's volatile write cache and
/// costs milliseconds per call; plain `fsync` keeps process- and kernel-crash durability but on
/// Apple not power-loss durability of that cache. Elsewhere this is `File::sync_all`.
pub fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: the descriptor belongs to `file`, which is borrowed for the whole call;
            // `fsync` neither closes it nor retains it.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_all()
    }
}

/// [`sync_file`] for data (and the metadata needed to read it back). Apple's
/// `File::sync_data` is also `F_FULLFSYNC`, so it uses `fsync`; elsewhere this is
/// `File::sync_data` (`fdatasync`).
pub fn sync_file_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        sync_file(file)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_data()
    }
}

/// Open `path` with `options`, refusing a symbolic link (or, on Windows, a junction or other
/// name-surrogate reparse point) as the final component. Unix: `O_NOFOLLOW | O_CLOEXEC`.
pub fn open_nofollow(options: &OpenOptions, path: &Path) -> io::Result<File> {
    super::imp::fs::open_nofollow(options, path, false)
}

/// [`open_nofollow`] that also never blocks on a FIFO or device (Unix adds `O_NONBLOCK`; Windows
/// has no filesystem FIFOs). Callers must still check that the result is a regular file.
pub fn open_nofollow_nonblocking(options: &OpenOptions, path: &Path) -> io::Result<File> {
    super::imp::fs::open_nofollow(options, path, true)
}

/// Open the directory `path` read-only without following a final symlink
/// (Unix: `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`).
pub fn open_dir_nofollow(path: &Path) -> io::Result<File> {
    super::imp::fs::open_dir_nofollow(path)
}

#[cfg(windows)]
pub fn open_dir_nofollow_write(path: &Path) -> io::Result<File> {
    super::imp::fs::open_dir_nofollow_write(path)
}

/// Open `relative` beneath the directory `root` one component at a time, never following a
/// symlink anywhere along the way (Unix: an `openat` walk with `O_NOFOLLOW`; Windows:
/// handle-relative `NtCreateFile` with `FILE_OPEN_REPARSE_POINT`). `relative` must consist of
/// plain names only. Intermediate components must be directories; the last one must be a
/// directory when `directory` is set. Files are opened read-only and non-blocking.
pub fn open_beneath(root: &Path, relative: &Path, directory: bool) -> io::Result<File> {
    super::imp::fs::open_beneath(root, relative, directory)
}
/// Create a new file relative to an already-open directory handle; fails if the name exists.
/// Unix requires a single plain name, uses write-only access, and applies mode `0o600` when
/// `owner_only` is set or `0o666` otherwise (both subject to the process umask).
pub fn create_new_beneath(root: &File, relative: &Path, owner_only: bool) -> io::Result<File> {
    super::imp::fs::create_new_beneath(root, relative, owner_only)
}

/// Create a hard link under a pinned directory, failing if the new name already exists.
/// Unix requires two plain names and links the source itself rather than following a symlink.
pub fn link_beneath(root: &File, existing: &Path, new: &Path) -> io::Result<()> {
    super::imp::fs::link_beneath(root, existing, new)
}

#[cfg(windows)]
pub fn rename_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
    super::imp::fs::rename_beneath(root, from, to)
}

/// Rename one entry under a pinned directory, replacing the destination if it exists.
/// Unix requires two plain names; neither the source nor destination symlink is followed.
pub fn replace_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
    super::imp::fs::replace_beneath(root, from, to)
}

#[cfg(windows)]
pub fn replace_beneath_file(
    root: &File,
    from: &Path,
    expected: &File,
    to: &Path,
) -> io::Result<()> {
    super::imp::fs::replace_beneath_file(root, from, expected, to)
}

/// Remove a non-directory entry under a pinned directory without following its symlink.
/// Unix requires a single plain name.
pub fn remove_beneath(root: &File, relative: &Path) -> io::Result<()> {
    super::imp::fs::remove_beneath(root, relative)
}

/// Create an owner-only directory under an open directory handle, refusing existing names.
/// Unix requires a single plain name; no symlink is followed.
pub fn create_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
    super::imp::fs::create_dir_beneath(root, relative)
}

/// Remove an empty directory entry under a pinned parent without following symlinks.
/// Unix requires a single plain name.
pub fn remove_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
    super::imp::fs::remove_dir_beneath(root, relative)
}

/// Open a plain name relative to an already-open directory handle without following a final
/// symlink or reparse point. Read-only and non-blocking.
pub fn open_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
    super::imp::fs::open_beneath_handle(root, relative)
}

/// Open a directory relative to an open directory handle without following symlinks.
/// Unix requires a single plain name.
pub fn open_dir_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
    super::imp::fs::open_dir_beneath_handle(root, relative)
}

#[cfg(windows)]
pub fn open_dir_beneath_handle_write(root: &File, relative: &Path) -> io::Result<File> {
    super::imp::fs::open_dir_beneath_handle_write(root, relative)
}

#[cfg(windows)]
pub fn open_file_beneath(root: &File, relative: &Path, write: bool) -> io::Result<File> {
    super::imp::fs::open_file_beneath(root, relative, write)
}

/// Snapshot names beneath a pinned directory; callers may mutate only after enumeration ends.
pub fn read_dir_beneath(root: &File) -> io::Result<Vec<std::ffi::OsString>> {
    super::imp::fs::read_dir_beneath(root)
}

/// Whether the open file belongs to the current user (Unix: `st_uid == geteuid()`; Windows:
/// the owner SID is the token user or the token's default owner).
pub fn owned_by_current_user(file: &File) -> io::Result<bool> {
    super::imp::fs::owned_by_current_user(file)
}

/// [`owned_by_current_user`] for a path, without following a final symlink.
pub fn path_owned_by_current_user(path: &Path) -> io::Result<bool> {
    super::imp::fs::path_owned_by_current_user(path)
}

/// Owned by the operating system (Unix root; Windows LocalSystem/Administrators), without
/// following a final symlink. Such aliases (macOS `/var`) are trusted path ancestors.
pub fn path_owned_by_system(path: &Path) -> io::Result<bool> {
    super::imp::fs::path_owned_by_system(path)
}

/// Non-blocking advisory lock for an append-only journal that other handles must still read.
/// Unix takes `flock` (compatible with `fs2`); Windows byte-range locks are mandatory, so it
/// locks one byte at an offset no journal reaches, leaving the data readable. The lock is
/// released when the handle closes. Contention is reported as `ErrorKind::WouldBlock`.
pub fn try_lock_journal(file: &File, exclusive: bool) -> io::Result<()> {
    super::imp::fs::try_lock_journal(file, exclusive)
}

/// Hard-link count of the open file.
pub fn link_count(file: &File) -> io::Result<u64> {
    super::imp::fs::link_count(file)
}

/// Identity of the open file.
pub fn identity(file: &File) -> io::Result<FileIdentity> {
    super::imp::fs::identity(file)
}

/// Identity of `path` itself (a final symlink is not followed, like `lstat`).
pub fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    super::imp::fs::path_identity(path)
}

/// Whether nobody but the owner can access the open file (Unix: `mode & 0o077 == 0`; Windows:
/// every allow ACE names the current user, SYSTEM or Administrators).
pub fn is_owner_only(file: &File) -> io::Result<bool> {
    super::imp::fs::is_owner_only(file)
}

/// Restrict `path` to its owner: Unix `0o600` for files and `0o700` for directories (symlinks
/// are followed, as `chmod` does); Windows a protected DACL granting only the current user
/// (inherited by children of a directory).
pub fn set_owner_only(path: &Path) -> io::Result<()> {
    super::imp::fs::set_owner_only(path)
}

/// Restrict permissions on an already-open file or directory handle without reopening its path.
pub fn set_owner_only_file(file: &File) -> io::Result<()> {
    super::imp::fs::set_owner_only_file(file)
}

/// Apply a POSIX mode. Windows has no group/other bits: a mode without them installs the
/// owner-only DACL; a mode without the owner write bit sets the read-only attribute; execute
/// bits have no Windows equivalent (executability comes from the file extension).
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    super::imp::fs::set_mode(path, mode)
}

/// POSIX permission bits of `meta`. Windows synthesizes them: `0o444`/`0o666` from the
/// read-only attribute, plus `0o111` for directories.
pub fn mode(meta: &Metadata) -> u32 {
    super::imp::fs::mode(meta)
}

/// Whether `path` is an executable regular file (Unix: any execute bit; Windows: an extension
/// listed in `PATHEXT`).
pub fn is_executable(path: &Path) -> bool {
    super::imp::fs::is_executable(path)
}

/// Open `path`, creating it owner-only if it does not exist (Unix: `mode(0o600)`; Windows: the
/// owner-only DACL is installed on a file this call created).
pub fn open_owner_only(options: &OpenOptions, path: &Path) -> io::Result<File> {
    super::imp::fs::open_owner_only(options, path)
}

/// Create directories like `builder.create(path)`, the new ones owner-only (Unix: `mode(0o700)`;
/// Windows: the owner-only DACL on `path` when this call created it).
pub fn create_dir_owner_only(builder: &DirBuilder, path: &Path) -> io::Result<()> {
    super::imp::fs::create_dir_owner_only(builder, path)
}
