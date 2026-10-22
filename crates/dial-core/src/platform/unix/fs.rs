use crate::platform::FileIdentity;
use std::ffi::CString;
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

pub(in crate::platform) fn open_nofollow(
    options: &OpenOptions,
    path: &Path,
    nonblocking: bool,
) -> io::Result<File> {
    let mut options = options.clone();
    let extra = if nonblocking { libc::O_NONBLOCK } else { 0 };
    options
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | extra)
        .open(path)
}

pub(in crate::platform) fn open_dir_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub(in crate::platform) fn open_beneath(
    root: &Path,
    relative: &Path,
    directory: bool,
) -> io::Result<File> {
    let mut file = open_dir_nofollow(root)?;
    let parts: Vec<_> = relative.components().collect();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path beneath a root must contain plain names only",
            ));
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if index + 1 < parts.len() || directory {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: name is NUL-terminated and `file` is an open directory descriptor.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor owned exactly once.
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

pub(in crate::platform) fn open_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
    let mut parts = relative.components();
    let (Some(Component::Normal(name)), None) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handle-relative open requires one plain name",
        ));
    };
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: name is NUL-terminated and `root` is an open directory descriptor.
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor owned exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(in crate::platform) fn owned_by_current_user(file: &File) -> io::Result<bool> {
    // SAFETY: geteuid has no preconditions.
    Ok(file.metadata()?.uid() == unsafe { libc::geteuid() })
}

pub(in crate::platform) fn path_owned_by_current_user(path: &Path) -> io::Result<bool> {
    // SAFETY: geteuid has no preconditions.
    Ok(std::fs::symlink_metadata(path)?.uid() == unsafe { libc::geteuid() })
}

pub(in crate::platform) fn path_owned_by_system(path: &Path) -> io::Result<bool> {
    Ok(std::fs::symlink_metadata(path)?.uid() == 0)
}

pub(in crate::platform) fn try_lock_journal(file: &File, exclusive: bool) -> io::Result<()> {
    let mode = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    // EWOULDBLOCK decodes to ErrorKind::WouldBlock.
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(in crate::platform) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.nlink())
}

pub(in crate::platform) fn identity(file: &File) -> io::Result<FileIdentity> {
    let meta = file.metadata()?;
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

pub(in crate::platform) fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

pub(in crate::platform) fn is_owner_only(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.mode() & 0o077 == 0)
}

pub(in crate::platform) fn set_owner_only_file(file: &File) -> io::Result<()> {
    let mode = if file.metadata()?.is_dir() {
        0o700
    } else {
        0o600
    };
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(in crate::platform) fn set_owner_only(path: &Path) -> io::Result<()> {
    let mode = if std::fs::metadata(path)?.is_dir() {
        0o700
    } else {
        0o600
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

pub(in crate::platform) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

pub(in crate::platform) fn mode(meta: &Metadata) -> u32 {
    meta.permissions().mode()
}

pub(in crate::platform) fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

pub(in crate::platform) fn open_owner_only(options: &OpenOptions, path: &Path) -> io::Result<File> {
    let mut options = options.clone();
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private file must be a regular file",
        ));
    }
    Ok(file)
}

pub(in crate::platform) fn create_dir_owner_only(
    builder: &DirBuilder,
    path: &Path,
) -> io::Result<()> {
    let mut builder = clone_builder(builder);
    builder.mode(0o700).create(path)
}

/// `DirBuilder` is not `Clone`; its only portable state is `recursive`, which `create`
/// observably needs. Recover it from the builder's `Debug` output.
fn clone_builder(builder: &DirBuilder) -> DirBuilder {
    let mut copy = DirBuilder::new();
    copy.recursive(format!("{builder:?}").contains("recursive: true"));
    copy
}
