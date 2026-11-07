//! Shared settings declarations used by configuration validation, CLI help and the settings panel. Every entry names a real configuration key (a field of
//! `types::Config` or `[ui]`), carries the real default and the same range the configuration
//! validation enforces, so the panel can reject bad input before the controller does. The tests
//! cross-check each entry against `Config::default()`.
use toml::{Table, Value};

/// Settings that form one valid override. Consumers still stage and publish changes themselves;
/// a bulk reset must review every stored partner rather than silently expanding its selection.
pub fn pair_partner(path: &str) -> Option<&'static str> {
    match path {
        "memory.semantic.embedding_base_url" => Some("memory.semantic.embedding_model"),
        "memory.semantic.embedding_model" => Some("memory.semantic.embedding_base_url"),
        _ => None,
    }
}

pub type EnumValues = &'static [(&'static str, &'static str)];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    /// A boolean, or `auto`, which the configuration resolves from the model it serves.
    AutoBool,
    /// `(stored value, label)` pairs, in cycling order.
    Enum(EnumValues),
    /// Suggested picker values for an open string domain (for example a model selector).
    Choice(EnumValues),
    Int {
        min: i64,
        max: i64,
        step: i64,
    },
    /// Auto follows the model; an integer sets a retained-context cap.
    AutoInt {
        min: i64,
        max: i64,
        step: i64,
    },
    Float {
        min: f64,
        max: f64,
        step: f64,
    },
    AutoFloat {
        min: f64,
        max: f64,
        step: f64,
    },
    Text,
    StringList,
    /// A TOML inline table, validated against the complete configuration before saving.
    Table,
    /// A complete TOML value, including arrays of tables and optional structured settings.
    /// The controller still deserializes and validates the complete configuration before saving.
    Toml,
    /// Names come from the theme manager (passed to `SettingsOverlay::new`).
    Theme,
}

#[derive(Clone, Copy, Debug)]
pub struct SettingDef {
    /// Dotted config key, e.g. `ui.status_separator`.
    pub path: &'static str,
    pub tab: &'static str,
    pub section: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub kind: Kind,
    /// The change only takes effect for a new session.
    pub restart: bool,
    accessor: Option<fn(&crate::config::types::Config) -> Option<Value>>,
}

impl SettingDef {
    /// Original metadata remains searchable after presentation-only regrouping.
    pub fn search_location(&self) -> (&'static str, &'static str) {
        declarations()
            .chain(management::DECLARATIONS)
            .find(|def| def.path == self.path)
            .map_or((self.tab, self.section), |def| (def.tab, def.section))
    }
    /// Goal-oriented discovery terms shared by every settings surface. These are static
    /// metadata, never configured values (which may contain credentials).
    pub fn search_terms(&self) -> &'static str {
        match self.path {
            "approval" => "stop asking fewer prompts permissions approval confirm confirmations",
            "sandbox.allow_network" => "stay offline no network internet privacy",
            "sandbox.mode" => "stay offline confinement isolation safety permissions",
            "sandbox.extra_write" | "extra_dirs" => {
                "allow directories folders writable paths permissions access"
            }
            "isolation.backend" => "isolate workers separate checkout workspace safety",
            "budget" => "spend less cost costs cheaper money spending limit budget",
            "thinking_auto" | "thinking_level" | "harness.max_tokens" => {
                "spend less faster cheaper reasoning thinking tokens latency"
            }
            "ui.notify" | "ui.notify_errors" | "ask.notify" => {
                "notifications alerts notify bell attention finished failure questions"
            }
            "providers" | "endpoint" | "provider" => {
                "connections connect credentials authentication login api server endpoint provider"
            }
            "mcp" | "mcp_settings" => "connections connect mcp external tools servers integrations",
            "keys" => "keyboard shortcuts hotkeys bindings remap keys",
            "ui.theme" => "appearance colors colours dark light theme",
            path if path.starts_with("memory.") => {
                "memory remember forget recall knowledge facts previous sessions persistence"
            }
            path if path.starts_with("improve.") || path.starts_with("training.") => {
                "learning learn improve training fine tune finetune adaptation"
            }
            _ => "",
        }
    }
    /// Collections that may contain credentials never render their contents in the panel.
    pub fn sensitive(&self) -> bool {
        matches!(
            self.path,
            "secrets.values"
                | "mcp"
                | "telemetry"
                | "telemetry.headers"
                | "endpoint"
                | "providers"
                | "model_roles"
                | "model_favorites"
                | "model_presets"
                | "tools"
                | "hooks"
                | "dap"
                | "lsp"
        )
    }
    /// Machine-readable UI constraints; open choices remain suggestions, never closed enums.
    pub fn constraints(&self) -> serde_json::Value {
        use serde_json::json;
        match self.kind {
            Kind::Bool => json!({"type":"boolean"}),
            Kind::AutoBool => json!({"anyOf":[{"type":"boolean"},{"const":"auto"}]}),
            Kind::Enum(values) => {
                json!({"type":"string","enum":values.iter().map(|(value,_)| *value).collect::<Vec<_>>(),"labels":values})
            }
            Kind::Choice(values) => json!({"type":"string","suggestions":values}),
            Kind::Int { min, max, step } => {
                json!({"type":"integer","minimum":min,"maximum":max,"step":step})
            }
            Kind::AutoInt { min, max, step } => {
                json!({"anyOf":[{"type":"integer","minimum":min,"maximum":max},{"const":"auto"}],"step":step})
            }
            Kind::Float { min, max, step } => {
                json!({"type":"number","minimum":min,"maximum":max,"step":step})
            }
            Kind::AutoFloat { min, max, step } => {
                json!({"anyOf":[{"type":"number","minimum":min,"maximum":max},{"const":"auto"}],"step":step})
            }
            Kind::Text | Kind::Theme => json!({"type":"string"}),
            Kind::StringList => json!({"type":"array","items":{"type":"string"}}),
            Kind::Table => json!({"type":"object"}),
            Kind::Toml => json!({"format":"toml"}),
        }
    }
    pub fn kind_for(&self, values: &Table) -> Kind {
        if self.path == "harness.max_tokens"
            && let Ok(config) =
                Value::Table(values.clone()).try_into::<crate::config::types::Config>()
        {
            return Kind::Int {
                min: 0,
                max: i64::from(
                    config
                        .active_model_profile()
                        .and_then(|profile| profile.max_tokens)
                        .unwrap_or(8192)
                        .max(8192),
                ),
                step: 256,
            };
        }
        self.kind
    }
    /// The built-in default as a value (`None` if the literal is malformed, which a test forbids).
    pub fn default_value(&self) -> Option<Value> {
        static CONFIG: std::sync::LazyLock<crate::config::types::Config> =
            std::sync::LazyLock::new(crate::config::types::Config::default);
        static DEFAULTS: std::sync::LazyLock<Table> = std::sync::LazyLock::new(|| {
            Table::try_from(crate::config::types::Config::default())
                .expect("configuration defaults serialize")
        });
        self.accessor
            .and_then(|get| get(&CONFIG))
            .or_else(|| get(&DEFAULTS, self.path).cloned())
            .or_else(|| match self.kind {
                Kind::AutoInt { .. } | Kind::AutoFloat { .. } | Kind::AutoBool => {
                    Some(Value::String("auto".into()))
                }
                Kind::Text => Some(Value::String(String::new())),
                Kind::Toml => Some(Value::Table(Table::new())),
                _ => None,
            })
    }
}

const SYMBOLS: EnumValues = &[
    ("auto", "Auto"),
    ("unicode", "Unicode"),
    ("nerd", "Nerd Font"),
    ("ascii", "ASCII"),
];
const STATUS: EnumValues = &[
    ("default", "Default"),
    ("minimal", "Minimal"),
    ("compact", "Compact"),
    ("full", "Full"),
    ("nerd", "Nerd"),
    ("ascii", "ASCII"),
    ("custom", "Custom"),
];
const PINNED_WORKERS: EnumValues = &[("collapsed", "Collapsed"), ("full", "Full"), ("off", "Off")];
const SEPARATOR: EnumValues = &[
    ("powerline", "Powerline"),
    ("powerline-thin", "Powerline thin"),
    ("slash", "Slash"),
    ("pipe", "Pipe"),
    ("block", "Block"),
    ("none", "None"),
    ("ascii", "ASCII"),
];
const SHAPE: EnumValues = &[("band", "Band"), ("box", "Box"), ("line", "Line")];
const MODEL_ROLES: EnumValues = &[
    ("main", "Main"),
    ("small", "Small"),
    ("task", "Task"),
    ("coach", "Coach"),
    ("tester", "Tester"),
    ("memory", "Memory"),
    ("decision", "Decision"),
    ("image", "Image"),
];
const SERVICE_TIER: EnumValues = &[
    ("none", "None"),
    ("auto", "Auto"),
    ("default", "Default"),
    ("flex", "Flex"),
    ("scale", "Scale"),
    ("priority", "Priority"),
    ("ultrafast", "Ultrafast"),
];
const APPROVAL: EnumValues = &[
    ("ask", "Ask first"),
    ("write", "Edit freely"),
    ("auto", "Full auto"),
    ("readonly", "Read-only"),
];
const SANDBOX: EnumValues = &[("off", "Off"), ("dev", "Dev"), ("strict", "Strict")];
const ISOLATION_BACKENDS: EnumValues = &[
    ("auto", "Auto"),
    ("clone", "Clone"),
    ("overlay", "Overlay"),
    ("worktree", "Worktree"),
    ("projfs", "ProjFS"),
    ("copy", "Copy"),
];
const SHELL_BACKEND: EnumValues = &[
    ("auto", "Auto"),
    ("system", "System"),
    ("embedded", "Embedded"),
    ("inprocess", "In-process (sandbox off only)"),
    ("powershell", "PowerShell"),
];
const GIT_GUARD: EnumValues = &[
    ("workers", "Workers sharing the checkout"),
    ("always", "Always"),
    ("off", "Off"),
];
const RULE_JUDGE: EnumValues = &[
    ("auto", "When the decision role is assigned"),
    ("on", "Always (decision, small or main model)"),
    ("off", "Off"),
];
const EDIT_MODE: EnumValues = &[
    ("hashline", "Hashline"),
    ("replace", "Replace"),
    ("apply_patch", "Apply patch"),
    ("patch", "Structured patch"),
    ("sloppy", "Sloppy"),
];
const LOCAL_EDIT_MODE: EnumValues = &[
    ("inherit", "Inherit [edit] mode"),
    ("hashline", "Hashline"),
    ("replace", "Replace"),
    ("apply_patch", "Apply patch"),
    ("patch", "Structured patch"),
    ("sloppy", "Sloppy"),
];
const LOCAL_THINKING: EnumValues = &[("phase", "Phase"), ("model", "Model decides")];
const REPRO_FIRST: EnumValues = &[("off", "Off"), ("advise", "Advise"), ("gate", "Gate")];
const LOCAL_SIDE_MODELS: EnumValues = &[
    ("warn", "Warn once"),
    ("reuse_main", "Reuse main model"),
    ("allow", "Allow silently"),
];
const FALLBACK_ON: EnumValues = &[
    ("any", "Any recoverable failure"),
    ("usage-limit", "Usage limits only"),
    ("except-usage-limit", "Everything but usage limits"),
];
const FALLBACK_REVERT: EnumValues = &[
    ("cooldown-expiry", "When the fallback cooldown ends"),
    ("never", "Stay on the fallback until a model switch"),
];
const COMPACTION_STRATEGY: EnumValues = &[
    ("context-full", "Context full"),
    ("handoff", "Handoff"),
    ("shake", "Shake"),
    ("snapcompact", "Image archive"),
    ("off", "Off"),
];
const SNAP_SYSTEM: EnumValues = &[
    ("none", "Keep system outside archive"),
    ("agents-md", "Archive separate AGENTS.md data"),
    ("all", "Archive system copies"),
];
const SNAP_IMAGE_DETAIL: EnumValues = &[
    ("auto", "Automatic per model and wire"),
    ("high", "High detail"),
    ("original", "Original detail"),
];
const SNAP_SHAPES: EnumValues = &[
    ("auto", "Automatic provider layout"),
    ("8x8r-bw", "8x8 repeated"),
    ("8x8r-sent", "8x8 repeated sentence ink"),
    ("8x8u-bw", "8x8"),
    ("8x8u-sent", "8x8 sentence ink"),
    ("6x6u-bw", "6x6"),
    ("6x6u-sent", "6x6 sentence ink"),
    ("5x8-bw", "5x8"),
    ("5x8-sent", "5x8 sentence ink"),
    ("6x12-dim", "6x12 dim"),
    ("8x13-bw", "8x13"),
    ("8on16-bw", "8 on 16"),
    ("8on22-bw", "8 on 22"),
    ("11on16-bw", "11 on 16"),
    ("silver16-bw", "Silver Unicode"),
    ("doc-8on16-bw", "Two columns"),
    ("doc-8on16-sent", "Two columns sentence ink"),
    ("doc-8on16-sent-dim", "Two columns sentence ink and dim"),
];
const ARCHITECTURE_MODELS: EnumValues = &[
    (
        "mlx-community/Qwen2.5-3B-Instruct-4bit",
        "Qwen2.5-3B Instruct 4-bit",
    ),
    (
        "mlx-community/Qwen2.5-7B-Instruct-4bit",
        "Qwen2.5-7B Instruct 4-bit",
    ),
    (
        "mlx-community/Llama-3.2-1B-Instruct-4bit",
        "Llama 3.2 1B Instruct 4-bit",
    ),
    (
        "mlx-community/Llama-3.2-3B-Instruct-4bit",
        "Llama 3.2 3B Instruct 4-bit",
    ),
    (
        "mlx-community/Mistral-Small-24B-Instruct-4bit",
        "Mistral Small 24B Instruct 4-bit",
    ),
    ("mlx-community/gemma-3-4b-it-4bit", "Gemma 3 4B IT 4-bit"),
    ("mlx-community/gemma-3-12b-it-4bit", "Gemma 3 12B IT 4-bit"),
    ("mlx-community/gemma-3-27b-it-4bit", "Gemma 3 27B IT 4-bit"),
    ("", "Follow main model"),
];

const AP: &str = "Appearance";
const MO: &str = "Model";
const IN: &str = "Interaction";
const TO: &str = "Tools";
const LE: &str = "Memory";
const IM: &str = "Improvement";

const EX: &str = "Experimental";
const VO: &str = "Voice";
const TTS_BACKENDS: EnumValues = &[
    ("auto", "Auto"),
    ("say", "macOS say"),
    ("piper", "Piper"),
    ("espeak", "espeak-ng"),
    ("openai", "OpenAI"),
];
const STT_BACKENDS: EnumValues = &[
    ("auto", "Auto"),
    ("openai", "OpenAI"),
    ("whisper-cpp", "whisper.cpp"),
    ("mlx-whisper", "MLX Whisper"),
];
macro_rules! def {
    ($path:expr, $tab:expr, $section:expr, $label:expr, $desc:expr, $kind:expr $(,)?) => {
        def!($path, $tab, $section, $label, $desc, $kind, false)
    };
    ($path:expr, $tab:expr, $section:expr, $label:expr, $desc:expr, $kind:expr, $restart:expr $(,)?) => {
        SettingDef {
            path: $path,
            tab: $tab,
            section: $section,
            label: $label,
            description: $desc,
            kind: $kind,
            restart: $restart,
            accessor: None,
        }
    };
}

macro_rules! field {
    ($path:expr, $($field:ident).+, $($metadata:expr),+ $(,)?) => {{
        let mut declaration = def!($path, $($metadata),+);
        declaration.accessor = Some(|config| Value::try_from(&config.$($field).+).ok());
        declaration
    }};
}
mod compaction;
mod coordination;
mod experimental;
mod ida;
mod image;
mod improvement;
mod management;
mod memory;
mod message_routing;
mod model;
mod preparation;
mod sources;
mod tools;
pub use sources::{
    COMPACTION_METHODS, FOREIGN_SOURCES, ModelUse, Names, PathKind, RECALL_TYPES, STATUS_SEGMENTS,
    Source,
};
mod voice;
mod web;
pub(super) static APPEARANCE: &[SettingDef] = &[
    field!(
        "ui.theme",
        ui.theme,
        AP,
        "Theme",
        "Theme",
        "Color theme. `auto` follows the terminal background using the dark and light themes below.",
        Kind::Theme,
    ),
    field!(
        "ui.theme_dark",
        ui.theme_dark,
        AP,
        "Theme",
        "Dark theme",
        "Theme used by `auto` on a dark terminal.",
        Kind::Theme,
    ),
    field!(
        "ui.theme_light",
        ui.theme_light,
        AP,
        "Theme",
        "Light theme",
        "Theme used by `auto` on a light terminal.",
        Kind::Theme,
    ),
    field!(
        "ui.color_blind",
        ui.color_blind,
        AP,
        "Theme",
        "Color-blind palette",
        "Show diff additions in blue instead of green.",
        Kind::Bool,
    ),
    field!(
        "ui.symbols",
        ui.symbols,
        AP,
        "Symbols",
        "Symbol set",
        "Glyphs for borders, icons and separators. Auto uses Nerd Font glyphs when the terminal advertises them.",
        Kind::Enum(SYMBOLS),
    ),
    field!(
        "ui.composer_shape",
        ui.composer_shape,
        AP,
        "Composer",
        "Composer shape",
        "Band: a shaded band with the status line inside it. Box: rounded frame with the status line in its border. Line: plain rules, easier to copy from.",
        Kind::Enum(SHAPE),
    ),
    field!(
        "ui.session_accent",
        ui.session_accent,
        AP,
        "Composer",
        "Session accent",
        "Tint the composer border with a color derived from the session id.",
        Kind::Bool,
    ),
    field!(
        "ui.hints",
        ui.hints,
        AP,
        "Composer",
        "Key hints",
        "Show key hints in the composer's bottom border.",
        Kind::Bool,
    ),
    field!(
        "ui.word_completion",
        ui.word_completion,
        AP,
        "Composer",
        "Word completion",
        "Suggest the rest of the word under the cursor as dim text, from words you have used in prompts and this project's identifiers. Tab or Right accepts. Learned words stay in your DIAL_HOME; secret-looking text is never learned.",
        Kind::Bool,
    ),
    field!(
        "ui.status",
        ui.status,
        AP,
        "Status Line",
        "Preset",
        "Which segments the status line shows. Custom uses the left and right lists below.",
        Kind::Enum(STATUS),
    ),
    field!(
        "ui.status_separator",
        ui.status_separator,
        AP,
        "Status Line",
        "Separator",
        "Glyph between status line segments.",
        Kind::Enum(SEPARATOR),
    ),
    field!(
        "ui.status_left",
        ui.status_left,
        AP,
        "Status Line",
        "Left segments",
        "Segments for the custom preset: spinner, model, mode, thinking, inference (last local request settings), role, path, git, context, tokens, rate, cost (observed API cost of the turn and session; hidden when unknown or zero), approval, sandbox, learning, usage, session, time, workers.",
        Kind::StringList,
    ),
    field!(
        "ui.status_right",
        ui.status_right,
        AP,
        "Status Line",
        "Right segments",
        "Segments right-aligned for the custom preset (same names as the left list).",
        Kind::StringList,
    ),
    field!(
        "ui.status_path_abbreviate",
        ui.status_path_abbreviate,
        AP,
        "Status Line",
        "Abbreviate path",
        "Show `~` for the home directory and only the last two path components.",
        Kind::Bool,
    ),
    field!(
        "ui.status_git_counts",
        ui.status_git_counts,
        AP,
        "Status Line",
        "Git counts",
        "Show staged, modified and untracked counts next to the branch.",
        Kind::Bool,
    ),
    field!(
        "ui.status_transparent",
        ui.status_transparent,
        AP,
        "Status Line",
        "Transparent background",
        "Use the terminal background instead of a tinted status line.",
        Kind::Bool,
    ),
    field!(
        "ui.welcome",
        ui.welcome,
        AP,
        "Display",
        "Welcome banner",
        "Show the welcome banner when a session starts.",
        Kind::Bool,
    ),
    field!(
        "ui.tips",
        ui.tips,
        AP,
        "Display",
        "Tips",
        "Show a usage tip under the welcome banner.",
        Kind::Bool,
    ),
    field!(
        "ui.motion",
        ui.motion,
        AP,
        "Display",
        "Motion",
        "Brief animation: one sweep across the wordmark at start-up and a short reveal when a panel opens. Never runs without truecolor, with ASCII symbols or under NO_COLOR.",
        Kind::Bool,
    ),
    field!(
        "ui.smooth_reveal",
        ui.smooth_reveal,
        AP,
        "Display",
        "Smooth reveal",
        "Show a streaming answer gradually instead of in network bursts. Finished and tool output always appears whole at once.",
        Kind::Bool,
    ),
    field!(
        "ui.show_tools",
        ui.show_tools,
        AP,
        "Display",
        "Show tool activity",
        "Show tool calls in the transcript. Errors are always shown.",
        Kind::Bool,
    ),
    field!(
        "ui.tool_output_lines",
        ui.tool_output_lines,
        AP,
        "Display",
        "Tool output lines",
        "Lines of tool output shown before collapsing (Ctrl+O expands).",
        Kind::Int {
            min: 0,
            max: 1000,
            step: 1
        },
    ),
    field!(
        "ui.session_rail",
        ui.session_rail,
        AP,
        "Display",
        "Session rail",
        "Pin workspace change counts above the composer. Includes pre-existing work.",
        Kind::Bool,
    ),
    field!(
        "ui.pinned_workers",
        ui.pinned_workers,
        AP,
        "Display",
        "Pinned workers",
        "Running workers above the composer: collapsed (up to three), full, or off.",
        Kind::Enum(PINNED_WORKERS),
    ),
    field!(
        "ui.worker_live_preview",
        ui.worker_live_preview,
        AP,
        "Display",
        "Worker live preview",
        "Show each pinned worker's current tool or step on a second row.",
        Kind::Bool,
    ),
    field!(
        "ui.show_turn_stats",
        ui.show_turn_stats,
        AP,
        "Display",
        "Turn statistics",
        "Show duration, tokens and speed under each turn.",
        Kind::Bool,
    ),
    field!(
        "ui.show_thinking",
        ui.show_thinking,
        AP,
        "Display",
        "Show reasoning",
        "Show the model's reasoning text; off collapses it to a one-line summary.",
        Kind::Bool,
    ),
    field!(
        "ui.mouse",
        ui.mouse,
        IN,
        "Terminal",
        "Mouse in panels",
        "Wheel scrolling and clicks inside full-screen panels. The inline view never captures the mouse.",
        Kind::Bool,
    ),
    field!(
        "ui.notify",
        ui.notify,
        IN,
        "Notifications",
        "Completion notification",
        "Notify (OSC 9/777/99, or a bell) when a turn finishes or a question waits for you.",
        Kind::Bool,
    ),
    field!(
        "ui.notify_errors",
        ui.notify_errors,
        IN,
        "Notifications",
        "Error notification",
        "Also notify when a turn fails, is blocked or ends incomplete. A turn you walked away from is the one that most needs a ping.",
        Kind::Bool,
    ),
    field!(
        "ui.terminal_integration",
        ui.terminal_integration,
        IN,
        "Notifications",
        "Terminal integration",
        "Tab state circle and title, attention, progress, status variables, working directory and file links in terminals that support them (iTerm2, Ghostty, WezTerm, kitty).",
        Kind::Bool,
    ),
];

pub(super) static COMPOSER: &[SettingDef] = &[
    field!(
        "ui.autocomplete_items",
        ui.autocomplete_items,
        IN,
        "Input",
        "Menu rows",
        "Rows shown in the slash-command menu before it scrolls.",
        Kind::Int {
            min: 1,
            max: 50,
            step: 1
        },
    ),
    field!(
        "ui.history_limit",
        ui.history_limit,
        IN,
        "Input",
        "History size",
        "Submitted prompts remembered for Up and Ctrl+R; 0 means unlimited.",
        Kind::Int {
            min: 0,
            max: 10000,
            step: 100
        },
    ),
    field!(
        "ui.busy_send",
        ui.busy_send,
        IN,
        "Input",
        "Enter while working",
        "Choose opens a send-action chooser without sending. Other choices send directly: redirect at a safe point, queue after the turn, request a worker, or use automatic routing. Never learned from previous sends.",
        Kind::Enum(&[
            ("choose", "Choose each time"),
            ("redirect", "Redirect current task"),
            ("after_turn", "Send after turn"),
            ("independent", "Run independently"),
            ("auto", "Let Dial choose (auto)"),
        ]),
    ),
];
// Domain-owned groups retain observable order without numeric offsets into unrelated arrays.
const GROUPS: &[&[SettingDef]] = &[
    APPEARANCE,
    model::MODEL,
    memory::DECLARATIONS,
    COMPOSER,
    tools::APPROVAL_POLICY,
    improvement::LEARNING,
    tools::SANDBOX_AND_EDITING,
    compaction::COMPACTION,
    tools::EXECUTION,
    improvement::ASSISTANCE,
    experimental::EXPERIMENTS,
    preparation::DECLARATIONS,
    coordination::DECLARATIONS,
    message_routing::DECLARATIONS,
    voice::DECLARATIONS,
    ida::DECLARATIONS,
    image::DECLARATIONS,
    web::DECLARATIONS,
    tools::INTEGRATIONS,
    model::EFFICIENCY,
    tools::EVAL,
    experimental::OUTPUT_EFFORT,
    compaction::ARGUMENT_PROJECTION,
];
/// Original metadata before presentation grouping. No intermediate registry allocation.
fn declarations() -> impl Iterator<Item = &'static SettingDef> {
    GROUPS.iter().flat_map(|group| group.iter())
}

const fn group_len(groups: &[&[SettingDef]]) -> usize {
    let mut size = 0;
    let mut index = 0;
    while index < groups.len() {
        size += groups[index].len();
        index += 1;
    }
    size
}
// Adding to an existing group is automatic. Adding a new group must also assemble it here.
const _: () = assert!(
    group_len(GROUPS)
        == APPEARANCE.len()
            + COMPOSER.len()
            + group_len(model::GROUPS)
            + group_len(tools::GROUPS)
            + group_len(improvement::GROUPS)
            + group_len(compaction::GROUPS)
            + group_len(experimental::GROUPS)
            + memory::DECLARATIONS.len()
            + preparation::DECLARATIONS.len()
            + coordination::DECLARATIONS.len()
            + message_routing::DECLARATIONS.len()
            + voice::DECLARATIONS.len()
            + ida::DECLARATIONS.len()
            + image::DECLARATIONS.len()
            + web::DECLARATIONS.len()
);

pub fn registry() -> &'static [SettingDef] {
    static ALL: std::sync::LazyLock<Vec<SettingDef>> = std::sync::LazyLock::new(|| {
        declarations()
            .chain(management::DECLARATIONS)
            .copied()
            .map(|mut def| {
                def.tab = settings_group(def.path, def.tab, def.section);
                if COMMON_SETTINGS.contains(&def.path) {
                    def.tab = "Common";
                    def.section = "Common";
                }
                def
            })
            .collect()
    });
    &ALL
}

/// Frequently changed settings, surfaced first without duplicating declarations or values.
pub const COMMON_SETTINGS: &[&str] = &[
    "ui.theme",
    "thinking_auto",
    "ui.motion",
    "ui.show_turn_stats",
];

fn settings_group(path: &str, tab: &'static str, section: &str) -> &'static str {
    // Structured collections retain their original searchable locations, but belong to
    // their purpose here rather than the generic Configuration section they came from.
    if matches!(
        path,
        "providers"
            | "endpoint"
            | "provider"
            | "mcp"
            | "mcp_settings"
            | "ssh_hosts"
            | "ssh"
            | "snowflake_account"
            | "cloudflare_account_id"
            | "cloudflare_gateway_id"
    ) {
        return "Connections";
    }
    if matches!(path, "approval" | "extra_dirs" | "secrets.values")
        || path.starts_with("sandbox.")
        || path.starts_with("isolation.")
        || path.starts_with("permissions.")
    {
        return "Safety";
    }
    if path.starts_with("memory.") {
        return "Memory";
    }
    match (tab, section) {
        ("Appearance" | "Voice", _) | ("Interaction", "Notifications") => "Appearance",
        ("Interaction", "Approvals") | ("Tools", "Sandbox" | "Secrets" | "Portable rules") => {
            "Safety"
        }
        ("Interaction", "Input" | "Terminal") => "Keys",
        ("Improvement", _) | ("Model", "Learning" | "Adaptive policy" | "Laya") => "Learning",
        ("Model", "Runtime" | "Local worker") => "Advanced",
        ("Model", _) => "Model",
        ("Tools", _) => "Tools",
        // Opt-in experiments stay visibly separate rather than blending into Advanced.
        ("Experimental", _) => "Experimental",
        _ => "Advanced",
    }
}

/// Checks a value written outside `/settings` (the CLI) against its schema entry, so a value the
/// overlay could never produce is refused before it is saved. Keys without an entry are left to
/// `Config` validation.
pub fn check_value(path: &str, value: &Value) -> Result<(), String> {
    check_value_for(&Table::new(), path, value)
}

pub fn check_value_for(values: &Table, path: &str, value: &Value) -> Result<(), String> {
    let Some(def) = registry().iter().find(|def| def.path == path) else {
        return Ok(());
    };
    let number = value
        .as_float()
        .or_else(|| value.as_integer().map(|n| n as f64));
    let fits = match def.kind_for(values) {
        Kind::Bool => value.is_bool(),
        Kind::AutoBool => value.is_bool() || value.as_str() == Some("auto"),
        Kind::Enum(options) => value
            .as_str()
            .is_some_and(|s| options.iter().any(|(stored, _)| *stored == s)),
        Kind::Int { min, max, .. } => value.as_integer().is_some_and(|n| (min..=max).contains(&n)),
        Kind::AutoInt { min, max, .. } => {
            value.as_str() == Some("auto")
                || value.as_integer().is_some_and(|n| (min..=max).contains(&n))
        }
        Kind::Float { min, max, .. } => number.is_some_and(|f| (min..=max).contains(&f)),
        Kind::AutoFloat { min, max, .. } => {
            value.as_str() == Some("auto") || number.is_some_and(|f| (min..=max).contains(&f))
        }
        Kind::Text | Kind::Theme | Kind::Choice(_) => value.is_str(),
        Kind::StringList => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_str)),
        Kind::Table => value.is_table(),
        Kind::Toml => true,
    };
    if fits {
        Ok(())
    } else {
        Err(format!(
            "{path} does not accept that value; /settings shows its allowed values and range"
        ))
    }
}

/// Canonical order of the settings panel: each tab with its sections. Declaration order in the
/// registry modules is an implementation detail, so the panel follows this table instead:
/// everyday coding controls first, learning and voice after them, experiments last. A section
/// missing here is a test failure (`layout_lists_every_registered_section`), so new settings
/// always get a deliberate place.
const LAYOUT: &[(&str, &[&str])] = &[
    (
        AP,
        &["Theme", "Symbols", "Composer", "Status Line", "Display"],
    ),
    (IN, &["Approvals", "Input", "Notifications", "Terminal"]),
    (
        MO,
        &[
            "Context",
            "Requests",
            "Roles & Sampling",
            "Compaction",
            "Project understanding",
            "Cache warming",
            "Local worker",
            "Runtime",
            "Laya",
            "Learning",
            "Adaptive policy",
            "Configuration",
        ],
    ),
    (
        TO,
        &[
            "Sandbox",
            "Shell",
            "Editing",
            "Verification",
            "Language servers",
            "Web",
            "Browser",
            "JavaScript code mode",
            "Image generation",
            "IDA",
            "Subagents",
            "Session coordination",
            "Portable rules",
            "Worktree",
            "Commit",
            "Questions",
            "Investigation",
            "Skills",
            "Context",
            "Secrets",
            "Configuration",
        ],
    ),
    (
        LE,
        &[
            "Memory",
            "Semantic memory",
            "Rollout memory",
            "Hindsight memory",
            "Decision memory",
        ],
    ),
    (
        IM,
        &[
            "Automatic improvement",
            "Local model assistance",
            "Tool reliability",
            "Learn from other models",
            "Training",
            "Escalation",
            "Activation steering",
            "Local runtime",
            "Weights",
            "Configuration",
        ],
    ),
    (VO, &["Read-aloud", "Dictation", "Configuration"]),
    (
        EX,
        &[
            "Reasoning",
            "Execution",
            "Verification",
            "Context",
            "Agent loop",
            "Agent routing",
            "Agent coordination",
            "Preparation",
            "Media",
            "Configuration",
        ],
    ),
];

/// Tabs and their sections in panel order (see [`LAYOUT`]). Original locations determine
/// section order even when path-specific presentation groups split a declaration section.
/// Setup launchers belong to the UI rather than the settings registry.
pub fn tabs() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut out: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for def in registry() {
        match out.iter_mut().find(|(tab, _)| *tab == def.tab) {
            Some((_, sections)) => {
                if !sections.contains(&def.section) {
                    sections.push(def.section)
                }
            }
            None => out.push((def.tab, vec![def.section])),
        }
    }
    let order = [
        "Common",
        "Appearance",
        "Keys",
        "Model",
        "Connections",
        "Safety",
        "Tools",
        "Memory",
        "Learning",
        "Experimental",
        "Advanced",
    ];
    out.sort_by_key(|(tab, _)| {
        order
            .iter()
            .position(|name| name == tab)
            .unwrap_or(usize::MAX)
    });
    for (tab, sections) in &mut out {
        let target = *tab;
        let listed: Vec<_> = LAYOUT
            .iter()
            .flat_map(|(old_tab, sections)| {
                sections.iter().filter_map(move |section| {
                    registry()
                        .iter()
                        .any(|def| {
                            def.tab == target && def.search_location() == (*old_tab, *section)
                        })
                        .then_some(*section)
                })
            })
            .collect();
        sections.sort_by_key(|section| {
            listed
                .iter()
                .position(|name| name == section)
                .unwrap_or(usize::MAX)
        });
    }
    out
}

/// Tuning knobs most people never need (timeouts, leases, retry delays, budgets, TTLs). Each
/// section hides them behind a "Show advanced" row; search still finds them.
const ADVANCED: &[&str] = &[
    "harness.max_steps",
    "worker_timeout_secs",
    "remote_model_idle_timeout_secs",
    "retry.max_attempts",
    "retry.max_delay_secs",
    "reasoning_guard.max_pre_action_secs",
    "reasoning_guard.max_reasoning_bytes",
    "reasoning_guard.require_plan",
    "efficiency.history_token_cap",
    "efficiency.batch_result_percent",
    "laya.timeout_secs",
    "memory.semantic.embedding_timeout_secs",
    "codebase.context_max_bytes",
    "compaction.threshold_tokens",
    "compaction.reserve_tokens",
    "compaction.keep_recent_tokens",
    "compaction.idle_threshold_tokens",
    "compaction.idle_timeout_seconds",
    "compaction.v2_retained_message_budget",
    "compaction.remote_streaming_v2_enabled",
    "compaction.model_thresholds",
    "compaction.agent_threshold_overrides",
    "compaction.native_models",
    "compaction.snapcompact_models",
    "compaction.promotion.targets",
    "compaction.snapcompact.shape",
    "compaction.snapcompact.system_prompt",
    "compaction.snapcompact.tool_results",
    "compaction.snapcompact.overlay",
    "compaction.snapcompact.frame_size",
    "compaction.snapcompact.max_edge",
    "compaction.snapcompact.image_detail",
    "assistance.initial_tool_limit",
    "assistance.lean_local",
    "assistance.local.small_window",
    "assistance.local.reasoning_share",
    "assistance.local.escalate_after",
    "assistance.local.keep_loaded_secs",
    "reliability.measured_interfaces",
    "reliability.examples",
    "reliability.max_contract_failures",
    "adaptive.reasoning",
    "adaptive.output_limits",
    "adaptive.sampling",
    "adaptive.pin_sampling",
    "adaptive.phase",
    "adaptive.investigation_temperature",
    "adaptive.editing_temperature",
    "adaptive.verification_temperature",
    "adaptive.recovery_temperature",
    "adaptive.exploration_temperature",
    "adaptive.investigation_tokens",
    "adaptive.verification_tokens",
    "adaptive.recovery_tokens",
    "adaptive.exploration_tokens",
    "ask.timeout_seconds",
    "commit.cache_ttl_days",
    "edit.fuzzy_threshold",
    "edit.blackbox.enabled",
    "edit.auto_repair.enabled",
    "eval.timeout_secs",
    "subagent_max_live_jobs",
    "isolation.backend",
    "portable_rules.max_regenerations",
    "portable_rules.repeat_after_turns",
    "portable_rules.judge",
    "memory.local.max_rollouts_per_startup",
    "memory.local.max_rollout_age_days",
    "memory.local.min_rollout_idle_hours",
    "memory.local.thread_scan_limit",
    "memory.local.max_raw_memories",
    "memory.local.stage1_concurrency",
    "memory.local.stage1_lease_secs",
    "memory.local.stage1_retry_delay_secs",
    "memory.local.consolidation_lease_secs",
    "memory.local.consolidation_retry_delay_secs",
    "memory.local.consolidation_heartbeat_secs",
    "memory.local.rollout_payload_percent",
    "memory.local.phase1_input_tokens",
    "memory.local.fallback_token_limit",
    "memory.local.summary_injection_tokens",
    "memory.local.timeout_secs",
    "memory.hindsight.recall_budget",
    "memory.hindsight.recall_max_tokens",
    "memory.hindsight.request_timeout_secs",
    "memory.hindsight.retain_every_turns",
    "memory.hindsight.retain_overlap_turns",
    "memory.hindsight.recall_context_turns",
    "memory.hindsight.recall_max_query_chars",
    "memory.hindsight.reflect_timeout_secs",
    "memory.hindsight.recall_timeout_secs",
    "memory.hindsight.retain_timeout_secs",
    "memory.hindsight.mental_model_auto_seed",
    "memory.hindsight.mental_model_max_render_chars",
    "memory.sharpshooter.interval_minutes",
    "memory.sharpshooter.injection_token_limit",
    "memory.sharpshooter.max_file_lines",
    "memory.sharpshooter.max_parallel",
    "memory.sharpshooter.lease_secs",
    "memory.sharpshooter.timeout_secs",
    "improve.interval_secs",
    "improve.min_new_turns",
    "improve.selfplay_tasks",
    "improve.gate_primary_cases",
    "improve.gate_guard_cases",
    "improve.max_candidates",
    "improve.alpha",
    "improve.min_net_wins",
    "improve.drift_tolerance",
    "improve.lora_min_examples",
    "improve.monitor_drop",
    "improve.monitor_min_turns",
    "training.interval_secs",
    "training.iters",
    "training.batch_size",
    "training.num_layers",
    "training.rank",
    "training.max_seq_length",
    "training.learn_from",
    "training.learn_from_all_verified",
    "training.learn_from_max_percent",
    "assistance.context_max_bytes",
    "assistance.teacher_timeout_secs",
];

/// Whether `path` is hidden behind its section's "Show advanced" row.
pub fn is_advanced(path: &str) -> bool {
    // Adaptive-policy knobs tune the harness itself; everything else in a tab stays visible,
    // including the Improvement tab, which people open on purpose.
    ADVANCED.contains(&path)
        || registry()
            .iter()
            .any(|def| def.path == path && def.section == "Adaptive policy")
}

/// Dotted lookup: `get(t, "ui.status")` is `t["ui"]["status"]`.
pub fn get<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

/// Set a dotted path, creating intermediate tables (replacing non-table values in the way).
pub fn set(table: &mut Table, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            table.insert(path.to_owned(), value);
        }
        Some((head, rest)) => {
            let entry = table
                .entry(head.to_owned())
                .or_insert_with(|| Value::Table(Table::new()));
            if !entry.is_table() {
                *entry = Value::Table(Table::new());
            }
            if let Some(inner) = entry.as_table_mut() {
                set(inner, rest, value);
            }
        }
    }
}

/// Remove a dotted path; empty parent tables are dropped. Returns whether anything was removed.
pub fn unset(table: &mut Table, path: &str) -> bool {
    match path.split_once('.') {
        None => table.remove(path).is_some(),
        Some((head, rest)) => {
            let Some(inner) = table.get_mut(head).and_then(Value::as_table_mut) else {
                return false;
            };
            let removed = unset(inner, rest);
            if inner.is_empty() {
                table.remove(head);
            }
            removed
        }
    }
}

#[cfg(test)]
mod tests;
