//! Locked, owner-private settings and submitted-prompt history. TOML values outside
//! the requested paths survive updates; invalid existing files are never replaced.
use super::paths::Layout;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::platform::fs as platform_fs;

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
                !meta.file_type().is_symlink() || platform_fs::path_owned_by_system(ancestor)?,
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
                meta.is_dir() && !meta.file_type().is_symlink(),
                "private storage directory is not a regular directory"
            );
            ensure!(
                platform_fs::path_owned_by_current_user(dir)?,
                "private storage directory has another owner"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = dir.parent().context("private directory has no parent")?;
            if !parent.exists() {
                private_dir(parent)?;
            }
            match platform_fs::create_dir_owner_only(&fs::DirBuilder::new(), dir) {
                Ok(()) => {}
                // Another owner thread/process may create the shared store concurrently.
                // Re-enter validation; never treat an arbitrary pre-existing path as trusted.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return private_dir(dir);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(e) => return Err(e.into()),
    }
    platform_fs::set_owner_only(dir)?;
    Ok(())
}

fn check_file(file: &File) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && platform_fs::link_count(file)? == 1
            && platform_fs::owned_by_current_user(file)?,
        "private storage target must be an owner-owned regular file without hard links"
    );
    if !platform_fs::is_owner_only(file)? {
        platform_fs::set_owner_only_file(file)?;
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File> {
    private_dir(path.parent().context("storage path has no parent")?)?;
    let lock_path = path.with_extension(format!(
        "{}.lock",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    let lock = platform_fs::open_owner_only(
        &OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false),
        &lock_path,
    )?;
    check_file(&lock)?;
    Ok(lock)
}

pub(crate) fn locked(path: &Path) -> Result<File> {
    let lock = open_lock(path)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

/// A long-running owner must refuse an occupied record rather than overwrite another run.
pub(crate) fn try_locked(path: &Path) -> Result<File> {
    let lock = open_lock(path)?;
    lock.try_lock_exclusive()
        .context("private record is owned by another active operation")?;
    Ok(lock)
}

/// Wait without relinquishing an acquired owner or reading unprotected auth state.
pub(crate) fn locked_cancellable(path: &Path, cancel: &AtomicBool) -> Result<File> {
    locked_for(path, Some(cancel), Duration::from_secs(30))
}

/// Like `locked_cancellable`, with a caller-chosen deadline for UI-thread callers that must not stall.
pub(crate) fn locked_for(
    path: &Path,
    cancel: Option<&AtomicBool>,
    timeout: Duration,
) -> Result<File> {
    let lock = open_lock(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(crate::llm::Interrupted.into());
        }
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(
                    Instant::now() < deadline,
                    "authentication storage lock timed out"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    validate_private_path(path.parent().context("storage path has no parent")?)?;
    let file = match platform_fs::open_nofollow_nonblocking(&OpenOptions::new().read(true), path) {
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
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_file()
                && !meta.file_type().is_symlink()
                && platform_fs::link_count(&platform_fs::open_nofollow_nonblocking(
                    &OpenOptions::new().read(true),
                    path
                )?)? == 1
                && platform_fs::path_owned_by_current_user(path)?,
            "unsafe private storage target"
        );
    }
    let dir = path.parent().context("storage path has no parent")?;
    validate_private_path(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    platform_fs::set_owner_only(temp.path())?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        temp.persist(path).map_err(|e| e.error)?;
        File::open(dir)?.sync_all()?;
    }
    #[cfg(windows)]
    {
        let temp_name = temp
            .path()
            .file_name()
            .context("temporary file has no name")?
            .to_os_string();
        let target_name = path
            .file_name()
            .context("storage file has no name")?
            .to_os_string();
        let parent = platform_fs::open_dir_nofollow_write(dir)?;
        let staged_file = temp.as_file().try_clone()?;
        let staged_path = temp.into_temp_path();
        platform_fs::replace_beneath_file(
            &parent,
            Path::new(&temp_name),
            &staged_file,
            Path::new(&target_name),
        )?;
        let _ = staged_path.keep().map_err(|error| error.error)?;
        // Windows has no documented unprivileged directory flush/write-through rename; the staged
        // file is flushed and publication is atomic for readers, but namespace crash durability is
        // not guaranteed after a power loss.
    }
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
        // `edit_mode` moved to `[edit] mode`: write the canonical key and drop the legacy one so
        // validation does not reject the write and no duplicate remains.
        if path == "edit_mode" {
            patch(&mut doc, &["edit_mode"], None)?;
            patch(&mut doc, &["edit", "mode"], value)?;
        } else {
            patch(&mut doc, &parts, value)?;
        }
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
    save_config_with_project_resets(layout, scope, changes, &[])
}

/// Explicit Global model choices clear only this project's override for the chosen role.
pub fn save_model_config(
    layout: &Layout,
    scope: Scope,
    changes: &[Change],
    role: &str,
) -> Result<crate::config::types::Config> {
    ensure!(
        crate::llm::model_config::ROLES
            .iter()
            .any(|(name, _)| *name == role),
        "unknown model role {role:?}"
    );
    let paths = if role == "main" {
        vec![
            "model".to_owned(),
            "provider".to_owned(),
            "endpoint".to_owned(),
            "model_profile".to_owned(),
            "context_window".to_owned(),
        ]
    } else {
        vec![format!("model_roles.{role}")]
    };
    let resets: Vec<_> = paths
        .into_iter()
        .map(|path| Change::Remove { path })
        .collect();
    save_config_with_project_resets(layout, scope, changes, &resets)
}

fn save_config_with_project_resets(
    layout: &Layout,
    scope: Scope,
    changes: &[Change],
    project_resets: &[Change],
) -> Result<crate::config::types::Config> {
    layout.ensure_state_outside_project()?;
    let files = layout.config_files();
    let selected = if scope == Scope::Global { 0 } else { 1 };
    // Always lock global before project when both are involved.
    let _lock = locked(&files[selected])?;
    let reset_project = scope == Scope::Global && !project_resets.is_empty();
    let _project_lock = reset_project.then(|| locked(&files[1])).transpose()?;
    let mut layers: [toml::Table; 2] = Default::default();
    for (index, path) in files.iter().enumerate() {
        let edits = if index == selected { changes } else { &[] };
        let mut layer = changed_document(path, edits)?;
        crate::config::migrate_memory_tool_table(&mut layer);
        crate::config::migrate_edit_configuration(&mut layer);
        crate::config::migrate_model_role_table(&mut layer);
        crate::config::migrate_removed_settings(&mut layer);
        if reset_project && index == 1 {
            // Clear canonical roles after migrating legacy assignments too.
            for reset in project_resets {
                if let Change::Remove { path } = reset {
                    patch(&mut layer, &path.split('.').collect::<Vec<_>>(), None)?;
                }
            }
        }
        layers[index] = layer;
    }
    let config = crate::config::types::Config::from_layers(
        layout,
        &layers[0],
        &layers[1],
        &toml::Table::new(),
    )?;
    let selected_bytes = toml::to_string_pretty(&layers[selected])?;
    if reset_project {
        let project_bytes = toml::to_string_pretty(&layers[1])?;
        let original = read_private(&files[0])?;
        write_private(&files[0], selected_bytes.as_bytes())?;
        if let Err(error) = write_private(&files[1], project_bytes.as_bytes()) {
            // Roll back the first publication if the second fails. Never hide partial saves.
            let rollback = match original {
                Some(bytes) => write_private(&files[0], &bytes),
                None => fs::remove_file(&files[0]).map_err(anyhow::Error::from),
            };
            if let Err(rollback_error) = rollback {
                anyhow::bail!(
                    "project model override could not be cleared: {error:#}; global settings changed and rollback failed: {rollback_error:#}"
                );
            }
            return Err(
                error.context("model choice not saved: project override could not be cleared")
            );
        }
    } else {
        write_private(&files[selected], selected_bytes.as_bytes())?;
    }
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

/// Resolve disk layers plus CLI overrides without migrating or rewriting disk configuration.
pub fn load_config(layout: &Layout, overlay: &toml::Table) -> Result<crate::config::types::Config> {
    let layers = [
        load_layer(layout, Scope::Global)?,
        load_layer(layout, Scope::Project)?,
    ];
    resolve_layers(layout, &layers, overlay)
}

fn resolve_layers(
    layout: &Layout,
    layers: &[toml::Table; 2],
    overlay: &toml::Table,
) -> Result<crate::config::types::Config> {
    crate::config::types::Config::from_layers(layout, &layers[0], &layers[1], overlay)
}

/// Last validated external layers. Failed reads/parses retain that layer; a failed complete
/// validation retains the last good effective configuration, never writes defaults to disk.
pub struct SettingsReload {
    layers: Option<[toml::Table; 2]>,
    settings: crate::config::settings::Settings,
}

impl SettingsReload {
    pub fn new(
        layout: &Layout,
        overlay: &toml::Table,
        config: &crate::config::types::Config,
    ) -> Self {
        let layers = (|| -> Result<_> {
            let layers = [
                load_layer(layout, Scope::Global)?,
                load_layer(layout, Scope::Project)?,
            ];
            resolve_layers(layout, &layers, overlay)?;
            Ok(layers)
        })()
        .ok();
        Self {
            layers,
            settings: crate::config::settings::Settings::new(config.clone())
                .expect("SettingsReload starts from validated configuration"),
        }
    }
    pub fn config(&self) -> &crate::config::types::Config {
        self.settings.snapshot().config()
    }

    pub fn snapshot(&self) -> &crate::config::settings::Snapshot {
        self.settings.snapshot()
    }
    pub fn subscribe(&mut self) -> crate::config::settings::Subscription {
        self.settings.subscribe()
    }

    pub fn reload(
        &mut self,
        layout: &Layout,
        overlay: &toml::Table,
    ) -> (crate::config::types::Config, Vec<String>) {
        let files = layout.config_files();
        let mut diagnostics = Vec::new();
        let mut candidate = self.layers.clone().unwrap_or_default();
        let mut readable = [false; 2];
        for (index, scope) in [Scope::Global, Scope::Project].into_iter().enumerate() {
            match load_layer(layout, scope) {
                Ok(layer) => {
                    candidate[index] = layer;
                    readable[index] = true;
                }
                Err(error) => diagnostics.push(format!(
                    "{}: {error:#}; keeping last good settings",
                    files[index].display()
                )),
            }
        }
        // If startup raced an invalid edit, there is no trustworthy layer baseline yet.
        if self.layers.is_none() && !readable.iter().all(|value| *value) {
            return (self.settings.snapshot().config().clone(), diagnostics);
        }
        match resolve_layers(layout, &candidate, overlay) {
            Ok(config) => {
                self.layers = Some(candidate);
                if let Err(error) = self.settings.publish(config) {
                    diagnostics.push(format!(
                        "Settings publication failed: {error:#}; keeping last good settings"
                    ));
                }
            }
            Err(error) => {
                diagnostics.push(format!(
                    "Merged settings from {} and {}: {error:#}; invalid changes were not applied",
                    files[0].display(),
                    files[1].display()
                ));
                // Every accepted independent replacement validates all layers plus CLI.
                if let Some(mut good) = self.layers.clone() {
                    for index in 0..2 {
                        if !readable[index] || candidate[index] == good[index] {
                            continue;
                        }
                        let mut trial = good.clone();
                        trial[index] = candidate[index].clone();
                        if let Ok(config) = resolve_layers(layout, &trial, overlay) {
                            good = trial;
                            if let Err(error) = self.settings.publish(config) {
                                diagnostics.push(format!("Settings publication failed: {error:#}; keeping last good settings"));
                            }
                        }
                    }
                    self.layers = Some(good);
                }
            }
        }
        (self.settings.snapshot().config().clone(), diagnostics)
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
    // 0 means "unlimited" in config, but the file is still capped: an unbounded file would
    // exceed MAX_FILE_BYTES, after which reads fail and history silently stops persisting.
    let cap = if limit == 0 {
        MAX_HISTORY
    } else {
        limit.min(MAX_HISTORY)
    };
    let excess = history.len().saturating_sub(cap);
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
    fn model_assignment_rejects_invalid_project_without_changing_global() {
        let (_home, _project, layout) = fixture();
        save_config(
            &layout,
            Scope::Global,
            &[Change::Set {
                path: "model".into(),
                value: "original".into(),
            }],
        )
        .unwrap();
        let original = fs::read(&layout.config_files()[0]).unwrap();
        save_config(&layout, Scope::Project, &[]).unwrap();
        write_private(&layout.config_files()[1], b"not valid TOML [").unwrap();
        assert!(
            save_model_config(
                &layout,
                Scope::Global,
                &[Change::Set {
                    path: "model".into(),
                    value: "replacement".into(),
                }],
                "main"
            )
            .is_err()
        );
        assert_eq!(fs::read(&layout.config_files()[0]).unwrap(), original);
    }
    #[test]
    fn model_favorites_persist_and_project_empty_list_overrides_global() {
        use crate::llm::model_config::ModelSelection;
        let (_home, _project, layout) = fixture();
        let favorite = ModelSelection {
            provider: "openai".into(),
            model: "favorite-model".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            ..ModelSelection::default()
        };
        let changes = [Change::Set {
            path: "model_favorites".into(),
            value: toml::Value::try_from(vec![favorite.clone()]).unwrap(),
        }];
        let saved = save_config(&layout, Scope::Global, &changes).unwrap();
        assert_eq!(saved.model_favorites, vec![favorite.clone()]);
        let reloaded = crate::config::types::Config::from_table(
            &layout,
            load_layer(&layout, Scope::Global).unwrap(),
        )
        .unwrap();
        assert_eq!(reloaded.model_favorites, vec![favorite.clone()]);
        let saved = save_config(
            &layout,
            Scope::Project,
            &[Change::Set {
                path: "model_favorites".into(),
                value: toml::Value::Array(vec![]),
            }],
        )
        .unwrap();
        assert!(saved.model_favorites.is_empty());
        let global: Vec<ModelSelection> =
            load_layer(&layout, Scope::Global).unwrap()["model_favorites"]
                .clone()
                .try_into()
                .unwrap();
        assert_eq!(global, vec![favorite]);
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
            platform_fs::mode(&fs::metadata(history_path(&l).unwrap()).unwrap()) & 0o777,
            0o600
        );
        #[cfg(windows)]
        assert!(
            platform_fs::is_owner_only(&File::open(history_path(&l).unwrap()).unwrap()).unwrap()
        );
        save_history(&l, &["external".into(), "new".into(), "last".into()], 2).unwrap();
        assert_eq!(load_history(&l, 2).unwrap(), ["new", "last"]);
    }
    #[test]
    fn refuses_symlink_and_hardlink_targets() {
        let (_h, _p, l) = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), &l.config_files()[0]).unwrap();
            assert!(save(&l, Scope::Global, &[]).is_err());
            fs::remove_file(&l.config_files()[0]).unwrap();
        }
        fs::hard_link(outside.path(), &l.config_files()[0]).unwrap();
        assert!(save(&l, Scope::Global, &[]).is_err());
    }
}
