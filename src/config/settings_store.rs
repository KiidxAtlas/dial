//! Locked, owner-private settings and submitted-prompt history. TOML values outside
//! the requested paths survive updates; invalid existing files are never replaced.
use super::paths::Layout;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::config::session_models::SessionModels;
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

// Compatibility exports; publication mechanics and private path policy have a neutral owner.
pub(crate) use crate::platform::atomic::{validate_private_path, write_private};

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
        OpenOptions::new()
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
    let file = match platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path) {
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
    layout.ensure_project_outside_storage()?;
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
    let resets = model_project_resets(role)?;
    save_config_with_project_resets(layout, scope, changes, &resets)
}

/// Validate the same future layers as `save_model_config`, without publishing or migrating files.
/// Save re-reads under its locks; a preview is not a reservation against concurrent edits.
pub fn preview_model_config(
    layout: &Layout,
    scope: Scope,
    changes: &[Change],
    role: &str,
) -> Result<crate::config::types::Config> {
    let resets = model_project_resets(role)?;
    let (_, config) = prepare_config(layout, scope, changes, &resets)?;
    Ok(config)
}

fn model_project_resets(role: &str) -> Result<Vec<Change>> {
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
    Ok(paths
        .into_iter()
        .map(|path| Change::Remove { path })
        .collect())
}

fn save_config_with_project_resets(
    layout: &Layout,
    scope: Scope,
    changes: &[Change],
    project_resets: &[Change],
) -> Result<crate::config::types::Config> {
    layout.ensure_project_outside_storage()?;
    let files = layout.config_files();
    let selected = if scope == Scope::Global { 0 } else { 1 };
    // Always lock global before project when both are involved.
    let _lock = locked(&files[selected])?;
    let reset_project = scope == Scope::Global && !project_resets.is_empty();
    let _project_lock = reset_project.then(|| locked(&files[1])).transpose()?;
    let (layers, config) = prepare_config(layout, scope, changes, project_resets)?;
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

fn prepare_config(
    layout: &Layout,
    scope: Scope,
    changes: &[Change],
    project_resets: &[Change],
) -> Result<([toml::Table; 2], crate::config::types::Config)> {
    layout.ensure_project_outside_storage()?;
    let selected = if scope == Scope::Global { 0 } else { 1 };
    let mut layers: [toml::Table; 2] = Default::default();
    for (index, path) in layout.config_files().iter().enumerate() {
        let edits = if index == selected { changes } else { &[] };
        let mut layer = changed_document(path, edits)?;
        crate::config::migrate_memory_tool_table(&mut layer);
        crate::config::migrate_edit_configuration(&mut layer);
        crate::config::migrate_model_role_table(&mut layer);
        crate::config::migrate_removed_settings(&mut layer);
        if scope == Scope::Global && index == 1 {
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
    Ok((layers, config))
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
    load_session_config(layout, overlay, None)
}

/// [`load_config`] with a running session's own model choices kept over the files' model keys.
pub fn load_session_config(
    layout: &Layout,
    overlay: &toml::Table,
    models: Option<&SessionModels>,
) -> Result<crate::config::types::Config> {
    let layers = [
        load_layer(layout, Scope::Global)?,
        load_layer(layout, Scope::Project)?,
    ];
    resolve_layers(layout, &layers, overlay, models)
}

/// The raw merged settings table (both files plus CLI overrides) that loading starts from.
pub fn load_merged(layout: &Layout, overlay: &toml::Table) -> Result<toml::Table> {
    Ok(crate::config::merged_layers(
        &load_layer(layout, Scope::Global)?,
        &load_layer(layout, Scope::Project)?,
        overlay,
    ))
}

fn resolve_layers(
    layout: &Layout,
    layers: &[toml::Table; 2],
    overlay: &toml::Table,
    models: Option<&SessionModels>,
) -> Result<crate::config::types::Config> {
    crate::config::types::Config::from_session_layers(
        layout, &layers[0], &layers[1], overlay, models,
    )
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
            resolve_layers(layout, &layers, overlay, None)?;
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

    /// Re-read both files. `models` keeps a running session's own model choices over the files'.
    pub fn reload(
        &mut self,
        layout: &Layout,
        overlay: &toml::Table,
        models: Option<&SessionModels>,
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
        match resolve_layers(layout, &candidate, overlay, models) {
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
                        if let Ok(config) = resolve_layers(layout, &trial, overlay, models) {
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
    layout.ensure_project_outside_storage()?;
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
mod tests;
