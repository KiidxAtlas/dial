//! Model assistance and context economy: portable rules, the lean and local-model profiles,
//! efficiency budgets and saved codebase context.
use super::*;

/// Non-executing scoped guidance and bounded inference-only rule interception.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortableRulesConfig {
    pub enabled: bool,
    pub max_regenerations: u32,
    /// User turns after which a fired stream rule may fire again; 0 keeps it quiet for the session.
    pub repeat_after_turns: u32,
    /// When judged `question` rules are asked of the `judge` role.
    pub judge: RuleJudge,
}
impl Default for PortableRulesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_regenerations: 8,
            repeat_after_turns: 0,
            judge: RuleJudge::Auto,
        }
    }
}

/// `portable_rules.judge`. `auto` judges `question` rules only when the `judge` role is explicitly
/// assigned, so no main-model calls are spent silently; `on` uses the judge role with its `small`
/// and main-model fallbacks; `off` never judges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleJudge {
    #[default]
    Auto,
    On,
    Off,
}

/// `assistance.lean_local`. TOML `true` (the default) turns the lean profile on for every local route,
/// whatever its window: a large window does not make a small model able to use the full prompt and
/// tool offer. `"auto"` enables it only for planned windows of at most [`LEAN_AUTO_MAX_WINDOW`], and
/// `false` opts out, so boolean configurations keep their meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeanLocal {
    Auto,
    #[default]
    On,
    Off,
}

/// Largest planned context window at which `assistance.lean_local = "auto"` enables the lean profile.
pub const LEAN_AUTO_MAX_WINDOW: u32 = 32_768;

impl From<bool> for LeanLocal {
    fn from(forced: bool) -> Self {
        if forced { Self::On } else { Self::Off }
    }
}

impl Serialize for LeanLocal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::On => serializer.serialize_bool(true),
            Self::Off => serializer.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for LeanLocal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Word(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Flag(forced) => Ok(forced.into()),
            Raw::Word(word) if word == "auto" => Ok(Self::Auto),
            Raw::Word(word) => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&word),
                &"true, false or \"auto\"",
            )),
        }
    }
}

/// `assistance.local.edit_mode`: the edit grammar for local routes when nothing pins one. `inherit`
/// follows `[edit] mode`; any edit mode name selects that grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalEditMode(pub Option<EditMode>);

impl Default for LocalEditMode {
    fn default() -> Self {
        Self(Some(EditMode::Replace))
    }
}

impl Serialize for LocalEditMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(mode) => mode.serialize(serializer),
            None => serializer.serialize_str("inherit"),
        }
    }
}

impl<'de> Deserialize<'de> for LocalEditMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        if word == "inherit" {
            return Ok(Self(None));
        }
        EditMode::deserialize(serde::de::value::StrDeserializer::<D::Error>::new(&word))
            .map(|mode| Self(Some(mode)))
            .map_err(|_| {
                serde::de::Error::invalid_value(
                    serde::de::Unexpected::Str(&word),
                    &"\"inherit\" or an edit mode (hashline, replace, apply_patch, patch, sloppy)",
                )
            })
    }
}

/// `assistance.local.sampling`: who chooses sampling on local routes when no temperature is pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalSamplingSource {
    /// The model's shipped settings: read from a local model's files, or left to the local server's
    /// own per-model defaults (Dial omits `temperature`).
    #[default]
    Model,
    /// Dial's harness temperature, as on hosted routes.
    Harness,
}

/// `assistance.local.thinking`: who decides whether a local thinking model reasons on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalThinking {
    /// Reasoning on for planning and recovery (a turn's first request, after a failed round or an
    /// intercepted generation, after a plan change) and off for mechanical steps that follow a
    /// successful tool round. Only models whose server exposes a thinking switch are affected.
    Phase,
    /// Leave it to the model, the server and the model profile.
    #[default]
    Model,
}

/// `assistance.local.side_models`: side requests (compaction, summaries, retrieval ranking, edit
/// repair) routed to a different model on the main model's local server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalSideModels {
    /// Use the configured role model, and say once per session that it swaps models on the server.
    #[default]
    Warn,
    /// Send side requests to the loaded main model instead (with thinking off where switchable).
    ReuseMain,
    /// Use the configured role model without a notice.
    Allow,
}

/// Default `assistance.local.small_window`: planned windows at or below it get the small tier.
pub const LOCAL_SMALL_WINDOW: u32 = 16_384;

/// Longest `assistance.local.keep_loaded_secs`: one day.
pub const LOCAL_KEEP_LOADED_MAX_SECS: u32 = 86_400;

/// `[assistance.local]`: the local-model profile. Every policy here applies only to locally served
/// main models (`Config::local_main_model`); hosted routes and hermetic runs are unchanged, and explicit
/// pins (edit-mode override, model profile, harness, `[edit] model_variants`, a pinned temperature) win.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LocalAssistance {
    /// Master switch for the policies below. `assistance.lean_local` still owns the lean prompt.
    pub enabled: bool,
    /// Planned window (tokens) at or below which a lean local route also gets the small tier: a fixed
    /// minimal tool set up front (others load on demand). 0 disables the small tier.
    pub small_window: u32,
    /// Edit grammar for local routes when nothing pins one; `inherit` follows `[edit] mode`.
    pub edit_mode: LocalEditMode,
    /// `model` uses the model's shipped sampling; `harness` sends Dial's harness temperature.
    pub sampling: LocalSamplingSource,
    /// Percent of the planned window that streamed reasoning may use per request on lean tiers, when
    /// `reasoning_guard.max_reasoning_bytes` is unlimited or larger. 0 disables the cap.
    pub reasoning_share: u32,
    /// Consecutive failing rounds (malformed tool calls or failed edits) before the session escalates
    /// one step: first the minimal tool set and lean prompt, then the replace edit grammar. The
    /// reached step is remembered per model. 0 disables escalation.
    pub escalate_after: u32,
    /// Thinking per request phase (`phase`) or left to the model (`model`).
    pub thinking: LocalThinking,
    /// Ask the local server to keep the model loaded this long after each request (Ollama
    /// `keep_alive`, LM Studio `ttl`), and unload a model Dial loaded itself at session end. An
    /// explicit model-profile `keep_alive` wins. 0 leaves the server default.
    pub keep_loaded_secs: u32,
    /// llama.cpp: reuse the server's prompt cache (`cache_prompt`) and keep the main session on one
    /// slot (`id_slot`) with workers on others, so they do not evict each other's cache.
    pub prompt_cache: bool,
    /// Side requests to another model on the same local server: `warn`, `reuse_main` or `allow`.
    pub side_models: LocalSideModels,
    /// One actionable notice when the server serves a smaller window than configured, and an Ollama
    /// window bounded by the model's memory estimate when no window is pinned.
    pub context_advice: bool,
    /// Retry a generation the repetition detector stopped with a presence/repetition penalty.
    pub loop_penalty: bool,
    /// Record per-model evidence in `local-profile.json` and offer evidence-backed setting suggestions
    /// (including a speculative-decoding draft model) in `/model` settings. Never applied silently.
    pub suggestions: bool,
    /// Derive the pre-action time limit from the model's measured speed when
    /// `reasoning_guard.max_pre_action_secs` is 0, and show time estimates in the status line.
    pub time_budget: bool,
}

impl Default for LocalAssistance {
    fn default() -> Self {
        Self {
            enabled: true,
            small_window: LOCAL_SMALL_WINDOW,
            edit_mode: LocalEditMode::default(),
            sampling: LocalSamplingSource::Model,
            reasoning_share: 50,
            escalate_after: 2,
            thinking: LocalThinking::Model,
            keep_loaded_secs: 1800,
            prompt_cache: true,
            side_models: LocalSideModels::Warn,
            context_advice: true,
            loop_penalty: true,
            suggestions: true,
            time_budget: true,
        }
    }
}

impl LocalAssistance {
    pub fn validate(&self) -> Result<(), String> {
        if self.small_window != 0 && !(1024..=1_048_576).contains(&self.small_window) {
            return Err("assistance.local.small_window must be 0 or 1024..=1048576".into());
        }
        if self.reasoning_share > 90 {
            return Err("assistance.local.reasoning_share must be 0..=90".into());
        }
        if self.escalate_after > 10 {
            return Err("assistance.local.escalate_after must be 0..=10".into());
        }
        if self.keep_loaded_secs > LOCAL_KEEP_LOADED_MAX_SECS {
            return Err(format!(
                "assistance.local.keep_loaded_secs must be 0..={LOCAL_KEEP_LOADED_MAX_SECS}"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssistanceConfig {
    pub enabled: bool,
    pub syntax_guard: bool,
    pub selective_verification: bool,
    pub compact_tools: bool,
    /// Core and explicitly loaded tools are never hidden by this budget.
    pub initial_tool_limit: usize,
    pub context_max_bytes: usize,
    /// Requires an explicitly assigned experience model role; no implicit downloads or calls.
    pub teacher_enabled: bool,
    pub teacher_timeout_secs: u64,
    /// Constrain local MLX tool calls to the advertised tool schemas (supported tool-parser formats only).
    pub tool_grammar: bool,
    /// Offer verified repair hints and similar verified examples from this project.
    pub repair_hints: bool,
    /// After a malformed local tool call, retry once with output constrained to the offered tool schemas
    /// (LM Studio json_schema, Ollama format). Converted calls still pass runtime validation and approvals.
    pub constrained_repair: bool,
    /// Lean profile for locally served models (no endpoint, or a loopback/LAN host): compact tool
    /// interface, an initial tool budget of at most 8, optional prompt sections and the delegation
    /// guidance omitted when the local server runs one request at a time. `true` (default) enables it
    /// for every local route; `auto` only when the planned window is at most `LEAN_AUTO_MAX_WINDOW`;
    /// `false` opts out. Hosted routes are unchanged. Measure with `dial benchmark --ablate-assistance`.
    pub lean_local: LeanLocal,
    /// The local-model profile: tiers, edit grammar, sampling, reasoning cap and escalation.
    pub local: LocalAssistance,
}
impl Default for AssistanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            syntax_guard: true,
            selective_verification: true,
            compact_tools: false,
            initial_tool_limit: 16,
            context_max_bytes: 0,
            teacher_enabled: false,
            teacher_timeout_secs: 0,
            tool_grammar: true,
            repair_hints: false,
            constrained_repair: true,
            lean_local: LeanLocal::On,
            local: LocalAssistance::default(),
        }
    }
}

/// Default `efficiency.history_token_cap` (2^18, a multiple of the settings step).
pub const DEFAULT_HISTORY_TOKEN_CAP: u32 = 262_144;

/// Shared context economy; does not change approvals, output/reasoning budgets or verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EfficiencyConfig {
    pub enabled: bool,
    pub ambient_context_bytes: usize,
    /// Bytes of a successful, artifact-retained inspection result the model sees inline; 0 keeps the
    /// whole result up to the per-model output limit. Errors and edits are never reduced.
    pub tool_preview_bytes: usize,
    /// Append a transient task evidence packet (active todo, findings, constraints) to every request
    /// while a task is in progress. Off keeps requests to the conversation plus a short task focus.
    pub task_packet: bool,
    pub lightweight_requests: u32,
    pub cache_pressure_percent: u32,
    /// Window (tokens) that compaction and pruning plan against when the model's window is larger;
    /// 0 plans against the full window. Every step resends the history; at the default the auto
    /// trigger fires near 223k instead of 850k on a 1M-token model. Smaller model windows are
    /// unaffected by the cap.
    pub history_token_cap: u32,
}
impl Default for EfficiencyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ambient_context_bytes: 8192,
            tool_preview_bytes: 0,
            task_packet: false,
            lightweight_requests: 4,
            cache_pressure_percent: 75,
            history_token_cap: DEFAULT_HISTORY_TOKEN_CAP,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodebaseConfig {
    pub enabled: bool,
    pub context_max_bytes: usize,
    /// Attach a bounded, task-relevant brief of fresh saved codebase context (architecture notes,
    /// findings, source outlines) to every user turn; no model request. Off leaves retrieval to the
    /// explicit `codebase_memory` tool.
    pub ambient_context: bool,
    /// Build a bounded architecture synthesis after successful interactive turns.
    pub synthesize_architecture: bool,
    /// Record each turn's file diffs (branch and prompt included) and offer `code_history`.
    pub history: bool,
    /// Local MLX model for architecture summaries; blank follows main, explicit roles override.
    pub architecture_model: String,
}
impl Default for CodebaseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_max_bytes: 0,
            ambient_context: true,
            synthesize_architecture: false,
            history: true,
            architecture_model: "mlx-community/Qwen2.5-3B-Instruct-4bit".into(),
        }
    }
}
