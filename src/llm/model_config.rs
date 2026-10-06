//! Persisted UI preferences and provider-independent model selections.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub allow_insecure: bool,
    pub context_window: Option<u32>,
}
impl Default for ModelSelection {
    fn default() -> Self {
        Self {
            provider: "mlx".into(),
            model: String::new(),
            base_url: None,
            api_key_env: None,
            allow_insecure: false,
            context_window: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSpec {
    pub label: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub enabled: bool,
    pub allow_insecure: bool,
}
impl Default for ProviderSpec {
    fn default() -> Self {
        Self {
            label: String::new(),
            base_url: String::new(),
            api_key_env: None,
            enabled: true,
            allow_insecure: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolPreset {
    /// Nerd Font glyphs when the terminal advertises a Nerd Font, unicode otherwise.
    #[default]
    Auto,
    Unicode,
    Nerd,
    Ascii,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatusPreset {
    #[default]
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
    Custom,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSeparator {
    Powerline,
    #[default]
    PowerlineThin,
    Slash,
    Pipe,
    Block,
    None,
    Ascii,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComposerShape {
    /// Rounded box; the status line is embedded in its top border and key hints in its bottom border.
    #[default]
    Box,
    /// Rules above and below with no side borders (cleaner when copying text).
    Line,
}

/// Everything about how the terminal UI looks and behaves that a user may tailor. All of it is
/// editable in `/settings`, stored in the same layered config as everything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Theme name: a built-in (`dark`, `light`, `mono`, …), a file in `<dial home>/themes`, or `auto`.
    pub theme: String,
    /// Themes used by `auto`, chosen from the terminal's reported background.
    pub theme_dark: String,
    pub theme_light: String,
    pub symbols: SymbolPreset,
    /// Blue instead of green for additions in diffs.
    pub color_blind: bool,
    pub status: StatusPreset,
    pub status_separator: StatusSeparator,
    /// Segment names for `status = "custom"`: spinner, model, role, path, git, context, tokens, rate,
    /// approval, sandbox, learning, session, time.
    pub status_left: Vec<String>,
    pub status_right: Vec<String>,
    pub status_path_abbreviate: bool,
    pub status_git_counts: bool,
    /// Use the terminal background instead of a tinted status line.
    pub status_transparent: bool,
    /// Tint the composer border with a per-session color.
    pub session_accent: bool,
    pub composer_shape: ComposerShape,
    pub welcome: bool,
    pub tips: bool,
    pub show_tools: bool,
    /// Lines of tool output shown before collapsing (Ctrl-O expands).
    pub tool_output_lines: usize,
    pub show_turn_stats: bool,
    pub show_thinking: bool,
    pub hints: bool,
    pub autocomplete_items: usize,
    pub history_limit: usize,
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            theme_dark: "dark".into(),
            theme_light: "light".into(),
            symbols: SymbolPreset::Auto,
            color_blind: false,
            status: StatusPreset::Default,
            status_separator: StatusSeparator::PowerlineThin,
            status_left: vec![
                "spinner".into(),
                "model".into(),
                "path".into(),
                "git".into(),
            ],
            status_right: vec!["context".into()],
            status_path_abbreviate: true,
            status_git_counts: true,
            status_transparent: false,
            session_accent: true,
            composer_shape: ComposerShape::Box,
            welcome: true,
            tips: true,
            show_tools: true,
            tool_output_lines: 8,
            show_turn_stats: true,
            show_thinking: true,
            hints: true,
            autocomplete_items: 8,
            history_limit: 500,
        }
    }
}

/// Only roles that have real consumers in Dial are exposed by the selector.
pub const ROLES: &[(&str, &str)] = &[
    ("main", "Main conversation and coding agent"),
    ("feedback", "Independent self-improvement feedback agent"),
    ("subagent", "Delegated investigation tasks"),
    ("compaction", "Conversation summarization"),
    ("architecture", "Saved architecture synthesis"),
    ("experience", "Optional verified-workflow teacher"),
    (
        "stronger",
        "Narrow escalation advice (remote requires explicit opt-in)",
    ),
];

impl ModelSelection {
    pub fn display_name(&self) -> String {
        let path = std::path::Path::new(&self.model);
        let name = path
            .components()
            .find_map(|part| {
                part.as_os_str()
                    .to_str()
                    .and_then(|s| s.strip_prefix("models--"))
                    .map(|s| s.replace("--", "/"))
            })
            .unwrap_or_else(|| {
                if path.is_absolute() {
                    path.file_name()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.model.clone())
                } else {
                    self.model.clone()
                }
            });
        format!("{}/{}", self.provider, name)
    }
}
