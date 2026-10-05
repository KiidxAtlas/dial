//! The settings the `/settings` panel can edit. Every entry names a real configuration key (a field of
//! `types::Config` or `[ui]`), carries the real default and the same range the configuration
//! validation enforces, so the panel can reject bad input before the controller does. The tests
//! cross-check each entry against `Config::default()`.
use toml::{Table, Value};

pub type EnumValues = &'static [(&'static str, &'static str)];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    /// `(stored value, label)` pairs, in cycling order.
    Enum(EnumValues),
    Int { min: i64, max: i64, step: i64 },
    Float { min: f64, max: f64, step: f64 },
    Text,
    StringList,
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
    /// TOML literal of the built-in default, used for display and "reset".
    pub default: &'static str,
    /// The change only takes effect for a new session.
    pub restart: bool,
}

impl SettingDef {
    /// The built-in default as a value (`None` if the literal is malformed, which a test forbids).
    pub fn default_value(&self) -> Option<Value> {
        let table: Table = toml::from_str(&format!("v = {}", self.default)).ok()?;
        table.get("v").cloned()
    }
}

const SYMBOLS: EnumValues = &[("auto", "Auto"), ("unicode", "Unicode"), ("nerd", "Nerd Font"), ("ascii", "ASCII")];
const STATUS: EnumValues = &[
    ("default", "Default"), ("minimal", "Minimal"), ("compact", "Compact"), ("full", "Full"), ("nerd", "Nerd"),
    ("ascii", "ASCII"), ("custom", "Custom"),
];
const SEPARATOR: EnumValues = &[
    ("powerline", "Powerline"), ("powerline-thin", "Powerline thin"), ("slash", "Slash"), ("pipe", "Pipe"),
    ("block", "Block"), ("none", "None"), ("ascii", "ASCII"),
];
const SHAPE: EnumValues = &[("box", "Box"), ("line", "Line")];
const APPROVAL: EnumValues = &[("ask", "Ask"), ("auto", "Auto"), ("readonly", "Read-only")];
const SANDBOX: EnumValues = &[("off", "Off"), ("dev", "Dev"), ("strict", "Strict")];
const EDIT_MODE: EnumValues = &[("hashline", "Hashline"), ("replace", "Replace")];

const AP: &str = "Appearance";
const MO: &str = "Model";
const IN: &str = "Interaction";
const TO: &str = "Tools";
const LE: &str = "Learning";

macro_rules! def {
    ($path:expr, $tab:expr, $section:expr, $label:expr, $desc:expr, $kind:expr, $default:expr) => {
        def!($path, $tab, $section, $label, $desc, $kind, $default, false)
    };
    ($path:expr, $tab:expr, $section:expr, $label:expr, $desc:expr, $kind:expr, $default:expr, $restart:expr) => {
        SettingDef { path: $path, tab: $tab, section: $section, label: $label, description: $desc, kind: $kind, default: $default, restart: $restart }
    };
}

static REGISTRY: &[SettingDef] = &[
    // Appearance
    def!("ui.theme", AP, "Theme", "Theme", "Color theme. `auto` follows the terminal background using the dark and light themes below.", Kind::Theme, "\"dark\""),
    def!("ui.theme_dark", AP, "Theme", "Dark theme", "Theme used by `auto` on a dark terminal.", Kind::Theme, "\"dark\""),
    def!("ui.theme_light", AP, "Theme", "Light theme", "Theme used by `auto` on a light terminal.", Kind::Theme, "\"light\""),
    def!("ui.color_blind", AP, "Theme", "Color-blind palette", "Show diff additions in blue instead of green.", Kind::Bool, "false"),
    def!("ui.symbols", AP, "Symbols", "Symbol set", "Glyphs for borders, icons and separators. Auto uses Nerd Font glyphs when the terminal advertises them.", Kind::Enum(SYMBOLS), "\"auto\""),
    def!("ui.composer_shape", AP, "Composer", "Composer shape", "Box: rounded frame with the status line in its border. Line: plain rules, easier to copy from.", Kind::Enum(SHAPE), "\"box\""),
    def!("ui.session_accent", AP, "Composer", "Session accent", "Tint the composer border with a color derived from the session id.", Kind::Bool, "true"),
    def!("ui.hints", AP, "Composer", "Key hints", "Show key hints in the composer's bottom border.", Kind::Bool, "true"),
    def!("ui.status", AP, "Status Line", "Preset", "Which segments the status line shows. Custom uses the left and right lists below.", Kind::Enum(STATUS), "\"default\""),
    def!("ui.status_separator", AP, "Status Line", "Separator", "Glyph between status line segments.", Kind::Enum(SEPARATOR), "\"powerline-thin\""),
    def!("ui.status_left", AP, "Status Line", "Left segments", "Segments for the custom preset: spinner, model, role, path, git, context, tokens, rate, approval, sandbox, learning, session, time.", Kind::StringList, "[\"spinner\", \"model\", \"path\", \"git\"]"),
    def!("ui.status_right", AP, "Status Line", "Right segments", "Segments right-aligned for the custom preset (same names as the left list).", Kind::StringList, "[\"context\"]"),
    def!("ui.status_path_abbreviate", AP, "Status Line", "Abbreviate path", "Show `~` for the home directory and only the last two path components.", Kind::Bool, "true"),
    def!("ui.status_git_counts", AP, "Status Line", "Git counts", "Show staged, modified and untracked counts next to the branch.", Kind::Bool, "true"),
    def!("ui.status_transparent", AP, "Status Line", "Transparent background", "Use the terminal background instead of a tinted status line.", Kind::Bool, "false"),
    def!("ui.welcome", AP, "Display", "Welcome banner", "Show the welcome banner when a session starts.", Kind::Bool, "true"),
    def!("ui.tips", AP, "Display", "Tips", "Show a usage tip under the welcome banner.", Kind::Bool, "true"),
    def!("ui.show_tools", AP, "Display", "Show tool activity", "Show tool calls in the transcript. Errors are always shown.", Kind::Bool, "true"),
    def!("ui.tool_output_lines", AP, "Display", "Tool output lines", "Lines of tool output shown before collapsing (Ctrl+O expands).", Kind::Int { min: 0, max: 1000, step: 1 }, "8"),
    def!("ui.show_turn_stats", AP, "Display", "Turn statistics", "Show duration, tokens and speed under each turn.", Kind::Bool, "true"),
    def!("ui.show_thinking", AP, "Display", "Show reasoning", "Show the model's reasoning text; off collapses it to a one-line summary.", Kind::Bool, "true"),
    // Model
    def!("context_window", MO, "Roles & Sampling", "Context window", "Tokens of context to plan against. A server that loaded a smaller window overrides this downward.", Kind::Int { min: 2048, max: 1_048_576, step: 1024 }, "32768"),
    def!("harness.temperature", MO, "Roles & Sampling", "Temperature", "Sampling temperature. The improvement loop may replace the admitted harness, including this value.", Kind::Float { min: 0.0, max: 2.0, step: 0.1 }, "0.2"),
    def!("harness.max_tokens", MO, "Roles & Sampling", "Max output tokens", "Output limit per model call, shared by reasoning and the answer. Reasoning-only limit hits use the reasoning gate's one recovery attempt; truncated answers or calls can retry with more tokens. The improvement loop may replace this harness value.", Kind::Int { min: 1, max: 8192, step: 256 }, "8192"),
    def!("harness.max_steps", MO, "Roles & Sampling", "Max steps per turn", "Tool-use steps allowed before a turn stops. The improvement loop may replace the admitted harness, including this value.", Kind::Int { min: 1, max: 64, step: 1 }, "40"),
    def!("worker_timeout_secs", MO, "Local worker", "Worker timeout (s)", "Deadline for local workers and local model silence after the first event. The reasoning gate separately bounds the wait before answer text or tool arguments begin. Remote model silence uses the remote idle limit.", Kind::Int { min: 1, max: 86400, step: 60 }, "3600", true),
    def!("remote_model_idle_timeout_secs", MO, "Roles & Sampling", "Remote idle timeout (s)", "Abandon a remote model request after this long without stream events. Keep-alive comments do not reset the deadline. The reasoning gate separately limits the wait before answer text or tool arguments begin.", Kind::Int { min: 1, max: 86400, step: 30 }, "180", true),
    def!("reasoning_guard.enabled", MO, "Roles & Sampling", "Reasoning gate", "Intercept long reasoning before answer text or tool arguments begin. Allow one action-focused recovery per turn, then stop incomplete. Harness improvement cannot raise these limits.", Kind::Bool, "true", true),
    def!("reasoning_guard.max_pre_action_secs", MO, "Roles & Sampling", "Before first output (s)", "Maximum wait before answer text or tool arguments begin, including hidden reasoning, connection setup, model startup and provider backoff. Reasoning events do not reset this deadline.", Kind::Int { min: 1, max: 86400, step: 10 }, "60", true),
    def!("reasoning_guard.max_reasoning_bytes", MO, "Roles & Sampling", "Before first output (bytes)", "Maximum streamed reasoning bytes before answer text or tool arguments begin. This measures text bytes, not hidden reasoning tokens; the time limit covers providers that do not expose reasoning.", Kind::Int { min: 256, max: 1048576, step: 1024 }, "16384", true),
    def!("codebase.enabled", MO, "Project understanding", "Saved codebase context", "Cache source outlines and reusable architectural findings per project across sessions. Refresh changed sources and retrieve only relevant bounded context each turn. Stored outside the project; disabled in sealed evaluations.", Kind::Bool, "true", true),
    def!("laya.enabled", MO, "Laya", "Decision advice", "Use Dial's independent typed-decision service for retrieval and post-edit evidence review. Advisory only; does not write summaries or certify tests. /laya status shows checkpoint and results.", Kind::Bool, "false", true),
    def!("laya.auto_start", MO, "Laya", "Start managed service", "Start Dial's installed service when its local port is stopped. Reuse ready/loading Dial services. The managed service shuts down after inactivity.", Kind::Bool, "true", true),
    def!("laya.endpoint", MO, "Laya", "Local service", "Dial Laya endpoint, including port. Default port 8002 is separate from other applications. Requires a Dial service identity.", Kind::Text, "\"http://127.0.0.1:8002\"", true),
    def!("laya.timeout_secs", MO, "Laya", "Request timeout (s)", "Bound each local service request. Failure leaves coding available and appears in Laya activity.", Kind::Int { min: 1, max: 10, step: 1 }, "3", true),
    def!("codebase.synthesize_architecture", MO, "Project understanding", "Architecture synthesis", "After successful turns, use the selected model to maintain cited architecture sections. One bounded call per source state; cached sections guide later edits. /understanding build retries manually.", Kind::Bool, "true", true),
    def!("codebase.context_max_bytes", MO, "Project understanding", "Context budget (bytes)", "Maximum saved project context added to a user turn. Includes the source map, relevant outlines and findings whose supporting files still match their saved hashes.", Kind::Int { min: 1024, max: 24000, step: 1024 }, "8000", true),
    // Interaction
    def!("ui.autocomplete_items", IN, "Input", "Menu rows", "Rows shown in the slash-command menu before it scrolls.", Kind::Int { min: 1, max: 50, step: 1 }, "8"),
    def!("ui.history_limit", IN, "Input", "History size", "Submitted prompts remembered for Up and Ctrl+R.", Kind::Int { min: 1, max: 10000, step: 100 }, "500"),
    def!("approval", IN, "Approvals", "Approval mode", "Ask: prompt before writes, edits and shell commands. Auto: never prompt. Read-only: only read tools run.", Kind::Enum(APPROVAL), "\"ask\""),
    // Tools
    def!("sandbox.mode", TO, "Sandbox", "Sandbox mode", "Off: no confinement. Dev: read most of the system, write only the project. Strict: workspace only, no network.", Kind::Enum(SANDBOX), "\"dev\"", true),
    def!("sandbox.allow_network", TO, "Sandbox", "Allow network", "Let shell commands use the network in dev mode. Strict mode never has network.", Kind::Bool, "true", true),
    def!("sandbox.extra_write", TO, "Sandbox", "Extra writable paths", "Additional directories shell commands may write to in dev mode, comma separated (e.g. ~/.cargo, ~/.npm).", Kind::StringList, "[]", true),
    def!("edit_mode", TO, "Editing", "Edit mode", "Replace: exact text replacement, works with every model. Hashline: anchored line edits, precise but easy to get wrong for small local models.", Kind::Enum(EDIT_MODE), "\"replace\""),
    // Learning: automatic improvement
    def!("improve.enabled", LE, "Automatic improvement", "Enabled", "Record turns and propose harness and LoRA changes that only the sealed gate can admit.", Kind::Bool, "true"),
    def!("improve.background", LE, "Automatic improvement", "Background cycles", "Run a cycle at session start and exit. Off: only `dial improve cycle` runs it.", Kind::Bool, "true"),
    def!("improve.learn_from_sessions", LE, "Automatic improvement", "Learn from sessions", "Learn from live turns whose outcome was verified (your /outcome, passing checks, no /undo).", Kind::Bool, "true"),
    def!("improve.interval_secs", LE, "Automatic improvement", "Cycle interval (s)", "Minimum seconds between background cycles.", Kind::Int { min: 1, max: 604_800, step: 3600 }, "21600"),
    def!("improve.min_new_turns", LE, "Automatic improvement", "New turns needed", "Recorded turns (or tool-call repairs) required since the last cycle.", Kind::Int { min: 1, max: 10000, step: 1 }, "5"),
    def!("improve.selfplay_tasks", LE, "Automatic improvement", "Self-play tasks", "Verified self-play tasks per cycle (training data and weakness signal).", Kind::Int { min: 0, max: 1000, step: 4 }, "24"),
    def!("improve.gate_primary_cases", LE, "Automatic improvement", "Gate primary cases", "Cases in the primary gate suite.", Kind::Int { min: 6, max: 1000, step: 2 }, "30"),
    def!("improve.gate_guard_cases", LE, "Automatic improvement", "Gate guard cases", "Cases in the regression guard suite.", Kind::Int { min: 4, max: 1000, step: 2 }, "20"),
    def!("improve.max_candidates", LE, "Automatic improvement", "Candidates per cycle", "Candidates evaluated per cycle; harness and weight changes compete.", Kind::Int { min: 1, max: 10, step: 1 }, "3"),
    def!("improve.alpha", LE, "Automatic improvement", "Significance level", "One-sided exact sign-test level for the primary suite, in (0, 1].", Kind::Float { min: 0.001, max: 1.0, step: 0.01 }, "0.1"),
    def!("improve.min_net_wins", LE, "Automatic improvement", "Minimum net wins", "Net wins on the primary suite needed in addition to significance. Must not exceed the primary case count.", Kind::Int { min: 0, max: 1000, step: 1 }, "2"),
    def!("improve.drift_tolerance", LE, "Automatic improvement", "Drift tolerance (pts)", "Percentage points a version may lose against the first version on the frozen anchor suite (0 to 3).", Kind::Float { min: 0.0, max: 3.0, step: 0.5 }, "3.0"),
    def!("improve.lora_min_examples", LE, "Automatic improvement", "LoRA minimum examples", "Verified training examples needed before a LoRA candidate is attempted.", Kind::Int { min: 1, max: 100_000, step: 4 }, "24"),
    def!("improve.monitor_drop", LE, "Automatic improvement", "Rollback drop (pts)", "Roll back when a version's live results fall this many points below its parent's.", Kind::Float { min: 1.0, max: 100.0, step: 1.0 }, "15.0"),
    def!("improve.monitor_min_turns", LE, "Automatic improvement", "Rollback min turns", "Live turns needed before the rollback check applies (at least 2).", Kind::Int { min: 2, max: 100_000, step: 1 }, "8"),
    // Learning: training
    def!("training.enabled", LE, "Training", "Enabled", "Allow LoRA fine-tuning of the local model.", Kind::Bool, "true"),
    def!("training.interval_secs", LE, "Training", "Interval (s)", "Minimum seconds between training runs.", Kind::Int { min: 1, max: 31_536_000, step: 3600 }, "21600"),
    def!("training.iters", LE, "Training", "Iterations", "Training iterations per run.", Kind::Int { min: 1, max: 10000, step: 10 }, "32"),
    def!("training.batch_size", LE, "Training", "Batch size", "Examples per optimizer step.", Kind::Int { min: 1, max: 16, step: 1 }, "1"),
    def!("training.num_layers", LE, "Training", "LoRA layers", "Transformer layers that receive LoRA adapters.", Kind::Int { min: 1, max: 256, step: 1 }, "4"),
    def!("training.rank", LE, "Training", "LoRA rank", "Rank of the LoRA adapters.", Kind::Int { min: 1, max: 128, step: 1 }, "8"),
    def!("training.max_seq_length", LE, "Training", "Max sequence length", "Longest training sequence in tokens.", Kind::Int { min: 32, max: 8192, step: 256 }, "8192"),
];

pub fn registry() -> &'static [SettingDef] { REGISTRY }

/// Tabs and their sections, in registry order. The panel adds a `Providers` tab of its own that
/// launches the provider and model screens; it holds no settings, so it is not listed here.
pub fn tabs() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut out: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for def in REGISTRY {
        match out.iter_mut().find(|(tab, _)| *tab == def.tab) {
            Some((_, sections)) => if !sections.contains(&def.section) { sections.push(def.section) },
            None => out.push((def.tab, vec![def.section])),
        }
    }
    out
}

/// Dotted lookup: `get(t, "ui.status")` is `t["ui"]["status"]`.
pub fn get<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts { value = value.as_table()?.get(part)?; }
    Some(value)
}

/// Set a dotted path, creating intermediate tables (replacing non-table values in the way).
pub fn set(table: &mut Table, path: &str, value: Value) {
    match path.split_once('.') {
        None => { table.insert(path.to_owned(), value); }
        Some((head, rest)) => {
            let entry = table.entry(head.to_owned()).or_insert_with(|| Value::Table(Table::new()));
            if !entry.is_table() { *entry = Value::Table(Table::new()); }
            if let Some(inner) = entry.as_table_mut() { set(inner, rest, value); }
        }
    }
}

/// Remove a dotted path; empty parent tables are dropped. Returns whether anything was removed.
pub fn unset(table: &mut Table, path: &str) -> bool {
    match path.split_once('.') {
        None => table.remove(path).is_some(),
        Some((head, rest)) => {
            let Some(inner) = table.get_mut(head).and_then(Value::as_table_mut) else { return false };
            let removed = unset(inner, rest);
            if inner.is_empty() { table.remove(head); }
            removed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{llm::model_config::UiConfig, config::types::Config};
    use std::collections::HashSet;

    fn default_table() -> Table { Table::try_from(&Config::default()).expect("default config serializes") }

    #[test]
    fn every_path_is_a_real_key_with_the_real_default() {
        let table = default_table();
        let ui = Table::try_from(&UiConfig::default()).unwrap();
        for def in registry() {
            let default = def.default_value().unwrap_or_else(|| panic!("{} default literal does not parse: {}", def.path, def.default));
            let actual = get(&table, def.path).unwrap_or_else(|| panic!("{} is not a key of the default config", def.path));
            assert_eq!(actual, &default, "{} default differs from the code's default", def.path);
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
                assert!(registry().iter().any(|d| d.tab == *tab && d.section == *section), "{tab}/{section} is empty");
            }
        }
        assert_eq!(tabs.iter().map(|(t, _)| *t).collect::<Vec<_>>(), ["Appearance", "Model", "Interaction", "Tools", "Learning"]);
    }

    #[test]
    fn every_enum_value_deserializes_and_ranges_are_accepted_by_the_config() {
        let base = default_table();
        for def in registry() {
            let mut values: Vec<Value> = Vec::new();
            match def.kind {
                Kind::Enum(options) => {
                    assert!(options.iter().any(|(v, _)| Some(Value::String((*v).into())) == def.default_value()), "{} default is not an option", def.path);
                    values.extend(options.iter().map(|(v, _)| Value::String((*v).into())));
                }
                Kind::Int { min, max, step } => {
                    assert!(min < max && step > 0 && step <= max - min, "{} bad int bounds", def.path);
                    values.extend([Value::Integer(min), Value::Integer(max)]);
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
                let config: Config = Value::Table(table).try_into().unwrap_or_else(|e| panic!("{} = {value} rejected: {e}", def.path));
                // `min_net_wins` is bounded by the primary case count, a cross-field rule the controller checks.
                if def.path != "improve.min_net_wins" {
                    config.validate().unwrap_or_else(|e| panic!("{} = {value}: {e}", def.path));
                }
            }
        }
    }

    #[test]
    fn numeric_defaults_lie_inside_their_ranges() {
        for def in registry() {
            match (def.kind, def.default_value()) {
                (Kind::Int { min, max, .. }, Some(Value::Integer(n))) => assert!((min..=max).contains(&n), "{}", def.path),
                (Kind::Float { min, max, .. }, Some(Value::Float(n))) => assert!(n >= min && n <= max, "{}", def.path),
                (Kind::Int { .. } | Kind::Float { .. }, other) => panic!("{} default {other:?} has the wrong type", def.path),
                _ => {}
            }
        }
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
