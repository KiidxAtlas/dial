//! Locked, owner-private settings and submitted-prompt history. TOML values outside
//! the requested paths survive updates; invalid existing files are never replaced.
use super::paths::Layout;
use crate::platform::fs::OpenOptions;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HISTORY: usize = 10_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}
pub enum Change {
    Set { path: String, value: toml::Value },
    Remove { path: String },
}

/// Validate ancestors without creating directories or changing permissions (also used for reads).
pub(crate) fn validate_private_path(dir: &Path) -> Result<()> {
    ensure!(
        !dir.components()
            .any(|part| part == std::path::Component::ParentDir),
        "private storage path must not contain parent traversal"
    );
    // System-owned aliases such as macOS /var are trusted. User-owned symlink
    // ancestors are not: they could redirect private state into the workspace.
    for ancestor in dir.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(ancestor) {
            ensure!(
                trusted_alias(&meta),
                "private storage path contains a user-owned symlink"
            );
        }
    }
    Ok(())
}

pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    validate_private_path(dir)?;
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && !crate::platform::fs::is_alias(&meta),
                "private storage directory is not a regular directory"
            );
            #[cfg(unix)]
            ensure!(
                meta.uid() == unsafe { libc::geteuid() },
                "private storage directory has another owner"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = dir.parent().context("private directory has no parent")?;
            if !parent.exists() {
                private_dir(parent)?;
            }
            match fs::create_dir(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let meta = fs::symlink_metadata(dir)?;
                    ensure!(
                        meta.is_dir() && !crate::platform::fs::is_alias(&meta),
                        "unsafe private storage directory"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => return Err(e.into()),
    }
    crate::platform::fs::private(dir)?;
    Ok(())
}

fn check_file(file: &File) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && !crate::platform::fs::is_alias(&meta)
            && crate::platform::fs::single_link(file)?
            && crate::platform::fs::owned(file)?,
        "private storage target must be an owner-owned regular file without hard links"
    );
    crate::platform::fs::seal_file(file)?;
    Ok(())
}

pub(crate) fn locked(path: &Path) -> Result<File> {
    private_dir(path.parent().context("storage path has no parent")?)?;
    let lock_path = path.with_extension(format!(
        "{}.lock",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(
            crate::platform::fs::NOFOLLOW
                | crate::platform::fs::CLOEXEC
                | crate::platform::fs::NONBLOCK,
        )
        .open(lock_path)?;
    check_file(&lock)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

pub(crate) fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(
            crate::platform::fs::NOFOLLOW
                | crate::platform::fs::CLOEXEC
                | crate::platform::fs::NONBLOCK,
        )
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    check_file(&file)?;
    ensure!(
        file.metadata()?.len() <= MAX_FILE_BYTES,
        "private storage file exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "private storage file exceeds size limit"
    );
    Ok(Some(bytes))
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "private storage file exceeds size limit"
    );
    if fs::symlink_metadata(path).is_ok() {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(crate::platform::fs::NOFOLLOW)
            .open(path)?;
        check_file(&file)?;
    }
    let dir = path.parent().context("storage path has no parent")?;
    private_dir(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    crate::platform::fs::seal_file(temp.as_file())?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn patch(
    table: &mut toml::map::Map<String, toml::Value>,
    parts: &[&str],
    value: Option<&toml::Value>,
) -> Result<()> {
    let key = parts[0];
    if parts.len() == 1 {
        if let Some(value) = value {
            table.insert(key.into(), value.clone());
        } else {
            table.remove(key);
        }
        return Ok(());
    }
    if !table.contains_key(key) {
        if value.is_none() {
            return Ok(());
        }
        table.insert(key.into(), toml::Value::Table(Default::default()));
    }
    let child = table
        .get_mut(key)
        .and_then(toml::Value::as_table_mut)
        .context("settings path crosses a non-table value")?;
    patch(child, &parts[1..], value)
}

fn changed_document(path: &Path, changes: &[Change]) -> Result<toml::Table> {
    let mut doc = match read_private(path)? {
        Some(bytes) => std::str::from_utf8(&bytes)
            .context("settings are not UTF-8")?
            .parse::<toml::Table>()
            .context("existing settings TOML is invalid; no changes saved")?,
        None => toml::Table::new(),
    };
    for change in changes {
        let (path, value) = match change {
            Change::Set { path, value } => (path, Some(value)),
            Change::Remove { path } => (path, None),
        };
        let parts: Vec<_> = path.split('.').collect();
        ensure!(
            parts.iter().all(|p| !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')),
            "settings path must contain dotted bare table keys"
        );
        patch(&mut doc, &parts, value)?;
    }
    Ok(doc)
}

pub fn save(layout: &Layout, scope: Scope, changes: &[Change]) -> Result<()> {
    layout.ensure_state_outside_project()?;
    ensure!(
        !layout.user_dir.starts_with(&layout.project_root),
        "global storage must be outside the project"
    );
    let path = layout.config_files()[if scope == Scope::Global { 0 } else { 1 }].clone();
    let _lock = locked(&path)?;
    let doc = changed_document(&path, changes)?;
    write_private(&path, toml::to_string_pretty(&doc)?.as_bytes())
}

/// Re-read, apply Set and Remove, validate the future merged configuration, then atomically save.
pub fn save_config(
    layout: &Layout,
    scope: Scope,
    changes: &[Change],
) -> Result<crate::config::types::Config> {
    layout.ensure_state_outside_project()?;
    let files = layout.config_files();
    let selected = if scope == Scope::Global { 0 } else { 1 };
    let _lock = locked(&files[selected])?;
    let mut doc = changed_document(&files[selected], changes)?;
    crate::config::migrate_memory_tool_table(&mut doc);
    let mut merged = toml::Table::new();
    for (index, path) in files.iter().enumerate() {
        let layer = if index == selected {
            doc.clone()
        } else {
            match read_private(path)? {
                Some(bytes) => std::str::from_utf8(&bytes)?.parse::<toml::Table>()?,
                None => toml::Table::new(),
            }
        };
        crate::config::merge(&mut merged, layer);
    }
    let config = crate::config::types::Config::from_table(layout, merged)?;
    write_private(&files[selected], toml::to_string_pretty(&doc)?.as_bytes())?;
    Ok(config)
}

/// One config layer as stored on disk (empty when the file does not exist). Used to show whether a
/// value comes from the global file, the project file, or the built-in default.
pub fn load_layer(layout: &Layout, scope: Scope) -> Result<toml::Table> {
    let path = &layout.config_files()[if scope == Scope::Global { 0 } else { 1 }];
    match read_private(path)? {
        Some(bytes) => Ok(std::str::from_utf8(&bytes)
            .context("settings are not UTF-8")?
            .parse::<toml::Table>()
            .context("settings TOML is invalid")?),
        None => Ok(toml::Table::new()),
    }
}

// Remember exactly what this process loaded/saved, so another session's appended
// submissions are preserved without mistaking its records for our new submissions.
static HISTORY_BASELINES: LazyLock<Mutex<BTreeMap<PathBuf, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
fn history_path(layout: &Layout) -> Result<PathBuf> {
    layout.ensure_state_outside_project()?;
    Ok(layout.state_dir.join("prompt-history.json"))
}
fn history_read(path: &Path) -> Result<Vec<String>> {
    match read_private(path)? {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("submitted prompt history is invalid JSON")),
    }
}
fn bound(history: &mut Vec<String>, limit: usize) {
    history.retain(|s| !s.trim().is_empty());
    history.dedup();
    let excess = history.len().saturating_sub(limit.min(MAX_HISTORY));
    history.drain(..excess);
}
pub fn load_history(layout: &Layout, limit: usize) -> Result<Vec<String>> {
    let path = history_path(layout)?;
    let mut baselines = HISTORY_BASELINES
        .lock()
        .map_err(|_| anyhow::anyhow!("history lock is poisoned"))?;
    let _lock = locked(&path)?;
    let mut history = history_read(&path)?;
    bound(&mut history, limit);
    baselines.insert(path, history.clone());
    Ok(history)
}
pub fn save_history(layout: &Layout, history: &[String], limit: usize) -> Result<()> {
    let path = history_path(layout)?;
    let mut baselines = HISTORY_BASELINES
        .lock()
        .map_err(|_| anyhow::anyhow!("history lock is poisoned"))?;
    let _lock = locked(&path)?;
    let mut disk = history_read(&path)?;
    let baseline = baselines.get(&path).map(Vec::as_slice).unwrap_or(&disk);
    // Editors may have trimmed the oldest entries. Match their retained prefix
    // against a suffix of the baseline, then append only new submissions.
    let overlap = (0..=baseline.len().min(history.len()))
        .rev()
        .find(|&n| baseline[baseline.len() - n..] == history[..n])
        .unwrap_or(0);
    disk.extend(history[overlap..].iter().cloned());
    bound(&mut disk, limit);
    write_private(&path, &serde_json::to_vec(&disk)?)?;
    baselines.insert(path, history.to_vec());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        (home, project, layout)
    }
    #[test]
    fn patches_preserve_disjoint_keys_and_fail_closed() {
        let (_h, _p, l) = fixture();
        fs::write(
            l.config_files()[0].clone(),
            "unrelated = 42\n[model]\nbase_url = 'http://localhost:1234'\nname = 'old'\n",
        )
        .unwrap();
        save(
            &l,
            Scope::Global,
            &[
                Change::Set {
                    path: "ui.theme".into(),
                    value: toml::Value::String("light".into()),
                },
                Change::Remove {
                    path: "model.base_url".into(),
                },
            ],
        )
        .unwrap();
        let text = fs::read_to_string(&l.config_files()[0]).unwrap();
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(parsed["unrelated"].as_integer(), Some(42));
        assert!(parsed["model"].get("base_url").is_none());
        assert_eq!(parsed["model"]["name"].as_str(), Some("old"));
        fs::write(&l.config_files()[0], "[broken").unwrap();
        assert!(save(&l, Scope::Global, &[]).is_err());
        assert_eq!(fs::read_to_string(&l.config_files()[0]).unwrap(), "[broken");
    }
    #[test]
    fn history_roundtrips_and_preserves_external_appends() {
        let (_h, _p, l) = fixture();
        load_history(&l, 5).unwrap();
        save_history(&l, &["a\n雪".into()], 5).unwrap();
        fs::write(
            history_path(&l).unwrap(),
            serde_json::to_vec(&vec!["a\n雪", "external"]).unwrap(),
        )
        .unwrap();
        save_history(&l, &["a\n雪".into(), "new".into(), "new".into()], 5).unwrap();
        assert_eq!(load_history(&l, 5).unwrap(), ["a\n雪", "external", "new"]);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(history_path(&l).unwrap()).unwrap().mode() & 0o777,
            0o600
        );
        save_history(&l, &["external".into(), "new".into(), "last".into()], 2).unwrap();
        assert_eq!(load_history(&l, 2).unwrap(), ["new", "last"]);
    }
    #[test]
    #[cfg(unix)]
    fn refuses_symlink_and_hardlink_targets() {
        let (_h, _p, l) = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), &l.config_files()[0]).unwrap();
        assert!(save(&l, Scope::Global, &[]).is_err());
        fs::remove_file(&l.config_files()[0]).unwrap();
        fs::hard_link(outside.path(), &l.config_files()[0]).unwrap();
        assert!(save(&l, Scope::Global, &[]).is_err());
    }
}

fn trusted_alias(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        !meta.file_type().is_symlink() || meta.uid() == 0
    }
    #[cfg(windows)]
    {
        !crate::platform::fs::is_alias(meta)
    }
}
