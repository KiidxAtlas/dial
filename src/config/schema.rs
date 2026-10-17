//! Shared settings declarations used by configuration validation, CLI help and the settings panel. Every entry names a real configuration key (a field of
//! `types::Config` or `[ui]`), carries the real default and the same range the configuration
//! validation enforces, so the panel can reject bad input before the controller does. The tests
//! cross-check each entry against `Config::default()`.
use toml::{Table, Value};

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
        }
    }
    pub fn kind_for(&self, values: &Table) -> Kind {
        if self.path == "harness.max_tokens" {
            if let Ok(config) =
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
const SHAPE: EnumValues = &[("box", "Box"), ("line", "Line")];
const MODEL_ROLES: EnumValues = &[
    ("main", "Main"),
    ("small", "Small"),
    ("task", "Task"),
    ("judge", "Judge"),
    ("coach", "Coach"),
    ("tester", "Tester"),
    ("planner", "Planner"),
    ("memory", "Memory"),
    ("decision", "Decision"),
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
    ("ask", "Ask"),
    ("write", "Write"),
    ("auto", "Auto"),
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
    ("powershell", "PowerShell"),
];
const EDIT_MODE: EnumValues = &[
    ("hashline", "Hashline"),
    ("replace", "Replace"),
    ("apply_patch", "Apply patch"),
    ("patch", "Structured patch"),
    ("sloppy", "Sloppy"),
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
#[path = "settings/compaction.rs"]
mod compaction;
#[path = "settings/experimental.rs"]
mod experimental;
#[path = "settings/improvement.rs"]
mod improvement;
#[path = "settings/memory.rs"]
mod memory;
#[path = "settings/model.rs"]
mod model;
#[path = "settings/tools.rs"]
mod tools;
#[path = "settings/voice.rs"]
mod voice;
static UI: &[SettingDef] = &[
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
        "Box: rounded frame with the status line in its border. Line: plain rules, easier to copy from.",
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
        "Segments for the custom preset: spinner, model, role, path, git, context, tokens, rate, approval, sandbox, learning, session, time.",
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
        "Also notify when a turn stops with an error.",
        Kind::Bool,
    ),
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
];
static REGISTRY: &[SettingDef] = &[
    UI[0],
    UI[1],
    UI[2],
    UI[3],
    UI[4],
    UI[5],
    UI[6],
    UI[7],
    UI[8],
    UI[9],
    UI[10],
    UI[11],
    UI[12],
    UI[13],
    UI[14],
    UI[15],
    UI[16],
    UI[17],
    UI[18],
    UI[19],
    UI[20],
    UI[21],
    UI[22],
    UI[23],
    UI[24],
    UI[25],
    UI[26],
    UI[27],
    UI[28],
    model::DECLARATIONS[32],
    model::DECLARATIONS[0],
    model::DECLARATIONS[1],
    model::DECLARATIONS[2],
    model::DECLARATIONS[3],
    model::DECLARATIONS[4],
    model::DECLARATIONS[5],
    model::DECLARATIONS[6],
    model::DECLARATIONS[7],
    model::DECLARATIONS[8],
    model::DECLARATIONS[9],
    model::DECLARATIONS[10],
    model::DECLARATIONS[11],
    model::DECLARATIONS[33],
    model::DECLARATIONS[12],
    model::DECLARATIONS[13],
    model::DECLARATIONS[14],
    model::DECLARATIONS[15],
    model::DECLARATIONS[16],
    model::DECLARATIONS[17],
    model::DECLARATIONS[18],
    model::DECLARATIONS[19],
    model::DECLARATIONS[20],
    model::DECLARATIONS[21],
    model::DECLARATIONS[22],
    model::DECLARATIONS[23],
    model::DECLARATIONS[24],
    model::DECLARATIONS[25],
    model::DECLARATIONS[26],
    model::DECLARATIONS[27],
    model::DECLARATIONS[28],
    model::DECLARATIONS[29],
    model::DECLARATIONS[30],
    model::DECLARATIONS[31],
    memory::DECLARATIONS[0],
    memory::DECLARATIONS[1],
    memory::DECLARATIONS[2],
    memory::DECLARATIONS[3],
    memory::DECLARATIONS[4],
    memory::DECLARATIONS[5],
    memory::DECLARATIONS[6],
    memory::DECLARATIONS[7],
    memory::DECLARATIONS[8],
    memory::DECLARATIONS[9],
    memory::DECLARATIONS[10],
    memory::DECLARATIONS[11],
    memory::DECLARATIONS[12],
    memory::DECLARATIONS[13],
    memory::DECLARATIONS[14],
    memory::DECLARATIONS[15],
    memory::DECLARATIONS[16],
    memory::DECLARATIONS[17],
    memory::DECLARATIONS[18],
    memory::DECLARATIONS[19],
    memory::DECLARATIONS[20],
    memory::DECLARATIONS[21],
    memory::DECLARATIONS[22],
    memory::DECLARATIONS[23],
    memory::DECLARATIONS[24],
    memory::DECLARATIONS[25],
    memory::DECLARATIONS[26],
    memory::DECLARATIONS[27],
    memory::DECLARATIONS[28],
    memory::DECLARATIONS[29],
    memory::DECLARATIONS[30],
    memory::DECLARATIONS[31],
    memory::DECLARATIONS[32],
    memory::DECLARATIONS[33],
    memory::DECLARATIONS[34],
    memory::DECLARATIONS[35],
    memory::DECLARATIONS[36],
    memory::DECLARATIONS[37],
    memory::DECLARATIONS[38],
    memory::DECLARATIONS[39],
    memory::DECLARATIONS[40],
    memory::DECLARATIONS[41],
    memory::DECLARATIONS[42],
    memory::DECLARATIONS[43],
    memory::DECLARATIONS[44],
    memory::DECLARATIONS[45],
    memory::DECLARATIONS[46],
    memory::DECLARATIONS[47],
    memory::DECLARATIONS[48],
    memory::DECLARATIONS[49],
    memory::DECLARATIONS[50],
    memory::DECLARATIONS[51],
    memory::DECLARATIONS[52],
    memory::DECLARATIONS[53],
    memory::DECLARATIONS[54],
    memory::DECLARATIONS[55],
    memory::DECLARATIONS[56],
    memory::DECLARATIONS[57],
    memory::DECLARATIONS[58],
    memory::DECLARATIONS[59],
    memory::DECLARATIONS[60],
    memory::DECLARATIONS[61],
    memory::DECLARATIONS[62],
    memory::DECLARATIONS[63],
    memory::DECLARATIONS[64],
    memory::DECLARATIONS[65],
    UI[29],
    UI[30],
    tools::DECLARATIONS[0],
    improvement::DECLARATIONS[0],
    improvement::DECLARATIONS[1],
    improvement::DECLARATIONS[2],
    improvement::DECLARATIONS[3],
    improvement::DECLARATIONS[4],
    improvement::DECLARATIONS[5],
    improvement::DECLARATIONS[6],
    improvement::DECLARATIONS[7],
    improvement::DECLARATIONS[8],
    improvement::DECLARATIONS[9],
    improvement::DECLARATIONS[10],
    improvement::DECLARATIONS[11],
    improvement::DECLARATIONS[12],
    improvement::DECLARATIONS[13],
    improvement::DECLARATIONS[14],
    improvement::DECLARATIONS[15],
    improvement::DECLARATIONS[16],
    improvement::DECLARATIONS[17],
    improvement::DECLARATIONS[18],
    improvement::DECLARATIONS[19],
    improvement::DECLARATIONS[20],
    improvement::DECLARATIONS[21],
    improvement::DECLARATIONS[22],
    improvement::DECLARATIONS[23],
    improvement::DECLARATIONS[24],
    improvement::DECLARATIONS[25],
    improvement::DECLARATIONS[26],
    improvement::DECLARATIONS[27],
    improvement::DECLARATIONS[28],
    improvement::DECLARATIONS[29],
    improvement::DECLARATIONS[30],
    improvement::DECLARATIONS[31],
    improvement::DECLARATIONS[32],
    improvement::DECLARATIONS[33],
    improvement::DECLARATIONS[34],
    improvement::DECLARATIONS[35],
    improvement::DECLARATIONS[36],
    improvement::DECLARATIONS[37],
    improvement::DECLARATIONS[38],
    improvement::DECLARATIONS[39],
    improvement::DECLARATIONS[40],
    improvement::DECLARATIONS[41],
    improvement::DECLARATIONS[42],
    improvement::DECLARATIONS[43],
    improvement::DECLARATIONS[44],
    improvement::DECLARATIONS[45],
    improvement::DECLARATIONS[46],
    improvement::DECLARATIONS[47],
    improvement::DECLARATIONS[48],
    improvement::DECLARATIONS[49],
    improvement::DECLARATIONS[50],
    improvement::DECLARATIONS[51],
    improvement::DECLARATIONS[52],
    tools::DECLARATIONS[1],
    tools::DECLARATIONS[2],
    tools::DECLARATIONS[3],
    tools::DECLARATIONS[4],
    tools::DECLARATIONS[5],
    tools::DECLARATIONS[6],
    tools::DECLARATIONS[7],
    tools::DECLARATIONS[8],
    tools::DECLARATIONS[9],
    tools::DECLARATIONS[10],
    tools::DECLARATIONS[11],
    tools::DECLARATIONS[12],
    tools::DECLARATIONS[13],
    tools::DECLARATIONS[14],
    tools::DECLARATIONS[15],
    tools::DECLARATIONS[16],
    tools::DECLARATIONS[17],
    tools::DECLARATIONS[18],
    tools::DECLARATIONS[19],
    tools::DECLARATIONS[20],
    tools::DECLARATIONS[21],
    tools::DECLARATIONS[22],
    compaction::DECLARATIONS[0],
    compaction::DECLARATIONS[1],
    compaction::DECLARATIONS[2],
    compaction::DECLARATIONS[3],
    compaction::DECLARATIONS[4],
    compaction::DECLARATIONS[5],
    compaction::DECLARATIONS[6],
    compaction::DECLARATIONS[7],
    compaction::DECLARATIONS[8],
    compaction::DECLARATIONS[9],
    compaction::DECLARATIONS[10],
    compaction::DECLARATIONS[11],
    compaction::DECLARATIONS[12],
    compaction::DECLARATIONS[13],
    compaction::DECLARATIONS[14],
    compaction::DECLARATIONS[15],
    compaction::DECLARATIONS[16],
    compaction::DECLARATIONS[17],
    compaction::DECLARATIONS[18],
    compaction::DECLARATIONS[19],
    compaction::DECLARATIONS[20],
    compaction::DECLARATIONS[21],
    compaction::DECLARATIONS[22],
    compaction::DECLARATIONS[23],
    compaction::DECLARATIONS[24],
    compaction::DECLARATIONS[25],
    compaction::DECLARATIONS[26],
    compaction::DECLARATIONS[27],
    compaction::DECLARATIONS[28],
    compaction::DECLARATIONS[29],
    tools::DECLARATIONS[23],
    tools::DECLARATIONS[24],
    tools::DECLARATIONS[25],
    tools::DECLARATIONS[26],
    tools::DECLARATIONS[27],
    tools::DECLARATIONS[28],
    tools::DECLARATIONS[29],
    tools::DECLARATIONS[30],
    tools::DECLARATIONS[31],
    tools::DECLARATIONS[32],
    tools::DECLARATIONS[33],
    tools::DECLARATIONS[34],
    improvement::DECLARATIONS[53],
    improvement::DECLARATIONS[54],
    improvement::DECLARATIONS[55],
    improvement::DECLARATIONS[56],
    improvement::DECLARATIONS[57],
    improvement::DECLARATIONS[58],
    improvement::DECLARATIONS[59],
    improvement::DECLARATIONS[60],
    improvement::DECLARATIONS[61],
    improvement::DECLARATIONS[62],
    improvement::DECLARATIONS[63],
    improvement::DECLARATIONS[64],
    improvement::DECLARATIONS[65],
    improvement::DECLARATIONS[66],
    improvement::DECLARATIONS[67],
    improvement::DECLARATIONS[68],
    improvement::DECLARATIONS[69],
    improvement::DECLARATIONS[70],
    improvement::DECLARATIONS[71],
    experimental::DECLARATIONS[0],
    experimental::DECLARATIONS[1],
    experimental::DECLARATIONS[2],
    experimental::DECLARATIONS[3],
    experimental::DECLARATIONS[4],
    voice::DECLARATIONS[0],
    voice::DECLARATIONS[1],
    voice::DECLARATIONS[2],
    voice::DECLARATIONS[3],
    tools::DECLARATIONS[35],
    tools::DECLARATIONS[36],
    tools::DECLARATIONS[37],
    tools::DECLARATIONS[38],
    tools::DECLARATIONS[39],
    tools::DECLARATIONS[40],
];
const _: () = assert!(
    REGISTRY.len()
        == UI.len()
            + model::DECLARATIONS.len()
            + memory::DECLARATIONS.len()
            + improvement::DECLARATIONS.len()
            + tools::DECLARATIONS.len()
            + compaction::DECLARATIONS.len()
            + experimental::DECLARATIONS.len()
            + voice::DECLARATIONS.len()
);

pub fn registry() -> &'static [SettingDef] {
    REGISTRY
}

/// Checks a value written outside `/settings` (the CLI) against its schema entry, so a value the
/// overlay could never produce is refused before it is saved. Keys without an entry are left to
/// `Config` validation.
pub fn check_value(path: &str, value: &Value) -> Result<(), String> {
    check_value_for(&Table::new(), path, value)
}

pub fn check_value_for(values: &Table, path: &str, value: &Value) -> Result<(), String> {
    let Some(def) = REGISTRY.iter().find(|def| def.path == path) else {
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
    };
    if fits {
        Ok(())
    } else {
        Err(format!(
            "{path} does not accept that value; /settings shows its allowed values and range"
        ))
    }
}

/// Tabs and their sections, in registry order. The panel adds a `Providers` tab of its own that
/// launches the provider and model screens; it holds no settings, so it is not listed here.
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
    "compaction.snapcompact.shape",
    "compaction.snapcompact.system_prompt",
    "compaction.snapcompact.tool_results",
    "assistance.initial_tool_limit",
    "assistance.lean_local",
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
mod tests {
    use super::*;
    use crate::{config::types::Config, llm::model_config::UiConfig};
    use std::collections::HashSet;

    fn default_table() -> Table {
        Table::try_from(Config::default()).expect("default config serializes")
    }
    #[test]
    fn suggested_models_remain_open_and_output_bounds_follow_the_selected_profile() {
        check_value(
            "codebase.architecture_model",
            &Value::String("operator/custom-model".into()),
        )
        .unwrap();
        let mut config = Config::default();
        let selection = config.selection();
        config.model_profile = Some("fixture".into());
        config.model_profiles.insert(
            "fixture".into(),
            crate::llm::profiles::ModelProfile {
                provider: selection.provider,
                model: selection.model,
                max_tokens: Some(16384),
                ..Default::default()
            },
        );
        let values = Table::try_from(&config).unwrap();
        assert!(check_value("harness.max_tokens", &Value::Integer(12000)).is_err());
        check_value_for(&values, "harness.max_tokens", &Value::Integer(12000)).unwrap();
        assert!(check_value_for(&values, "harness.max_tokens", &Value::Integer(16385)).is_err());
    }

    #[test]
    fn choice_settings_are_open_suggestions_not_closed_enums() {
        let def = registry()
            .iter()
            .find(|def| def.path == "memory.semantic.embedding_model")
            .expect("embedding model is a setting");
        assert!(matches!(def.kind, Kind::Choice(_)));
        assert_eq!(def.default_value(), Some(Value::String(String::new())));
        let constraints = def.constraints();
        assert!(constraints.get("enum").is_none());
        assert!(
            constraints["suggestions"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
        for value in ["", "operator/custom-model:latest"] {
            check_value(
                "memory.semantic.embedding_model",
                &Value::String(value.into()),
            )
            .unwrap();
        }
        assert!(check_value("memory.semantic.embedding_model", &Value::Integer(1)).is_err());
    }

    #[test]
    fn every_path_is_a_real_key_with_the_real_default() {
        let table = default_table();
        let ui = Table::try_from(UiConfig::default()).unwrap();
        for def in registry() {
            let default = def
                .default_value()
                .unwrap_or_else(|| panic!("{} has no declared default", def.path));
            let Some(actual) = get(&table, def.path) else {
                // `None` fields are left out of the serialized default. A blank or "auto" literal is exactly that unset state.
                let unset = default == Value::String(String::new())
                    || default == Value::String("auto".into());
                assert!(
                    (def.path.starts_with("local_runtime.") || def.path.starts_with("lsp."))
                        && unset,
                    "{} is not a key of the default config",
                    def.path
                );
                continue;
            };
            assert_eq!(
                actual, &default,
                "{} default differs from the code's default",
                def.path
            );
            if let Some(key) = def.path.strip_prefix("ui.") {
                assert!(ui.contains_key(key), "{key} missing from UiConfig");
            }
        }
    }

    #[test]
    fn no_duplicates_and_every_tab_and_section_has_entries() {
        let mut seen = HashSet::new();
        for def in registry() {
            assert!(seen.insert(def.path), "duplicate {}", def.path);
            assert!(!def.label.is_empty() && !def.description.is_empty());
        }
        let tabs = tabs();
        assert!(tabs.len() >= 5);
        for (tab, sections) in &tabs {
            assert!(!sections.is_empty());
            for section in sections {
                assert!(
                    registry()
                        .iter()
                        .any(|d| d.tab == *tab && d.section == *section),
                    "{tab}/{section} is empty"
                );
            }
        }
        assert_eq!(
            tabs.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            [
                "Appearance",
                "Interaction",
                "Model",
                "Memory",
                "Improvement",
                "Tools",
                "Experimental",
                "Voice"
            ]
        );
    }

    /// Sibling settings that `Config::validate` reads while `path` takes `value`. Each dependent is
    /// set to a value that satisfies the rule for every value in the key's own range, so the key is
    /// always validated against the real validator and no key is exempt.
    fn cross_field_dependents(path: &str, value: &Value) -> Vec<(&'static str, Value)> {
        match path {
            "memory.semantic.embedding" if *value == Value::String("on".into()) => vec![
                (
                    "memory.semantic.embedding_base_url",
                    Value::String("http://127.0.0.1:8888/v1".into()),
                ),
                (
                    "memory.semantic.embedding_model",
                    Value::String("fixture-embedding".into()),
                ),
            ],
            // The hindsight backend requires an endpoint (config.rs validate).
            "memory.backend" if *value == Value::String("hindsight".into()) => vec![(
                "memory.hindsight.endpoint",
                Value::String("http://127.0.0.1:8888".into()),
            )],
            // The timeout must not exceed either lease; the heartbeat sits strictly inside the
            // consolidation lease (1 <= heartbeat < lease).
            "memory.local.timeout_secs" => vec![
                ("memory.local.stage1_lease_secs", Value::Integer(86400)),
                (
                    "memory.local.consolidation_lease_secs",
                    Value::Integer(86400),
                ),
                (
                    "memory.local.consolidation_heartbeat_secs",
                    Value::Integer(30),
                ),
            ],
            "memory.local.stage1_lease_secs" => {
                vec![("memory.local.timeout_secs", Value::Integer(1))]
            }
            "memory.local.consolidation_lease_secs" => vec![
                ("memory.local.timeout_secs", Value::Integer(1)),
                (
                    "memory.local.consolidation_heartbeat_secs",
                    Value::Integer(1),
                ),
            ],
            "memory.local.consolidation_heartbeat_secs" => vec![
                ("memory.local.timeout_secs", Value::Integer(1)),
                (
                    "memory.local.consolidation_lease_secs",
                    Value::Integer(86400),
                ),
            ],
            // The decision lease must cover its model deadline and stay within a day.
            "memory.sharpshooter.lease_secs" => {
                vec![("memory.sharpshooter.timeout_secs", Value::Integer(1))]
            }
            "memory.sharpshooter.timeout_secs" => {
                vec![("memory.sharpshooter.lease_secs", Value::Integer(86400))]
            }
            // The net-win threshold may not exceed the primary case count; 1000 is its maximum.
            "improve.min_net_wins" => vec![("improve.gate_primary_cases", Value::Integer(1000))],
            _ => Vec::new(),
        }
    }

    #[test]
    fn every_enum_value_deserializes_and_ranges_are_accepted_by_the_config() {
        let mut base = default_table();
        // Memory backends that call a model require the memory role (config validate).
        set(
            &mut base,
            "model_roles.memory.provider",
            Value::String("mlx".into()),
        );
        set(
            &mut base,
            "model_roles.memory.model",
            Value::String("m".into()),
        );
        for def in registry() {
            let mut values: Vec<Value> = Vec::new();
            match def.kind {
                Kind::Enum(options) => {
                    assert!(
                        options
                            .iter()
                            .any(|(v, _)| Some(Value::String((*v).into())) == def.default_value()),
                        "{} default is not an option",
                        def.path
                    );
                    values.extend(options.iter().map(|(v, _)| Value::String((*v).into())));
                }
                Kind::Choice(options) => {
                    // Suggestions are not a closed set: the default may be blank and any text is valid.
                    values.extend(options.iter().map(|(v, _)| Value::String((*v).into())));
                    values.extend([
                        Value::String(String::new()),
                        Value::String("operator/custom-model".into()),
                    ]);
                }
                Kind::AutoBool => {
                    values.extend([
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::String("auto".into()),
                    ]);
                }
                Kind::Int { min, max, step } | Kind::AutoInt { min, max, step } => {
                    assert!(
                        min < max && step > 0 && step <= max - min,
                        "{} bad int bounds",
                        def.path
                    );
                    values.extend([Value::Integer(min), Value::Integer(max)]);
                    if matches!(def.kind, Kind::AutoInt { .. }) {
                        values.push(Value::String("auto".into()));
                    }
                }
                Kind::Float { min, max, step } => {
                    assert!(min < max && step > 0.0, "{} bad float bounds", def.path);
                    values.extend([Value::Float(min), Value::Float(max)]);
                }
                _ => {}
            }
            for value in values {
                let mut table = base.clone();
                set(&mut table, def.path, value.clone());
                for (dependent, dependent_value) in cross_field_dependents(def.path, &value) {
                    set(&mut table, dependent, dependent_value);
                }
                let config: Config = Value::Table(table)
                    .try_into()
                    .unwrap_or_else(|e| panic!("{} = {value} rejected: {e}", def.path));
                config
                    .validate()
                    .unwrap_or_else(|e| panic!("{} = {value}: {e}", def.path));
            }
        }
    }

    #[test]
    fn service_tier_options_match_the_config_tiers() {
        let options = registry()
            .iter()
            .find(|setting| setting.path == "service_tier")
            .and_then(|setting| match setting.kind {
                Kind::Enum(options) => Some(options),
                _ => None,
            })
            .expect("service_tier is an enum setting");
        let values: Vec<&str> = options.iter().map(|(value, _)| *value).collect();
        assert_eq!(values, crate::config::SERVICE_TIERS);
    }

    #[test]
    fn numeric_defaults_lie_inside_their_ranges() {
        for def in registry() {
            match (def.kind, def.default_value()) {
                (Kind::Int { min, max, .. }, Some(Value::Integer(n))) => {
                    assert!((min..=max).contains(&n), "{}", def.path)
                }
                (Kind::Float { min, max, .. }, Some(Value::Float(n))) => {
                    assert!(n >= min && n <= max, "{}", def.path)
                }
                (Kind::Int { .. } | Kind::Float { .. }, other) => {
                    panic!("{} default {other:?} has the wrong type", def.path)
                }
                _ => {}
            }
        }
    }
    #[test]
    fn experimental_options_are_registered_in_their_tab_and_default_off() {
        let paths = [
            "experimental.work_packet_contracts",
            "experimental.verification_receipts",
            "experimental.context_overlap_dedup",
            "experimental.profile_matching",
        ];
        for path in paths {
            let setting = registry()
                .iter()
                .find(|setting| setting.path == path)
                .unwrap();
            assert_eq!(setting.tab, "Experimental");
            assert_eq!(setting.kind, Kind::Bool);
            assert_eq!(setting.default_value(), Some(Value::Boolean(false)));
        }
        let config: Config =
            toml::from_str("[experimental]\ncontext_overlap_dedup = true").unwrap();
        assert!(config.experimental.context_overlap_dedup);
        assert!(!config.experimental.verification_receipts);
    }

    #[test]
    fn dotted_get_set_unset() {
        let mut t = Table::new();
        set(&mut t, "a.b.c", Value::Integer(1));
        assert_eq!(get(&t, "a.b.c"), Some(&Value::Integer(1)));
        assert_eq!(get(&t, "a.b.d"), None);
        assert_eq!(get(&t, "a.b.c.d"), None);
        set(&mut t, "a.b", Value::Boolean(true));
        assert_eq!(get(&t, "a.b"), Some(&Value::Boolean(true)));
        assert!(unset(&mut t, "a.b"));
        assert!(t.is_empty(), "empty parents are pruned");
        assert!(!unset(&mut t, "a.b"));
    }
}
