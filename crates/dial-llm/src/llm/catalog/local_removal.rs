//! Deleting a downloaded local model from disk.
//!
//! Only a model local discovery lists (Hugging Face cache, LM Studio, MLX-Serve) can be deleted, and
//! only its own folder inside the store that holds it: the whole `models--owner--name` repository
//! of the Hugging Face cache (snapshots share its blobs), or `<publisher>/<model>` of an LM Studio
//! or MLX-Serve store. A model the configuration still selects, as the main model or for a role, is
//! refused. Symbolic links are removed as links, never followed.

use super::metadata::ModelEntry;
use crate::config::types::Config;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// One downloaded model's folder, what deleting it frees, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LocalRemoval {
    /// The discovery label, such as `owner/name`.
    pub label: String,
    /// The model path discovery lists (a snapshot, a folder or a GGUF file).
    pub model: String,
    /// The folder deleting removes.
    pub folder: PathBuf,
    /// Bytes of the regular files under `folder`; links are not followed.
    pub bytes: u64,
    /// The discovery source text, such as `Hugging Face cache (MLX)`.
    pub source: String,
}

/// The configuration still selects the model a delete named; nothing was removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInUse {
    pub label: String,
    /// The selected model path that lies in the folder.
    pub selected: String,
}

impl std::fmt::Display for ModelInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is selected in your configuration ({}); choose another model for the main model and every role in /models first",
            self.label, self.selected
        )
    }
}

impl std::error::Error for ModelInUse {}

/// A store local discovery reads and how deep its model folders sit: `models--owner--name` is one
/// level below the Hugging Face cache, `publisher/model` two below LM Studio and MLX-Serve.
#[derive(Clone, Debug)]
pub(super) struct Store {
    pub(super) root: PathBuf,
    pub(super) depth: usize,
}

/// The stores on this machine local discovery reads.
fn stores() -> Vec<Store> {
    let mut out = Vec::new();
    if let Some(root) = super::local_models::hf_cache() {
        out.push(Store { root, depth: 1 });
    }
    if let Some(root) = super::local_models::lmstudio_folder() {
        out.push(Store { root, depth: 2 });
    }
    if let Some(home) = crate::config::paths::home_dir() {
        out.push(Store {
            root: home.join(".mlx-serve/models"),
            depth: 2,
        });
    }
    out
}

/// What deleting the discovered local model `model` (its path or its label) would remove.
/// Reads the file system only.
///
/// # Errors
/// `model` is not a model local discovery lists, or its folder is not inside a known store.
pub fn plan_local_model_removal(model: &str) -> Result<LocalRemoval> {
    plan_in(model, &super::local_model_entries(None), &stores())
}

/// Deletes the discovered local model `model` (its path or its label) unless `in_use` names it,
/// and returns what was removed.
///
/// # Errors
/// What [`plan_local_model_removal`] refuses, a model `in_use` still selects, or a failed delete
/// (with the folder named; a partial delete leaves the rest in place).
pub fn remove_local_model(model: &str, in_use: &[String]) -> Result<LocalRemoval> {
    let removal = plan_local_model_removal(model)?;
    remove_planned(&removal, in_use)?;
    Ok(removal)
}

/// The local model paths `config` selects: the main model and every assigned role.
pub fn models_in_use(config: &Config) -> Vec<String> {
    std::iter::once(config.selection().model)
        .chain(config.model_roles.values().map(|role| role.model.clone()))
        .filter(|model| !model.is_empty())
        .collect()
}

pub(super) fn plan_in(
    model: &str,
    entries: &[ModelEntry],
    stores: &[Store],
) -> Result<LocalRemoval> {
    let wanted = model.trim();
    ensure!(!wanted.is_empty(), "name the local model to delete");
    let canonical = fs::canonicalize(wanted).ok();
    let entry = entries
        .iter()
        .find(|entry| {
            entry.label == wanted
                || entry.selection.model == wanted
                || canonical.is_some()
                    && fs::canonicalize(&entry.selection.model).ok() == canonical
        })
        .with_context(|| {
            format!(
                "{wanted:?} is not a downloaded local model Dial found; only models in the Hugging Face cache, LM Studio or MLX-Serve folders can be deleted (models served by Ollama or another server are managed there)"
            )
        })?;
    let folder = model_folder(Path::new(&entry.selection.model), stores)?;
    Ok(LocalRemoval {
        label: entry.label.clone(),
        model: entry.selection.model.clone(),
        bytes: folder_bytes(&folder),
        folder,
        source: entry.source.clone(),
    })
}

/// The folder of the store holding `model`, `depth` levels below the store root.
fn model_folder(model: &Path, stores: &[Store]) -> Result<PathBuf> {
    // The model path itself may be a link (an HF snapshot file); its parent folders are real.
    let resolved = match fs::symlink_metadata(model) {
        Ok(meta) if meta.file_type().is_symlink() => model
            .parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .zip(model.file_name())
            .map(|(parent, name)| parent.join(name)),
        Ok(_) => fs::canonicalize(model).ok(),
        Err(_) => None,
    }
    .with_context(|| format!("{} no longer exists", model.display()))?;
    for store in stores {
        let Ok(root) = fs::canonicalize(&store.root) else {
            continue;
        };
        let Ok(relative) = resolved.strip_prefix(&root) else {
            continue;
        };
        let parts: Vec<_> = relative.components().take(store.depth).collect();
        if parts.len() < store.depth {
            continue;
        }
        let folder = parts
            .iter()
            .fold(root.clone(), |path, part| path.join(part.as_os_str()));
        let meta = fs::symlink_metadata(&folder)
            .with_context(|| format!("reading {}", folder.display()))?;
        ensure!(
            meta.is_dir(),
            "{} is not a model folder (a link or a file); delete it yourself",
            folder.display()
        );
        return Ok(folder);
    }
    bail!(
        "{} is outside the Hugging Face cache, LM Studio and MLX-Serve model folders; Dial only deletes models in those stores",
        model.display()
    )
}

/// Bytes of the regular files under `folder`, not following links.
fn folder_bytes(folder: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![folder.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Fails with [`ModelInUse`] when `in_use` selects a model inside `removal`'s folder, so a panel can
/// refuse before asking to confirm a delete that would be refused anyway.
pub fn ensure_not_in_use(removal: &LocalRemoval, in_use: &[String]) -> Result<(), ModelInUse> {
    let folder = &removal.folder;
    match in_use.iter().find(|model| {
        let path = Path::new(model.as_str());
        path.starts_with(folder)
            || fs::canonicalize(path).is_ok_and(|resolved| resolved.starts_with(folder))
    }) {
        Some(selected) => Err(ModelInUse {
            label: removal.label.clone(),
            selected: selected.clone(),
        }),
        None => Ok(()),
    }
}

pub(super) fn remove_planned(removal: &LocalRemoval, in_use: &[String]) -> Result<()> {
    ensure_not_in_use(removal, in_use)?;
    let folder = &removal.folder;
    // Re-checked right before deleting: the folder must still be a real directory, not a link.
    let meta = fs::symlink_metadata(folder)
        .with_context(|| format!("{} no longer exists", folder.display()))?;
    ensure!(
        meta.is_dir(),
        "{} is no longer a model folder",
        folder.display()
    );
    fs::remove_dir_all(folder).with_context(|| {
        format!(
            "deleting {}; files that were already removed stay removed",
            folder.display()
        )
    })
}

#[cfg(test)]
mod tests;
