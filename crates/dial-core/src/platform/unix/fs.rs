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

fn plain_name(relative: &Path) -> io::Result<CString> {
    let mut parts = relative.components();
    let (Some(Component::Normal(name)), None) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handle-relative operation requires one plain name",
        ));
    };
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

pub(in crate::platform) fn open_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
    open_at(root, relative, libc::O_NONBLOCK)
}

pub(in crate::platform) fn open_dir_beneath_handle(
    root: &File,
    relative: &Path,
) -> io::Result<File> {
    open_at(root, relative, libc::O_DIRECTORY)
}

pub(in crate::platform) fn create_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
    let name = plain_name(relative)?;
    // SAFETY: root keeps the parent descriptor live; name is one NUL-terminated component,
    // and mkdirat reads it synchronously with a valid owner-only permission mask.
    if unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(in crate::platform) fn create_new_beneath(
    root: &File,
    relative: &Path,
    owner_only: bool,
) -> io::Result<File> {
    let name = plain_name(relative)?;
    // SAFETY: root pins the parent and name is NUL-terminated. O_CREAT supplies a valid
    // variadic mode, and O_EXCL | O_NOFOLLOW prevent adopting any existing entry.
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            if owner_only { 0o600 } else { 0o666 },
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned one fresh descriptor, adopted exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(in crate::platform) fn link_beneath(
    root: &File,
    existing: &Path,
    new: &Path,
) -> io::Result<()> {
    let existing = plain_name(existing)?;
    let new = plain_name(new)?;
    // SAFETY: root pins both parent descriptors; both names are live C strings and
    // flags=0 links the entry itself, never following a source symlink.
    if unsafe {
        libc::linkat(
            root.as_raw_fd(),
            existing.as_ptr(),
            root.as_raw_fd(),
            new.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(in crate::platform) fn replace_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
    let from = plain_name(from)?;
    let to = plain_name(to)?;
    // SAFETY: root pins both parent descriptors and both names are live NUL-terminated
    // single components. renameat mutates entries without traversing either symlink.
    if unsafe {
        libc::renameat(
            root.as_raw_fd(),
            from.as_ptr(),
            root.as_raw_fd(),
            to.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(in crate::platform) fn remove_beneath(root: &File, relative: &Path) -> io::Result<()> {
    unlink_at(root, relative, 0)
}

pub(in crate::platform) fn remove_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
    unlink_at(root, relative, libc::AT_REMOVEDIR)
}

fn unlink_at(root: &File, relative: &Path, flags: libc::c_int) -> io::Result<()> {
    let name = plain_name(relative)?;
    // SAFETY: root pins the parent and name is a live NUL-terminated single component;
    // flags is either zero or AT_REMOVEDIR, and unlinkat never traverses the final symlink.
    if unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_at(root: &File, relative: &Path, extra: libc::c_int) -> io::Result<File> {
    let name = plain_name(relative)?;
    // SAFETY: root keeps the descriptor live and name is one NUL-terminated component;
    // these flags never create a file, so openat needs no variadic mode argument.
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | extra,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor owned exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(in crate::platform) fn read_dir_beneath(root: &File) -> io::Result<Vec<std::ffi::OsString>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;
    // SAFETY: root remains live and the literal is NUL-terminated. A fresh open description
    // avoids sharing a directory offset with other readers of root.
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is uniquely owned; fdopendir takes ownership only on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so fd still belongs to this function.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: this guard uniquely owns a successful fdopendir result.
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Directory(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: these libc accessors return this thread's live errno slot.
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "dragonfly"))]
        let errno = unsafe { libc::__errno_location() };
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        // SAFETY: __error returns the current thread's writable errno slot.
        let errno = unsafe { libc::__error() };
        #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
        // SAFETY: libc returns the current thread's live errno slot.
        let errno = unsafe { libc::__errno() };
        // SAFETY: errno is the current thread's writable errno slot, valid across readdir.
        unsafe { *errno = 0 };
        // SAFETY: stream is live and exclusively used here; copy d_name before the next read.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: errno remains the current thread's live slot.
            let error = unsafe { *errno };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            break;
        }
        // SAFETY: readdir returned a live dirent containing a NUL-terminated d_name.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
        }
    }
    Ok(names)
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
    // SAFETY: file owns a live descriptor; flock only changes its advisory lock state.
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
    // SAFETY: file owns a live descriptor and mode is a valid permission mask.
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

#[cfg(test)]
mod tests;
