//! Private file creation and native permission/alias checks.
#[cfg(unix)]
pub use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    fs::{self, File, Metadata, Permissions},
    io,
    path::Path,
};
#[cfg(unix)]
pub const NOFOLLOW: i32 = libc::O_NOFOLLOW;
#[cfg(unix)]
pub const CLOEXEC: i32 = libc::O_CLOEXEC;
#[cfg(unix)]
pub const NONBLOCK: i32 = libc::O_NONBLOCK;
#[cfg(windows)]
pub const NOFOLLOW: i32 = 0x00200000;
#[cfg(windows)]
pub const CLOEXEC: i32 = 0;
#[cfg(windows)]
pub const NONBLOCK: i32 = 0;

pub struct OpenOptions {
    inner: fs::OpenOptions,
    mode: Option<u32>,
    flags: i32,
    read: bool,
    write: bool,
    append: bool,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl OpenOptions {
    pub fn new() -> Self {
        Self {
            inner: fs::OpenOptions::new(),
            mode: None,
            flags: 0,
            read: false,
            write: false,
            append: false,
        }
    }
    pub fn read(&mut self, value: bool) -> &mut Self {
        self.read = value;
        self.inner.read(value);
        self
    }
    pub fn write(&mut self, value: bool) -> &mut Self {
        self.write = value;
        self.inner.write(value);
        self
    }
    pub fn append(&mut self, value: bool) -> &mut Self {
        self.append = value;
        self.inner.append(value);
        self
    }
    pub fn truncate(&mut self, value: bool) -> &mut Self {
        self.inner.truncate(value);
        self
    }
    pub fn create(&mut self, value: bool) -> &mut Self {
        self.inner.create(value);
        self
    }
    pub fn create_new(&mut self, value: bool) -> &mut Self {
        self.inner.create_new(value);
        self
    }
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = Some(mode);
        self
    }
    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.flags = flags;
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let mut options = self.inner.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(mode) = self.mode {
                options.mode(mode);
            }
            options.custom_flags(self.flags);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(self.flags as u32);
            if self.mode.is_some() {
                use windows_sys::Win32::{
                    Foundation::{GENERIC_READ, GENERIC_WRITE},
                    Storage::FileSystem::{FILE_APPEND_DATA, READ_CONTROL, WRITE_DAC},
                };
                let mut access = READ_CONTROL | WRITE_DAC;
                if self.read {
                    access |= GENERIC_READ;
                }
                if self.append {
                    access |= FILE_APPEND_DATA;
                } else if self.write {
                    access |= GENERIC_WRITE;
                }
                options.access_mode(access);
            }
        }
        let file = options.open(path)?;
        if self.flags & NOFOLLOW != 0
            && (!file.metadata()?.is_file() || is_alias(&file.metadata()?))
        {
            return Err(io::Error::other("expected regular file without aliases"));
        }
        #[cfg(windows)]
        if self.mode.is_some() {
            windows::seal_handle(&file)?;
        }
        Ok(file)
    }
}

pub fn is_alias(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
}
pub fn set_mode(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path.as_ref())?.permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, permissions)
    }
}
pub fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        file.set_permissions(Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        file.set_permissions(permissions)
    }
}
pub fn private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        set_mode(path, if path.is_dir() { 0o700 } else { 0o600 })
    }
    #[cfg(windows)]
    {
        windows::seal_handle(&windows::open_security(path)?)
    }
}
pub fn owned(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        Ok(file.metadata()?.uid() == unsafe { libc::geteuid() })
    }
    #[cfg(windows)]
    {
        windows::owned(file)
    }
}
pub fn single_link(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        Ok(file.metadata()?.nlink() == 1)
    }
    #[cfg(windows)]
    {
        windows::single_link(file)
    }
}
pub fn seal_file(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        set_file_mode(file, 0o600)
    }
    #[cfg(windows)]
    {
        windows::seal(file)
    }
}
#[cfg(windows)]
pub trait PermissionsExt {
    fn mode(&self) -> u32;
}
#[cfg(windows)]
impl PermissionsExt for Permissions {
    fn mode(&self) -> u32 {
        if self.readonly() { 0o444 } else { 0o666 }
    }
}

#[cfg(windows)]
#[path = "fs_windows.rs"]
mod windows;

/// fs2 reports Windows byte-range lock contention as ERROR_LOCK_VIOLATION,
/// whose std::io kind is not WouldBlock on every Rust version.
pub fn lock_contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}
