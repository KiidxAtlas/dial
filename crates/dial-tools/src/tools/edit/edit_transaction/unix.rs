//! Rollback-atomic edit transactions. Paths are opened relative to no-follow directory handles.
//! Originals are captured before installation; exclusive links never overwrite a racing writer.
//! This is not a cross-filesystem/crash transaction. Failed recovery retains named originals.
use crate::tools::Workspace;
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::{ffi::OsStrExt, fs::MetadataExt},
};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) const MAX_FILE: usize = 4 * 1024 * 1024;
pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    dev: crate::platform::FileIdentity,
    len: u64,
    modified: std::time::SystemTime,
    mode: u32,
    ctime: (i64, i64),
}
impl Identity {
    fn of(file: &File, meta: &fs::Metadata) -> Result<Self, String> {
        Ok(Self {
            dev: crate::platform::fs::identity(file).map_err(|e| e.to_string())?,
            len: meta.len(),
            modified: meta.modified().map_err(|e| e.to_string())?,
            mode: crate::platform::fs::mode(meta),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Original {
    pub text: String,
    identity: Identity,
    permissions: fs::Permissions,
}
#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub path: PathBuf,
    pub before: Option<Original>,
    pub after: Option<String>,
    pub report: Option<String>,
    pub seen_after: Option<Vec<bool>>,
    pub authored_whole: bool,
    pub projection: Option<crate::tools::edit::edit_source::Projection>,
    permissions: Option<fs::Permissions>,
}
pub(crate) fn inherit_permissions(change: &mut Change, original: &Original) {
    change.permissions = Some(original.permissions.clone());
}

fn cstr(path: &std::ffi::OsStr) -> Result<CString, String> {
    CString::new(path.as_bytes()).map_err(|_| "edit path contains NUL".into())
}
fn err(action: &str, path: &Path) -> String {
    format!(
        "{action} {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    )
}
struct Directory {
    file: File,
    path: PathBuf,
}
struct CreatedParent {
    parent: File,
    name: CString,
    dev: u64,
    ino: u64,
}
#[derive(Default)]
struct ParentCleanup {
    entries: Vec<CreatedParent>,
    committed: bool,
}
impl Drop for ParentCleanup {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for entry in self.entries.iter().rev() {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    entry.parent.as_raw_fd(),
                    entry.name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                let metadata = unsafe { metadata.assume_init() };
                #[cfg(target_os = "macos")]
                let dev = metadata.st_dev as u64;
                #[cfg(not(target_os = "macos"))]
                let dev = metadata.st_dev;
                if dev == entry.dev
                    && metadata.st_ino == entry.ino
                    && metadata.st_mode & libc::S_IFMT == libc::S_IFDIR
                {
                    // Only owned, still-empty scaffolding can disappear; concurrent data wins.
                    let _ = unsafe {
                        libc::unlinkat(
                            entry.parent.as_raw_fd(),
                            entry.name.as_ptr(),
                            libc::AT_REMOVEDIR,
                        )
                    };
                }
            }
        }
    }
}
impl Directory {
    fn open(
        ws: &Workspace,
        target: &Path,
        create: bool,
        mut parents: Option<&mut ParentCleanup>,
    ) -> Result<Self, String> {
        // Added directories are workspace roots too; confine the walk to the one holding `target`.
        let base = ws
            .root_containing(target)
            .ok_or("edit target escapes workspace")?;
        let relative = target
            .strip_prefix(base)
            .map_err(|_| "edit target escapes workspace")?;
        let root = cstr(base.as_os_str())?;
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(err("opening workspace", base));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut path = base.to_owned();
        for part in relative.parent().unwrap_or(Path::new("")).components() {
            let Component::Normal(name) = part else {
                return Err("edit path has a non-normal component".into());
            };
            let name = cstr(name)?;
            let mut next = unsafe {
                libc::openat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            let mut created = false;
            if next < 0
                && create
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                let made = unsafe { libc::mkdirat(file.as_raw_fd(), name.as_ptr(), 0o755) };
                created = made == 0;
                if made < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(err("creating edit directory", &path));
                }
                next = unsafe {
                    libc::openat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            path.push(std::ffi::OsStr::from_bytes(name.as_bytes()));
            if next < 0 {
                return Err(err("opening no-follow edit directory", &path));
            }
            let next_file = unsafe { File::from_raw_fd(next) };
            if created && let Some(parents) = parents.as_deref_mut() {
                let metadata = next_file.metadata().map_err(|e| e.to_string())?;
                parents.entries.push(CreatedParent {
                    parent: file.try_clone().map_err(|e| e.to_string())?,
                    name,
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                });
            }
            file = next_file;
        }
        Ok(Self { file, path })
    }
    fn validate_location(&self, ws: &Workspace, target: &Path) -> Result<(), String> {
        let current = Self::open(ws, target, false, None)?;
        let expected = self.file.metadata().map_err(|e| e.to_string())?;
        let actual = current.file.metadata().map_err(|e| e.to_string())?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(format!(
                "{} parent directory changed during transaction",
                ws.rel(target)
            ));
        }
        Ok(())
    }
    fn read(&self, name: &CString) -> Result<Option<Original>, String> {
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err(
                "opening edit target",
                &self.path.join(std::ffi::OsStr::from_bytes(name.as_bytes())),
            ));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > MAX_FILE as u64 {
            return Err("edit target must be a regular file no larger than 4 MiB".into());
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        (&mut file)
            .take(MAX_FILE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FILE || bytes.contains(&0) {
            return Err("edit target exceeds budget or is binary".into());
        }
        let identity = Identity::of(&file, &meta)?;
        if Identity::of(&file, &file.metadata().map_err(|e| e.to_string())?)? != identity {
            return Err("edit target changed while it was being read".into());
        }
        Ok(Some(Original {
            text: String::from_utf8(bytes).map_err(|_| "edit target is not UTF-8")?,
            permissions: meta.permissions(),
            identity,
        }))
    }
    fn link(&self, from: &CString, to: &CString) -> Result<(), String> {
        if unsafe {
            libc::linkat(
                self.file.as_raw_fd(),
                from.as_ptr(),
                self.file.as_raw_fd(),
                to.as_ptr(),
                0,
            )
        } < 0
        {
            return Err(err("exclusively installing edit", &self.path));
        }
        Ok(())
    }
    fn unlink(&self, name: &CString) -> Result<(), String> {
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(err("removing transaction entry", &self.path));
        }
        Ok(())
    }
    fn rename(&self, from: &CString, to: &CString) -> Result<(), String> {
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                from.as_ptr(),
                self.file.as_raw_fd(),
                to.as_ptr(),
            )
        } < 0
        {
            return Err(err("capturing edit original", &self.path));
        }
        Ok(())
    }
    fn temp(
        &self,
        content: &str,
        permissions: Option<&fs::Permissions>,
        kind: &str,
    ) -> Result<CString, String> {
        for _ in 0..16 {
            let name = CString::new(format!(
                ".dial-edit-{}-{}-{kind}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .unwrap();
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    // A new file gets the umask default like `creat`; existing targets and
                    // rollback staging are set explicitly or kept private.
                    if kind == "new" && permissions.is_none() {
                        0o666
                    } else {
                        0o600
                    },
                )
            };
            if fd < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(err("staging edit", &self.path));
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            let result = file
                .write_all(content.as_bytes())
                .and_then(|_| {
                    if let Some(perms) = permissions {
                        file.set_permissions(perms.clone())
                    } else {
                        Ok(())
                    }
                })
                .and_then(|_| file.sync_all());
            if let Err(e) = result {
                let _ = self.unlink(&name);
                return Err(format!("staging edit: {e}"));
            }
            return Ok(name);
        }
        Err("cannot allocate exclusive edit staging entry".into())
    }
}

pub(crate) fn open_read(ws: &Workspace, path: &Path) -> Result<File, String> {
    let dir = Directory::open(ws, path, false, None)?;
    dir.validate_location(ws, path)?;
    let name = cstr(path.file_name().ok_or("read target has no filename")?)?;
    let fd = unsafe {
        libc::openat(
            dir.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(err("opening read target", path));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("read target must be a regular file".into());
    }
    dir.validate_location(ws, path)?;
    Ok(file)
}
pub(crate) fn read(ws: &Workspace, path: &Path) -> Result<Option<Original>, String> {
    match Directory::open(ws, path, false, None) {
        Ok(dir) => dir.read(&cstr(
            path.file_name().ok_or("edit target has no filename")?,
        )?),
        Err(_) if !path.parent().is_some_and(Path::exists) => Ok(None),
        Err(e) => Err(e),
    }
}
impl Change {
    pub fn load(ws: &Workspace, path: PathBuf, after: Option<String>) -> Result<Self, String> {
        let before = read(ws, &path)?;
        let permissions = before.as_ref().map(|b| b.permissions.clone());
        Ok(Self {
            path,
            before,
            after,
            report: None,
            seen_after: None,
            authored_whole: false,
            projection: None,
            permissions,
        })
    }
    pub fn require_text(&self) -> Result<&str, String> {
        self.before
            .as_ref()
            .map(|x| x.text.as_str())
            .ok_or_else(|| format!("{} does not exist", self.path.display()))
    }
    pub fn existing(path: PathBuf, before: Original, after: Option<String>) -> Self {
        let permissions = Some(before.permissions.clone());
        Self {
            path,
            before: Some(before),
            after,
            report: None,
            seen_after: None,
            authored_whole: false,
            projection: None,
            permissions,
        }
    }
}
struct Staged {
    dir: Directory,
    name: CString,
    new: Option<CString>,
    backup: Option<CString>,
    installed: bool,
}
impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(name) = &self.new {
            let _ = self.dir.unlink(name);
        }
    }
}

fn matches(current: &Option<Original>, before: &Option<Original>, captured: bool) -> bool {
    match (current, before) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.text == b.text
                && if captured {
                    a.identity.dev == b.identity.dev
                        && a.identity.len == b.identity.len
                        && a.identity.modified == b.identity.modified
                        && a.identity.mode == b.identity.mode
                } else {
                    a.identity == b.identity
                }
        }
        _ => false,
    }
}
/// All observers and validation precede mutation. Store/register publication belongs to the caller
/// and occurs only after success. A racing create blocks installation rather than being overwritten.
pub(crate) fn commit(ws: &Workspace, changes: &[Change]) -> Result<Vec<String>, String> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    if changes.len() > 128
        || changes
            .iter()
            .map(|x| {
                x.before.as_ref().map_or(0, |b| b.text.len())
                    + x.after.as_ref().map_or(0, String::len)
            })
            .sum::<usize>()
            > MAX_BYTES
    {
        return Err("edit transaction exceeds 128 targets or 32 MiB".into());
    }
    let mut unique = std::collections::BTreeSet::new();
    for change in changes {
        if !unique.insert(&change.path) {
            return Err(format!("duplicate edit target {}", ws.rel(&change.path)));
        }
        if change.after.as_ref().is_some_and(|x| x.len() > MAX_FILE) {
            return Err("edited file exceeds 4 MiB".into());
        }
        if ws.resolve(&ws.rel(&change.path))? != change.path {
            return Err("edit target physical alias changed".into());
        }
        if !matches(&read(ws, &change.path)?, &change.before, false) {
            return Err(format!(
                "{} changed before commit; re-read it",
                ws.rel(&change.path)
            ));
        }
        if change.after.is_some() {
            ws.observe_write(&change.path)?;
        } else {
            ws.observe_delete(&change.path)?;
        }
    }
    let mut parents = ParentCleanup::default();
    let mut staged = Vec::with_capacity(changes.len());
    for change in changes {
        let dir = Directory::open(ws, &change.path, true, Some(&mut parents))?;
        let name = cstr(
            change
                .path
                .file_name()
                .ok_or("edit target has no filename")?,
        )?;
        let new = change
            .after
            .as_ref()
            .map(|text| dir.temp(text, change.permissions.as_ref(), "new"))
            .transpose()?;
        staged.push(Staged {
            dir,
            name,
            new,
            backup: None,
            installed: false,
        });
    }
    let result = (|| {
        for (stage, change) in staged.iter_mut().zip(changes) {
            stage.dir.validate_location(ws, &change.path)?;
            // Verify through the pinned directory handle, not a re-resolved pathname.
            if !matches(&stage.dir.read(&stage.name)?, &change.before, false) {
                return Err(format!("{} changed during staging", ws.rel(&change.path)));
            }
            if change.before.is_some() {
                // Reserve a unique backup name, then replace only our reservation.
                let backup = stage.dir.temp("", None, "original")?;
                if let Err(e) = stage.dir.rename(&stage.name, &backup) {
                    let _ = stage.dir.unlink(&backup);
                    return Err(e);
                }
                stage.backup = Some(backup);
                if !matches(
                    &stage.dir.read(stage.backup.as_ref().unwrap())?,
                    &change.before,
                    true,
                ) {
                    return Err(format!("{} raced original capture", ws.rel(&change.path)));
                }
            }
        }
        // Every original is safely captured before the first replacement is installed.
        for (stage, change) in staged.iter_mut().zip(changes) {
            stage.dir.validate_location(ws, &change.path)?;
            if let Some(backup) = &stage.backup
                && !matches(&stage.dir.read(backup)?, &change.before, true)
            {
                return Err(format!(
                    "{} original changed after capture",
                    ws.rel(&change.path)
                ));
            }
            if let Some(new) = &stage.new {
                stage.dir.link(new, &stage.name)?;
                stage.installed = true;
            }
        }
        for (stage, change) in staged.iter().zip(changes) {
            stage.dir.validate_location(ws, &change.path)?;
            if let Some(backup) = &stage.backup
                && !matches(&stage.dir.read(backup)?, &change.before, true)
            {
                return Err(format!(
                    "{} original changed during installation",
                    ws.rel(&change.path)
                ));
            }
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = result {
        let mut recovery = Vec::new();
        for (stage, change) in staged.iter_mut().zip(changes).rev() {
            if stage.installed {
                // Capture the name atomically BEFORE checking ownership. Never check then unlink
                // a pathname that a concurrent writer could replace between those operations.
                let captured = match stage.dir.temp("", None, "rollback") {
                    Ok(name) => name,
                    Err(e) => {
                        recovery.push(e);
                        continue;
                    }
                };
                if let Err(e) = stage.dir.rename(&stage.name, &captured) {
                    let _ = stage.dir.unlink(&captured);
                    recovery.push(e);
                    continue;
                }
                let owned = stage
                    .dir
                    .read(&captured)
                    .ok()
                    .flatten()
                    .zip(
                        stage
                            .new
                            .as_ref()
                            .and_then(|n| stage.dir.read(n).ok().flatten()),
                    )
                    .is_some_and(|(a, b)| a.identity.dev == b.identity.dev && a.text == b.text);
                if !owned {
                    let restored = stage.dir.link(&captured, &stage.name);
                    if restored.is_ok() {
                        let _ = stage.dir.unlink(&captured);
                    }
                    let retained = stage.backup.as_ref().map_or(String::new(), |backup| {
                        format!(
                            "; original retained at {}",
                            stage
                                .dir
                                .path
                                .join(std::ffi::OsStr::from_bytes(backup.as_bytes()))
                                .display()
                        )
                    });
                    recovery.push(format!(
                        "{} changed after installation; external content {}{retained}",
                        ws.rel(&change.path),
                        if restored.is_ok() {
                            "restored without overwrite".to_owned()
                        } else {
                            format!(
                                "retained at {}",
                                stage
                                    .dir
                                    .path
                                    .join(std::ffi::OsStr::from_bytes(captured.as_bytes()))
                                    .display()
                            )
                        }
                    ));
                    continue;
                }
                if let Err(e) = stage.dir.unlink(&captured) {
                    recovery.push(e);
                }
            }
            if let Some(backup) = &stage.backup {
                if let Err(e) = stage.dir.link(backup, &stage.name) {
                    recovery.push(format!(
                        "{e}; original retained at {}",
                        stage
                            .dir
                            .path
                            .join(std::ffi::OsStr::from_bytes(backup.as_bytes()))
                            .display()
                    ));
                } else {
                    let _ = stage.dir.unlink(backup);
                    stage.backup = None;
                }
            }
        }
        return Err(if recovery.is_empty() {
            format!("{error}; transaction rolled back, no edits committed")
        } else {
            crate::tools::exec::execution::unsettled();
            format!("{error}; recovery incomplete: {}", recovery.join("; "))
        });
    }
    parents.committed = true;
    let mut warnings = Vec::new();
    for stage in &staged {
        if let Some(backup) = &stage.backup
            && let Err(e) = stage.dir.unlink(backup)
        {
            warnings.push(format!(
                "Edits committed, but original cleanup failed: {e}; retained at {}",
                stage
                    .dir
                    .path
                    .join(std::ffi::OsStr::from_bytes(backup.as_bytes()))
                    .display()
            ));
        }
        if let Err(e) = stage.dir.file.sync_all() {
            warnings.push(format!(
                "Edits committed, but syncing directory failed: {e}"
            ));
        }
    }
    Ok(warnings)
}
