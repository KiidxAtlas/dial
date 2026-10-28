//! Curated local models for the `grounding` and `image_embedding` roles, run by Dial's MLX-VLM
//! server on Apple Silicon.
//!
//! `/setup grounding` and `/setup image-embeddings` list the catalog; `/setup grounding:<repo> yes`
//! (or the bare id, which means the default) installs the private MLX runtime when it is missing,
//! downloads the checkpoint from Hugging Face, and only then assigns `mlx / <repo>` to the role
//! through the canonical selection writer, so a previous endpoint's fields do not survive.
//! Any other MLX repository id is accepted when typed; previews never download anything.

use crate::config::paths::Layout;
use crate::config::selection;
use crate::config::settings_store::{self, Scope};
use crate::config::types::Config;
use crate::llm::download::{self, Choice};
use crate::llm::model_config::ModelSelection;
use anyhow::{Result, ensure};
use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Which vision role a setup id configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Grounding,
    ImageEmbedding,
}

/// One curated checkpoint. Sizes are the Hugging Face snapshot sizes, rounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub repo: &'static str,
    pub size_mb: u32,
    pub description: &'static str,
}

const GROUNDING: &[Model] = &[
    Model {
        repo: "mlx-community/UI-Venus-1.5-2B-6bit",
        size_mb: 2500,
        description: "Small, fast UI grounding model (Qwen3-VL); a good default.",
    },
    Model {
        repo: "mlx-community/UI-TARS-1.5-7B-4bit",
        size_mb: 4900,
        description: "Larger UI-TARS grounding model (Qwen2.5-VL); more robust on dense screens.",
    },
];

const IMAGE_EMBEDDING: &[Model] = &[Model {
    repo: "mlx-community/Qwen3-VL-Embedding-2B-8bit",
    size_mb: 2700,
    description: "Text and images in one 2048-dimension space (Qwen3-VL-Embedding).",
}];

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Grounding, Kind::ImageEmbedding];

    /// The model role this kind assigns.
    pub fn role(self) -> &'static str {
        match self {
            Kind::Grounding => "grounding",
            Kind::ImageEmbedding => "image_embedding",
        }
    }

    /// The `/setup` id: alone it names the default model, `<id>:<repo>` names another.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Grounding => "grounding",
            Kind::ImageEmbedding => "image-embeddings",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Kind::Grounding => "UI grounding model",
            Kind::ImageEmbedding => "Image embedding model",
        }
    }

    /// What the role enables, for previews and the setup picker.
    pub fn purpose(self) -> &'static str {
        match self {
            Kind::Grounding => {
                "finds described elements in screenshots for computer `locate` and browser action=locate"
            }
            Kind::ImageEmbedding => {
                "searches images by text, finds similar images and scores images against a prompt (image_search)"
            }
        }
    }

    pub fn catalog(self) -> &'static [Model] {
        match self {
            Kind::Grounding => GROUNDING,
            Kind::ImageEmbedding => IMAGE_EMBEDDING,
        }
    }

    pub fn default_model(self) -> &'static str {
        self.catalog()[0].repo
    }
}

/// Setup id for one model: `<kind id>:<repo>`.
pub fn setup_id(kind: Kind, repo: &str) -> String {
    format!("{}:{repo}", kind.id())
}

/// The kind and model a setup id names; `None` when the id is not a vision-model id or the
/// repository id is malformed.
pub fn parse_id(id: &str) -> Option<(Kind, &str)> {
    Kind::ALL.into_iter().find_map(|kind| {
        if id == kind.id() {
            return Some((kind, kind.default_model()));
        }
        let repo = id.strip_prefix(kind.id())?.strip_prefix(':')?;
        validate_repo(repo).is_ok().then_some((kind, repo))
    })
}

/// Hugging Face repository ids: `owner/name` with letters, digits and `-_.`, at most 96 bytes.
pub fn validate_repo(repo: &str) -> Result<()> {
    ensure!(repo.len() <= 96, "model repository id is too long");
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        anyhow::bail!("model must be a Hugging Face repository id like owner/name, got {repo:?}");
    };
    let valid = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['-', '.'])
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    ensure!(
        valid(owner) && valid(name),
        "model repository id {repo:?} may contain only letters, digits and - _ ."
    );
    Ok(())
}

/// The curated catalog for `/setup <kind id>`.
pub fn catalog_text(kind: Kind) -> String {
    let mut lines = vec![format!(
        "{} models (local MLX, Apple Silicon); the role {}. Set one up with /setup {}:<repo> yes:",
        kind.title(),
        kind.purpose(),
        kind.id()
    )];
    let width = kind
        .catalog()
        .iter()
        .map(|model| model.repo.len())
        .max()
        .unwrap_or(0);
    for (index, model) in kind.catalog().iter().enumerate() {
        lines.push(format!(
            "  {:<width$}  {:>5} MB  {}{}",
            model.repo,
            model.size_mb,
            model.description,
            if index == 0 { " (default)" } else { "" }
        ));
    }
    lines.push(format!(
        "Any other MLX vision checkpoint also works, or assign an OpenAI-compatible endpoint to the {} role in /roles (Alt+M).",
        kind.role()
    ));
    lines.join("\n")
}

/// What `/setup <id> yes` will do. Reads only local state.
pub fn preview_text(kind: Kind, repo: &str, user_dir: &Path) -> String {
    let mut lines = vec![
        match kind.catalog().iter().find(|model| model.repo == repo) {
            Some(model) => format!(
                "{} {repo} (about {} MB): {}",
                kind.title(),
                model.size_mb,
                kind.purpose()
            ),
            None => format!("{} {repo}: {}", kind.title(), kind.purpose()),
        },
    ];
    if !(cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")) {
        lines.push("Local MLX models need macOS on Apple Silicon. On this machine, assign an OpenAI-compatible endpoint to the role in /roles (Alt+M) instead.".to_owned());
        return lines.join("\n");
    }
    if user_dir.join("python/.venv/bin/python").is_file() {
        lines.push("The private MLX runtime is installed.".to_owned());
    } else {
        lines.push(format!(
            "Installs the private MLX runtime first (uv sync into {}; needs uv and network access).",
            user_dir.join("python").display()
        ));
    }
    let cached = download::cached_bytes(repo);
    if cached > 0 {
        lines.push(format!(
            "{repo} has {} MB in the Hugging Face cache; missing files are downloaded.",
            cached / 1_000_000
        ));
    } else {
        lines.push(format!(
            "Downloads {repo} from Hugging Face into its cache."
        ));
    }
    lines.push(format!(
        "Then sets model_roles.{} to mlx / {repo} for all projects. The model loads on first use and stays resident.",
        kind.role()
    ));
    lines.push(format!("Confirm with: /setup {} yes", setup_id(kind, repo)));
    lines.join("\n")
}

/// Installs the runtime when missing, downloads `repo`, then assigns it to the role. Settings are
/// written only after the download succeeds. Returns a summary for the user.
pub fn setup(
    layout: &Layout,
    kind: Kind,
    repo: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    validate_repo(repo)?;
    let progress = RefCell::new(progress);
    let report = |line: &str| (progress.borrow_mut())(line);
    if !layout.user_dir.join("python/.venv/bin/python").is_file() {
        report("installing the private MLX runtime (uv sync)");
        crate::setup::ensure_runtime_with(layout, cancel, &report)?;
    }
    let config = Config::load(layout, None)?;
    download::ensure_local_model_with(&config.python, repo, &Choice::Auto, cancel, &report)?;
    report("downloaded");
    let role = kind.role();
    let changes = selection::changes(&config, &ModelSelection::new("mlx", repo), role)?;
    let saved = settings_store::save_model_config(layout, Scope::Global, &changes, role)?;
    Ok(summary(kind, repo, &saved))
}

/// Prefix of a successful setup summary; the shell reloads the configuration when it sees it so
/// the role (and the tools that depend on it) apply to the running session.
pub const SUMMARY_PREFIX: &str = "Vision role ready · ";

/// What setup reports, read from the saved config so a project override is named, not hidden.
fn summary(kind: Kind, repo: &str, saved: &Config) -> String {
    let role = kind.role();
    let mut lines = vec![format!(
        "{SUMMARY_PREFIX}{}: mlx / {repo} (model_roles.{role}, all projects). The role {}.",
        kind.title(),
        kind.purpose()
    )];
    if let Some(selection) = saved.role_selection(role)
        && !(selection.provider == "mlx" && selection.model == repo)
    {
        lines.push(format!(
            "This project assigns its own {role} role ({}), which overrides the new global one. Clear it in /roles (Alt+M) with \"This project\" scope to use {repo}.",
            selection.display_name()
        ));
    }
    if kind == Kind::Grounding
        && !crate::tools::surface::catalog::integration_enabled("computer", &saved.integrations)
    {
        lines.push(
            "browser action=locate can use it now; enable the computer integration in /tools for desktop locate()."
                .to_owned(),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests;
