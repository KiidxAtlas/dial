use super::super::paths::Layout;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use crate::platform::fs as platform_fs;

#[derive(Clone)]
pub struct PrivateDirectory {
    path: PathBuf,
    handle: Arc<File>,
}

fn component_name(value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value
                .chars()
                .any(|ch| matches!(ch, '/' | '\\' | ':' | '\0')),
        "invalid private memory file name"
    );
    Ok(PathBuf::from(value))
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn already_exists(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
}

fn private_directory(handle: &File, change_acl: bool) -> Result<()> {
    ensure!(
        handle.metadata()?.is_dir(),
        "private storage target is not a directory"
    );
    ensure!(
        platform_fs::owned_by_current_user(handle)?,
        "private storage directory has another owner"
    );
    if change_acl {
        platform_fs::set_owner_only_file(handle)?;
    } else {
        ensure!(
            platform_fs::is_owner_only(handle)?,
            "private storage directory has unsafe permissions"
        );
    }
    Ok(())
}

fn open_directory_path(path: &Path, create: bool) -> Result<File> {
    ensure!(path.is_absolute(), "private memory path must be absolute");
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for part in path.components() {
        match part {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => anchor.push(part.as_os_str()),
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("private memory path must not contain parent traversal")
            }
        }
    }
    ensure!(
        anchor.is_absolute(),
        "private memory path has no filesystem root"
    );
    let mut current_path = anchor.clone();
    let mut current = platform_fs::open_dir_nofollow(&anchor)?;
    for component in names {
        let relative = Path::new(&component);
        let next = match platform_fs::open_dir_beneath_handle(&current, relative) {
            Ok(next) => next,
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                let parent = platform_fs::open_dir_nofollow_write(&current_path)?;
                ensure!(
                    platform_fs::identity(&parent)? == platform_fs::identity(&current)?,
                    "private memory directory path changed"
                );
                match platform_fs::create_dir_beneath(&parent, relative) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                platform_fs::open_dir_beneath_handle(&parent, relative)?
            }
            Err(error) => return Err(error.into()),
        };
        current_path.push(&component);
        current = next;
    }
    let writable = platform_fs::open_dir_nofollow_write(&current_path)?;
    ensure!(
        platform_fs::identity(&writable)? == platform_fs::identity(&current)?,
        "private memory directory path changed"
    );
    private_directory(&writable, create)?;
    Ok(writable)
}

impl PrivateDirectory {
    fn open_mode(layout: &Layout, id: &str, create: bool) -> Result<Self> {
        let id = component_name(id)?;
        layout.ensure_project_outside_storage()?;
        let base = Self {
            path: layout.user_dir.clone(),
            handle: Arc::new(open_directory_path(&layout.user_dir, create)?),
        };
        let resolved = std::fs::canonicalize(&layout.user_dir)?;
        ensure!(
            !layout.project_root.starts_with(&resolved),
            "workspace resolves inside private memory directory"
        );
        let backends = if create {
            base.child("memory-backends")?
        } else {
            base.existing_child("memory-backends")?
        };
        let child_name = id
            .to_str()
            .context("private memory directory name is not UTF-8")?;
        if create {
            backends.child(child_name)
        } else {
            backends.existing_child(child_name)
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

    pub fn child(&self, child_name: &str) -> Result<Self> {
        let child_name = component_name(child_name)?;
        match platform_fs::create_dir_beneath(&self.handle, &child_name) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let read_handle = platform_fs::open_dir_beneath_handle(&self.handle, &child_name)?;
        let path = self.path.join(&child_name);
        let handle = platform_fs::open_dir_nofollow_write(&path)?;
        ensure!(
            platform_fs::identity(&handle)? == platform_fs::identity(&read_handle)?,
            "private memory directory path changed"
        );
        private_directory(&handle, true)?;
        self.handle.sync_all()?;
        Ok(Self {
            path,
            handle: Arc::new(handle),
        })
    }

    pub fn existing_child(&self, child_name: &str) -> Result<Self> {
        let child_name = component_name(child_name)?;
        let read_handle = platform_fs::open_dir_beneath_handle(&self.handle, &child_name)?;
        private_directory(&read_handle, false)?;
        let path = self.path.join(&child_name);
        let handle = platform_fs::open_dir_nofollow_write(&path)?;
        ensure!(
            platform_fs::identity(&handle)? == platform_fs::identity(&read_handle)?,
            "private memory directory path changed"
        );
        Ok(Self {
            path,
            handle: Arc::new(handle),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn file(&self, name: &str, write: bool, create_new: bool) -> Result<File> {
        let relative = component_name(name)?;
        let file = if create_new {
            platform_fs::create_new_beneath(&self.handle, &relative, true)?
        } else {
            platform_fs::open_file_beneath(&self.handle, &relative, write)?
        };
        ensure!(
            file.metadata()?.is_file(),
            "private memory target is not a regular file"
        );
        ensure!(
            platform_fs::owned_by_current_user(&file)? && platform_fs::link_count(&file)? == 1,
            "unsafe private memory file"
        );
        if write {
            platform_fs::set_owner_only_file(&file)?;
        } else {
            ensure!(
                platform_fs::is_owner_only(&file)?,
                "private read file has unsafe permissions"
            );
        }
        Ok(file)
    }

    fn checked_database_path(&self, name: &str, file: &File) -> Result<PathBuf> {
        let path = self.path.join(name);
        let identity = platform_fs::identity(file)?;
        ensure!(
            platform_fs::path_identity(&path)? == identity,
            "memory database path changed"
        );
        Ok(path)
    }

    pub fn database(&self, name: &str) -> Result<Connection> {
        self.database_with_journal(name, "DELETE")
    }

    /// Opens a database that stays in WAL mode; see the Unix implementation.
    pub fn database_wal(&self, name: &str) -> Result<Connection> {
        self.database_with_journal(name, "WAL")
    }

    fn database_with_journal(&self, name: &str, journal: &str) -> Result<Connection> {
        let file = match self.file(name, true, false) {
            Ok(file) => file,
            Err(error) if missing(&error) => match self.file(name, true, true) {
                Ok(file) => file,
                Err(error) if already_exists(&error) => self.file(name, true, false)?,
                Err(error) => return Err(error),
            },
            Err(error) => return Err(error),
        };
        let path = self.checked_database_path(name, &file)?;
        let identity = platform_fs::identity(&file)?;
        let connection = Connection::open(&path).context("open private backend database")?;
        ensure!(
            platform_fs::path_identity(&path)? == identity,
            "memory database path changed"
        );
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch(&format!(
            "PRAGMA foreign_keys=ON;PRAGMA journal_mode={journal};PRAGMA synchronous=FULL;"
        ))?;
        Ok(connection)
    }

    pub fn database_readonly(&self, name: &str) -> Result<Option<Connection>> {
        let file = match self.file(name, false, false) {
            Ok(file) => file,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let path = self.checked_database_path(name, &file)?;
        let identity = platform_fs::identity(&file)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        ensure!(
            platform_fs::path_identity(&path)? == identity,
            "memory database path changed"
        );
        connection.busy_timeout(std::time::Duration::from_millis(100))?;
        Ok(Some(connection))
    }

    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let name = component_name(name)?;
        let temporary = PathBuf::from(format!(".memory-{:016x}.tmp", rand::random::<u64>()));
        let file = self.file(
            temporary.to_str().context("temporary name is not UTF-8")?,
            true,
            true,
        )?;
        // The handle moves into the closure so it is closed before cleanup: Windows cannot delete
        // an open file.
        let result = (|file: std::fs::File| -> Result<()> {
            let mut file = file;
            file.write_all(bytes)?;
            file.sync_all()?;
            platform_fs::replace_beneath(&self.handle, &temporary, &name)?;
            self.handle.sync_all()?;
            Ok(())
        })(file);
        if result.is_err() {
            let _ = platform_fs::remove_beneath(&self.handle, &temporary);
        }
        result
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        match platform_fs::remove_beneath(&self.handle, &component_name(name)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn remove_tree(&self, name: &str) -> Result<()> {
        let name = component_name(name)?;
        self.remove_tree_entry(&self.handle, &name)?;
        self.handle.sync_all()?;
        Ok(())
    }

    fn remove_tree_entry(&self, parent: &File, name: &Path) -> Result<()> {
        let read_handle = match platform_fs::open_dir_beneath_handle(parent, name) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return platform_fs::remove_beneath(parent, name).map_err(Into::into);
            }
            Err(error) => return Err(error.into()),
        };
        private_directory(&read_handle, false)?;
        let directory = platform_fs::open_dir_beneath_handle_write(parent, name)?;
        ensure!(
            platform_fs::identity(&directory)? == platform_fs::identity(&read_handle)?,
            "private cleanup directory changed"
        );
        for child in platform_fs::read_dir_beneath(&directory)? {
            if child == "." || child == ".." {
                continue;
            }
            self.remove_tree_entry(&directory, Path::new(&child))?;
        }
        platform_fs::remove_dir_beneath(parent, name)?;
        Ok(())
    }

    pub fn read_head(&self, name: &str, max: usize) -> Result<(Vec<u8>, bool)> {
        let file = self.file(name, false, false)?;
        let mut bytes = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut bytes)?;
        let truncated = bytes.len() > max;
        if truncated {
            bytes.truncate(max);
        }
        Ok((bytes, truncated))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_private_files_keep_owner_only_acl_and_handle_relative_replacement() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
        let directory = PrivateDirectory::open(&layout, "test").unwrap();

        directory.write("state", b"before").unwrap();
        directory.write("state", b"after").unwrap();
        assert_eq!(directory.read_head("state", 32).unwrap().0, b"after");
        assert!(platform_fs::is_owner_only(&directory.handle).unwrap());
        assert!(
            platform_fs::is_owner_only(&File::open(directory.path.join("state")).unwrap()).unwrap()
        );

        let nested = directory.child("nested").unwrap();
        nested.write("item", b"private").unwrap();
        directory.remove_tree("nested").unwrap();
        assert!(!directory.path.join("nested").exists());
    }
}
