use super::super::paths::Layout;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};
#[derive(Clone)]
pub struct PrivateDirectory {
    path: PathBuf,
    fd: Arc<File>,
}
fn cstring(value: &std::ffi::OsStr) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(CString::new(value.as_bytes())?)
}
fn component(value: &str) -> Result<CString> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains('/')
            && !value.contains('\0'),
        "invalid private memory file name"
    );
    Ok(CString::new(value)?)
}
fn open_dir(parent: RawFd, name: &CString, create: bool) -> Result<File> {
    if create && unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}
/// Canonicalizes the deepest existing ancestor of `path` and re-appends the missing tail. This resolves
/// OS aliases such as macOS `/var` -> `/private/var`; the walk from there still rejects symlinks with `O_NOFOLLOW`.
fn resolved_prefix(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .context("memory storage path has no existing ancestor")?
                    .to_owned();
                tail.push(name);
                existing.pop();
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = std::fs::canonicalize(&existing)?;
    for name in tail.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}
impl PrivateDirectory {
    fn open_mode(layout: &Layout, id: &str, create: bool) -> Result<Self> {
        layout.ensure_project_outside_storage()?;
        ensure!(
            layout.user_dir.is_absolute(),
            "memory storage must be absolute"
        );
        let resolved = resolved_prefix(&layout.user_dir)?;
        let mut file = File::open("/")?;
        for part in resolved.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    file = open_dir(file.as_raw_fd(), &cstring(name)?, create)?
                }
                _ => anyhow::bail!("memory storage path must be normalized"),
            }
        }
        ensure!(
            file.metadata()?.uid() == unsafe { libc::geteuid() },
            "memory storage has another owner"
        );
        if create && file.metadata()?.mode() & 0o777 != 0o700 {
            ensure!(
                unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } == 0,
                "cannot secure memory directory"
            );
        }
        let base = Self {
            path: layout.user_dir.clone(),
            fd: Arc::new(file),
        };
        if create {
            base.child("memory-backends")?.child(id)
        } else {
            base.existing_child("memory-backends")?.existing_child(id)
        }
    }
    pub fn open(layout: &Layout, id: &str) -> Result<Self> {
        Self::open_mode(layout, id, true)
    }
    pub fn existing(layout: &Layout, id: &str) -> Result<Option<Self>> {
        match Self::open_mode(layout, id, false) {
            Ok(value) => Ok(Some(value)),
            Err(error) if missing(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub fn child(&self, name: &str) -> Result<Self> {
        let fd = open_dir(self.directory_fd(), &component(name)?, true)?;
        let meta = fd.metadata()?;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() },
            "private directory has another owner"
        );
        if meta.mode() & 0o777 != 0o700 {
            ensure!(
                unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } == 0,
                "cannot secure private directory"
            );
        }
        self.fd.sync_all()?;
        Ok(Self {
            path: self.path.join(name),
            fd: Arc::new(fd),
        })
    }
    pub fn existing_child(&self, name: &str) -> Result<Self> {
        let fd = open_dir(self.directory_fd(), &component(name)?, false)?;
        let meta = fd.metadata()?;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "private read directory has unsafe owner or permissions"
        );
        Ok(Self {
            path: self.path.join(name),
            fd: Arc::new(fd),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn directory_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
    fn file(&self, name: &str, flags: i32) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                self.directory_fd(),
                component(name)?.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.nlink() == 1,
            "unsafe memory file"
        );
        if flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT) != 0 {
            if meta.mode() & 0o777 != 0o600 {
                ensure!(
                    unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } == 0,
                    "cannot secure memory file"
                );
            }
        } else {
            ensure!(
                meta.mode() & 0o077 == 0,
                "private read file has unsafe permissions"
            );
        }
        Ok(file)
    }
    fn checked_database_path(&self, name: &str, file: &File) -> Result<PathBuf> {
        let path = self.path.join(name);
        let actual = std::fs::symlink_metadata(&path)?;
        let meta = file.metadata()?;
        ensure!(
            !actual.file_type().is_symlink()
                && actual.dev() == meta.dev()
                && actual.ino() == meta.ino(),
            "memory database path changed"
        );
        Ok(path)
    }
    pub fn database(&self, name: &str) -> Result<Connection> {
        self.database_with_journal(name, "DELETE")
    }
    /// Opens a database that stays in WAL mode. Unlike `database`, it never switches the file
    /// back to rollback-journal mode, which fails with "database is locked" while any other
    /// connection (another session, a maintenance thread) has the WAL database open.
    pub fn database_wal(&self, name: &str) -> Result<Connection> {
        self.database_with_journal(name, "WAL")
    }
    fn database_with_journal(&self, name: &str, journal: &str) -> Result<Connection> {
        let file = self.file(name, libc::O_RDWR | libc::O_CREAT)?;
        let connection = Connection::open(self.checked_database_path(name, &file)?)
            .context("open private backend database")?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch(&format!(
            "PRAGMA foreign_keys=ON;PRAGMA journal_mode={journal};PRAGMA synchronous=FULL;"
        ))?;
        Ok(connection)
    }
    pub fn database_readonly(&self, name: &str) -> Result<Option<Connection>> {
        let file = match self.file(name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let connection = Connection::open_with_flags(
            self.checked_database_path(name, &file)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::from_millis(100))?;
        Ok(Some(connection))
    }
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let temporary = format!(".memory-{:016x}.tmp", rand::random::<u64>());
        let mut file = self.file(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            self.rename(&temporary, name)?;
            self.fd.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.remove(&temporary);
        }
        result
    }
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        ensure!(
            unsafe {
                libc::renameat(
                    self.directory_fd(),
                    component(from)?.as_ptr(),
                    self.directory_fd(),
                    component(to)?.as_ptr(),
                )
            } == 0,
            "cannot atomically publish memory file: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    pub fn remove(&self, name: &str) -> Result<()> {
        if unsafe { libc::unlinkat(self.directory_fd(), component(name)?.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        Ok(())
    }
    /// Cleanup traverses only pinned descriptors; symlinks are unlinked, never followed.
    pub fn remove_tree(&self, name: &str) -> Result<()> {
        let child = match self.existing_child(name) {
            Ok(child) => child,
            Err(error) if missing(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        let duplicate = unsafe { libc::fcntl(child.directory_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        ensure!(
            duplicate >= 0,
            "cannot duplicate private cleanup descriptor"
        );
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| -> Result<()> {
            loop {
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    break;
                }
                let value = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let bytes = value.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let value = value.to_str().context("private cleanup entry not UTF-8")?;
                let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                ensure!(
                    unsafe {
                        libc::fstatat(
                            child.directory_fd(),
                            component(value)?.as_ptr(),
                            stat.as_mut_ptr(),
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } == 0,
                    "private cleanup entry changed"
                );
                let stat = unsafe { stat.assume_init() };
                if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    child.remove_tree(value)?;
                } else {
                    child.remove(value)?;
                }
            }
            Ok(())
        })();
        unsafe { libc::closedir(stream) };
        result?;
        ensure!(
            unsafe {
                libc::unlinkat(
                    self.directory_fd(),
                    component(name)?.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } == 0,
            "cannot remove private staging directory"
        );
        self.fd.sync_all()?;
        Ok(())
    }
    pub fn read_head(&self, name: &str, max: usize) -> Result<(Vec<u8>, bool)> {
        let file = self.file(name, libc::O_RDONLY)?;
        let mut bytes = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut bytes)?;
        let truncated = bytes.len() > max;
        if truncated {
            bytes.truncate(max);
        }
        Ok((bytes, truncated))
    }
    pub fn read(&self, name: &str, max: usize) -> Result<Option<String>> {
        let file = match self.file(name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        ensure!(
            file.metadata()?.len() <= max as u64,
            "memory file exceeds read bound"
        );
        let mut text = String::new();
        file.take(max as u64 + 1).read_to_string(&mut text)?;
        ensure!(text.len() <= max, "memory file grew beyond read bound");
        Ok(Some(text))
    }
}
