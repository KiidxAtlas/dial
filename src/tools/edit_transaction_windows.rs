//! Handle-relative, rollback-atomic edit transactions on Windows.
use crate::tools::Workspace;
use std::{
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
    file: crate::platform::FileIdentity,
    len: u64,
    modified: std::time::SystemTime,
    mode: u32,
}
impl Identity {
    fn of(file: &File, meta: &fs::Metadata) -> Result<Self, String> {
        Ok(Self {
            file: crate::platform::fs::identity(file).map_err(|e| e.to_string())?,
            len: meta.len(),
            modified: meta.modified().map_err(|e| e.to_string())?,
            mode: crate::platform::fs::mode(meta),
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
    pub projection: Option<crate::tools::edit_source::Projection>,
    permissions: Option<fs::Permissions>,
}
pub(crate) fn inherit_permissions(change: &mut Change, original: &Original) {
    change.permissions = Some(original.permissions.clone());
}

fn name(path: &Path) -> Result<PathBuf, String> {
    let name = path.file_name().ok_or("edit target has no filename")?;
    if name.to_str().is_none() {
        return Err("Windows edit paths must be Unicode".into());
    }
    Ok(PathBuf::from(name))
}
fn err(action: &str, path: &Path, error: impl std::fmt::Display) -> String {
    format!("{action} {}: {error}", path.display())
}
struct CreatedParent {
    parent: File,
    name: PathBuf,
    identity: crate::platform::FileIdentity,
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
            if let Ok(file) = crate::platform::fs::open_beneath_handle(&entry.parent, &entry.name) {
                if crate::platform::fs::identity(&file).ok() == Some(entry.identity) {
                    let _ = crate::platform::fs::remove_dir_beneath(&entry.parent, &entry.name);
                }
            }
        }
    }
}
struct Directory {
    file: File,
    path: PathBuf,
}
impl Directory {
    fn open(
        ws: &Workspace,
        target: &Path,
        create: bool,
        mut parents: Option<&mut ParentCleanup>,
    ) -> Result<Self, String> {
        let relative = target
            .strip_prefix(ws.root())
            .map_err(|_| "edit target escapes workspace")?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let mut held = if create {
            crate::platform::fs::open_dir_nofollow_write(ws.root())
        } else {
            crate::platform::fs::open_dir_nofollow(ws.root())
        }
        .map_err(|e| err("opening workspace", ws.root(), e))?;
        let mut path = ws.root().to_owned();
        for component in parent.components() {
            let Component::Normal(part) = component else {
                return Err("edit path has a non-normal component".into());
            };
            if part.to_str().is_none() {
                return Err("Windows edit paths must be Unicode".into());
            }
            let part = PathBuf::from(part);
            path.push(&part);
            let (next, created) = match crate::platform::fs::open_dir_beneath_handle(&held, &part) {
                Ok(file) => (file, false),
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let created = match crate::platform::fs::create_dir_beneath(&held, &part) {
                        Ok(()) => true,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                        Err(e) => return Err(err("creating edit directory", &path, e)),
                    };
                    (
                        crate::platform::fs::open_dir_beneath_handle(&held, &part)
                            .map_err(|e| err("opening no-follow edit directory", &path, e))?,
                        created,
                    )
                }
                Err(e) => return Err(err("opening no-follow edit directory", &path, e)),
            };
            if created && let Some(cleanup) = parents.as_deref_mut() {
                let identity = crate::platform::fs::identity(&next).map_err(|e| e.to_string())?;
                cleanup.entries.push(CreatedParent {
                    parent: held.try_clone().map_err(|e| e.to_string())?,
                    name: part.clone(),
                    identity,
                });
            }
            held = if create {
                crate::platform::fs::open_dir_beneath_handle_write(&held, &part)
            } else {
                Ok(next)
            }
            .map_err(|e| err("opening edit directory handle", &path, e))?;
        }
        Ok(Self { file: held, path })
    }
    fn validate_location(&self, ws: &Workspace, target: &Path) -> Result<(), String> {
        let current = Self::open(ws, target, false, None)?;
        if crate::platform::fs::identity(&self.file).ok()
            != crate::platform::fs::identity(&current.file).ok()
        {
            return Err(format!(
                "{} parent directory changed during transaction",
                ws.rel(target)
            ));
        }
        Ok(())
    }
    fn read(&self, name: &Path) -> Result<Option<Original>, String> {
        let mut file = match crate::platform::fs::open_beneath_handle(&self.file, name) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(err("opening edit target", &self.path.join(name), e)),
        };
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > MAX_FILE as u64 {
            return Err("edit target must be a regular file no larger than 4 MiB".into());
        }
        let identity = Identity::of(&file, &meta)?;
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        (&mut file)
            .take(MAX_FILE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FILE || bytes.contains(&0) {
            return Err("edit target exceeds budget or is binary".into());
        }
        if Identity::of(&file, &file.metadata().map_err(|e| e.to_string())?)? != identity {
            return Err("edit target changed while it was being read".into());
        }
        Ok(Some(Original {
            text: String::from_utf8(bytes).map_err(|_| "edit target is not UTF-8")?,
            permissions: meta.permissions(),
            identity,
        }))
    }
    fn link(&self, from: &Path, to: &Path) -> Result<(), String> {
        crate::platform::fs::link_beneath(&self.file, from, to)
            .map_err(|e| err("exclusively installing edit", &self.path, e))
    }
    fn unlink(&self, name: &Path) -> Result<(), String> {
        crate::platform::fs::remove_beneath(&self.file, name)
            .map_err(|e| err("removing transaction entry", &self.path, e))
    }
    fn capture(&self, source: &Path, kind: &str) -> Result<PathBuf, String> {
        for _ in 0..16 {
            let target = PathBuf::from(format!(
                ".dial-edit-{}-{}-{kind}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match crate::platform::fs::rename_beneath(&self.file, source, &target) {
                Ok(()) => return Ok(target),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(err("capturing transaction entry", &self.path, e)),
            }
        }
        Err("cannot allocate unique transaction capture entry".into())
    }
    fn temp(
        &self,
        content: &str,
        permissions: Option<&fs::Permissions>,
        kind: &str,
    ) -> Result<PathBuf, String> {
        for _ in 0..16 {
            let name = PathBuf::from(format!(
                ".dial-edit-{}-{}-{kind}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match crate::platform::fs::create_new_beneath(&self.file, &name, true) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(err("staging edit", &self.path, e)),
            };
            let result = file
                .write_all(content.as_bytes())
                .and_then(|_| {
                    if let Some(p) = permissions {
                        file.set_permissions(p.clone())
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
    let target = name(path)?;
    let file = crate::platform::fs::open_beneath_handle(&dir.file, &target)
        .map_err(|e| err("opening read target", path, e))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("read target must be a regular file".into());
    }
    dir.validate_location(ws, path)?;
    Ok(file)
}
pub(crate) fn read(ws: &Workspace, path: &Path) -> Result<Option<Original>, String> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    if !parent.exists() {
        return Ok(None);
    }
    Directory::open(ws, path, false, None)?.read(&name(path)?)
}
impl Change {
    pub fn load(ws: &Workspace, path: PathBuf, after: Option<String>) -> Result<Self, String> {
        let before = read(ws, &path)?;
        let permissions = before.as_ref().map(|x| x.permissions.clone());
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
    name: PathBuf,
    new: Option<PathBuf>,
    backup: Option<PathBuf>,
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
                    a.identity.file == b.identity.file
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
        let name = name(&change.path)?;
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
            if !matches(&stage.dir.read(&stage.name)?, &change.before, false) {
                return Err(format!("{} changed during staging", ws.rel(&change.path)));
            }
            if change.before.is_some() {
                let backup = stage.dir.capture(&stage.name, "original")?;
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
        for (stage, change) in staged.iter_mut().zip(changes) {
            stage.dir.validate_location(ws, &change.path)?;
            if let Some(backup) = &stage.backup {
                if !matches(&stage.dir.read(backup)?, &change.before, true) {
                    return Err(format!(
                        "{} original changed after capture",
                        ws.rel(&change.path)
                    ));
                }
            }
            if let Some(new) = &stage.new {
                stage.dir.link(new, &stage.name)?;
                stage.installed = true;
            }
        }
        for (stage, change) in staged.iter().zip(changes) {
            stage.dir.validate_location(ws, &change.path)?;
            if let Some(backup) = &stage.backup {
                if !matches(&stage.dir.read(backup)?, &change.before, true) {
                    return Err(format!(
                        "{} original changed during installation",
                        ws.rel(&change.path)
                    ));
                }
            }
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = result {
        let mut recovery = Vec::new();
        for (stage, change) in staged.iter_mut().zip(changes).rev() {
            if stage.installed {
                let captured = match stage.dir.capture(&stage.name, "rollback") {
                    Ok(name) => name,
                    Err(e) => {
                        recovery.push(e);
                        continue;
                    }
                };
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
                    .is_some_and(|(a, b)| a.identity.file == b.identity.file && a.text == b.text);
                if !owned {
                    let restored = stage.dir.link(&captured, &stage.name);
                    if restored.is_ok() {
                        let _ = stage.dir.unlink(&captured);
                    }
                    let retained = stage.backup.as_ref().map_or(String::new(), |backup| {
                        format!(
                            "; original retained at {}",
                            stage.dir.path.join(backup).display()
                        )
                    });
                    recovery.push(format!(
                        "{} changed after installation; external content {}{retained}",
                        ws.rel(&change.path),
                        if restored.is_ok() {
                            "restored without overwrite".to_owned()
                        } else {
                            format!("retained at {}", stage.dir.path.join(&captured).display())
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
                        stage.dir.path.join(backup).display()
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
            super::super::execution::unsettled();
            format!("{error}; recovery incomplete: {}", recovery.join("; "))
        });
    }
    parents.committed = true;
    let mut warnings = Vec::new();
    for stage in &staged {
        if let Some(backup) = &stage.backup {
            if let Err(e) = stage.dir.unlink(backup) {
                warnings.push(format!(
                    "Edits committed, but original cleanup failed: {e}; retained at {}",
                    stage.dir.path.join(backup).display()
                ));
            }
        }
        if let Err(e) = stage.dir.file.sync_all() {
            warnings.push(format!(
                "Edits committed, but syncing directory failed: {e}"
            ));
        }
    }
    Ok(warnings)
}
