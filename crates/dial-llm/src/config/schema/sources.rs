//! Where a setting's values come from, so the settings panel can offer a choice (a file browser,
//! discovered models, configured names or known values) instead of free typing. A source only
//! changes how a value is entered: the setting's [`Kind`](super::Kind), validation and the typed
//! escape hatch stay the same, and every source remains open to values it does not list.
use super::SettingDef;
use crate::config::tools::integrations::INTEGRATION_TOOLS;

/// What a path setting names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    File,
    Directory,
    /// A program: any file may be chosen, executables are marked.
    Executable,
}

/// Which models a model setting accepts, and how its value names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelUse {
    /// A local MLX model id (Hugging Face repo) or path; missing weights can be downloaded.
    LocalMlx,
    /// A chat model's bare model name.
    Chat,
    /// A chat model as `provider/model`.
    QualifiedChat,
    /// An image-generation model identifier.
    Image,
}

/// Names defined elsewhere in the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    Providers,
    ModelProfiles,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Path(PathKind),
    Model(ModelUse),
    Names(Names),
    /// Known values of a list setting; `ordered` lists keep the user's order (a preference).
    Values {
        values: &'static [&'static str],
        ordered: bool,
    },
}

/// Status-line segment names the renderer understands (see `ui::statusline`).
pub const STATUS_SEGMENTS: &[&str] = &[
    "spinner",
    "model",
    "mode",
    "thinking",
    "inference",
    "role",
    "path",
    "git",
    "context",
    "tokens",
    "rate",
    "approval",
    "sandbox",
    "learning",
    "usage",
    "session",
    "time",
    "workers",
    "cost",
];
/// Serialized names of `CompactionMethod`, in the default preference order.
pub const COMPACTION_METHODS: &[&str] = &["remote", "soft", "shake", "snapcompact", "handoff"];
/// Foreign tools whose user-level rules Dial can load (see the agent's context loader).
pub const FOREIGN_SOURCES: &[&str] = &["codex", "gemini", "opencode"];
/// Hindsight recall fact types.
pub const RECALL_TYPES: &[&str] = &["world", "experience", "observation"];

impl SettingDef {
    /// Where this setting's values come from, when the panel can offer them.
    pub fn source(&self) -> Option<Source> {
        use {ModelUse::*, PathKind::*};
        Some(match self.path {
            "python" | "eval.node" | "ida.python" => Source::Path(Executable),
            path if path.starts_with("lsp.") && path.ends_with(".program") => {
                Source::Path(Executable)
            }
            "worker" => Source::Path(File),
            "ida.idadir" | "sandbox.extra_write" => Source::Path(Directory),
            "local_runtime.draft_model" | "codebase.architecture_model" => Source::Model(LocalMlx),
            "model" => Source::Model(Chat),
            "memory.sharpshooter.model" | "training.learn_from" => Source::Model(QualifiedChat),
            "image.model" => Source::Model(Image),
            "provider" => Source::Names(Names::Providers),
            "model_profile" => Source::Names(Names::ModelProfiles),
            "ui.status_left" | "ui.status_right" => Source::Values {
                values: STATUS_SEGMENTS,
                ordered: true,
            },
            "compaction.method_order" => Source::Values {
                values: COMPACTION_METHODS,
                ordered: true,
            },
            "integrations" => Source::Values {
                values: INTEGRATION_TOOLS,
                ordered: false,
            },
            "foreign_sources" => Source::Values {
                values: FOREIGN_SOURCES,
                ordered: false,
            },
            "memory.hindsight.recall_types" => Source::Values {
                values: RECALL_TYPES,
                ordered: false,
            },
            _ => return None,
        })
    }
}
