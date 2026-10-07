use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn deserialize_context_window<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ContextWindow {
        Tokens(u32),
        Auto(String),
    }

    match ContextWindow::deserialize(deserializer)? {
        ContextWindow::Tokens(tokens) => Ok(tokens),
        ContextWindow::Auto(value) if value.eq_ignore_ascii_case("auto") => Ok(32768),
        ContextWindow::Auto(value) => Err(serde::de::Error::custom(format!(
            "expected an integer or \"auto\", found {value:?}"
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Harness {
    /// The improvable part of the system prompt; dial's fixed tool guidance is added around it.
    pub system_prompt: String,
    pub memory: Vec<String>,
    /// Enabled tools: built-ins, dial's extra tools, and operator tools from `[tools.*]`.
    pub tools: Vec<String>,
    pub subagents: Vec<String>,
    pub max_steps: u32,
    pub temperature: f64,
    pub max_tokens: u32,
    /// `None` follows the configured `edit_mode`; the improvement loop may pin one per model.
    pub edit_mode: Option<EditModeConfig>,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            system_prompt: "Work in small verified steps. Read before you edit, run the project's checks after changing code, and say plainly when you are unsure.".into(),
            memory: vec![],
            tools: crate::tools::default_tools(),
            subagents: vec![],
            max_steps: 40,
            temperature: 0.2,
            max_tokens: 8192,
            edit_mode: None,
        }
    }
}

/// One-way migration of the removed monolithic memory tool selection.
pub fn migrate_memory_tools(harness: &mut Harness) -> bool {
    migrate_memory_tool_names(&mut harness.tools)
}

pub(crate) fn migrate_memory_tool_names(tools: &mut Vec<String>) -> bool {
    const REPLACEMENTS: [&str; 6] = [
        "recall",
        "retain",
        "memory_update",
        "forget",
        "memory_history",
        "memory_import",
    ];
    if !tools.iter().any(|tool| tool == "memory") {
        return false;
    }
    let present = REPLACEMENTS.map(|replacement| tools.iter().any(|tool| tool == replacement));
    let mut added = [false; 6];
    let original = std::mem::take(tools);
    tools.reserve(original.len().saturating_add(REPLACEMENTS.len()));
    for tool in original {
        if tool == "memory" {
            for (index, replacement) in REPLACEMENTS.iter().enumerate() {
                if !present[index] && !added[index] {
                    tools.push((*replacement).into());
                    added[index] = true;
                }
            }
        } else {
            tools.push(tool);
        }
    }
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    pub suites: Vec<SuiteConfig>,
    /// Percentage points on a 0..100 scale, per suite.
    pub max_regression: f64,
    pub min_improvement: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrainingConfig {
    pub enabled: bool,
    /// Defaults to `<state>/grounded/train.jsonl` when empty.
    pub train_data: PathBuf,
    pub valid_data: PathBuf,
    pub iters: u32,
    pub batch_size: u32,
    pub num_layers: u32,
    pub rank: u32,
    pub max_seq_length: u32,
    pub interval_secs: u64,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            train_data: PathBuf::new(),
            valid_data: PathBuf::new(),
            iters: 32,
            batch_size: 1,
            num_layers: 4,
            rank: 8,
            max_seq_length: 8192,
            interval_secs: 6 * 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

/// Use an OpenAI-compatible server (LM Studio, Ollama, llama.cpp, a hosted API) instead of the
/// supervised local MLX model. Weight updates are unavailable for endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub base_url: String,
    /// Name of the environment variable holding the bearer token (the token itself is never stored).
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub allow_insecure: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    /// Prompt before writes, edits and shell commands.
    #[default]
    Ask,
    Auto,
    /// Read-only tools only.
    Readonly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    Off,
    /// Shell can read most of the system but not secrets or dial state, and write only the project.
    #[default]
    Dev,
    /// Workspace-only reads and writes, no network.
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// Dev mode only; strict mode never has network.
    pub allow_network: bool,
    /// Extra writable directories in dev mode (for example `~/.cargo`, `~/.npm`).
    pub extra_write: Vec<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Dev,
            allow_network: true,
            extra_write: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EditModeConfig {
    /// Anchored line edits (`[path#TAG]` + `PUT`/`CUT` operations). Strong models handle it; local
    /// models were observed deleting neighbouring lines with off-by-one ranges.
    Hashline,
    /// Exact text replacement (`old` -> `new`); the default because every model handles it.
    #[default]
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip)]
    pub state_dir: PathBuf,
    #[serde(skip)]
    pub project_root: PathBuf,
    #[serde(skip)]
    pub user_dir: PathBuf,
    /// Python with mlx-lm for local models; defaults to the environment `dial setup` creates.
    pub python: PathBuf,
    pub worker: PathBuf,
    pub model: String,
    /// Explicit "mlx" clears any inherited endpoint when a project selects a local model.
    pub provider: Option<String>,
    pub endpoint: Option<EndpointConfig>,
    pub providers: BTreeMap<String, crate::llm::model_config::ProviderSpec>,
    pub model_roles: BTreeMap<String, crate::llm::model_config::ModelSelection>,
    /// Exact provider/model profiles; capabilities must be declared by the operator.
    pub model_profiles: BTreeMap<String, crate::llm::profiles::ModelProfile>,
    pub model_profile: Option<String>,
    pub ui: crate::llm::model_config::UiConfig,
    /// Defaults to the project directory name.
    pub use_case: String,
    pub worker_timeout_secs: u64,
    /// Silence limit for remote model responses, independent of long local workers.
    pub remote_model_idle_timeout_secs: u64,
    pub reasoning_guard: ReasoningGuardConfig,
    pub codebase: CodebaseConfig,
    pub assistance: AssistanceConfig,
    pub steering: crate::steering::Config,
    pub adaptive: crate::adaptive::Config,
    pub reliability: crate::reliability::Config,
    pub laya: crate::agent::laya::Config,
    /// Configured context budget; `auto` uses the standard budget, then honors reported server caps.
    #[serde(deserialize_with = "deserialize_context_window")]
    pub context_window: u32,
    pub harness: Harness,
    pub gate: Option<GateConfig>,
    pub training: TrainingConfig,
    pub improve: ImproveConfig,
    pub approval: Approval,
    pub sandbox: SandboxConfig,
    pub edit_mode: EditModeConfig,
    pub tools: BTreeMap<String, ToolConfig>,
    pub subagents: BTreeMap<String, String>,
    /// Integration tools to offer (`bg`, `web_fetch`, `web_search`, `lsp`, `ast_search`, `ast_edit`).
    /// Every tool schema costs the model context and attention; trim this list for small models.
    /// MCP tools follow `[mcp.*]` instead.
    pub integrations: Vec<String>,
    /// MCP servers whose tools are offered as `mcp__<server>__<tool>`. Trusted operator programs.
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Language servers overriding or extending the built-in defaults, keyed by language name.
    pub lsp: BTreeMap<String, LspServerConfig>,
    /// Debug Adapter Protocol adapters keyed by adapter id.
    pub dap: BTreeMap<String, crate::tools::dap::DapAdapterConfig>,
    pub web: WebConfig,
    pub verification: VerificationConfig,
    /// Bounded isolated patch search. Requires explicit paths and executable checks.
    pub solve: crate::agent::solve::SolveConfig,
    /// Narrow stronger-model advice; remote providers require explicit operator opt-in.
    pub escalation: crate::agent::solve::EscalationConfig,
    pub browser: crate::tools::browser::BrowserConfig,
    pub kernel: crate::tools::kernel::KernelConfig,
    pub memory: MemoryConfig,
    /// Operator-owned lifecycle hooks (`[[hooks]]`); see `agent::hooks`.
    pub hooks: Vec<HookConfig>,
    pub budget: BudgetConfig,
    /// Named read-only subagents (`[agents.<name>]`) with their own instructions, tool subset and step budget.
    pub agents: BTreeMap<String, AgentConfig>,
}

/// A user-defined subagent built on a read-only built-in type. Writers stay built in because
/// launching one needs approval, and that decision is made without the configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// `scout`, `reviewer` or `planner`.
    #[serde(default = "default_agent_base")]
    pub base: String,
    /// One line shown to the model in the `task` description.
    #[serde(default)]
    pub description: String,
    /// Instructions appended to the base agent's prompt.
    pub prompt: String,
    /// Restrict the base agent's tools to these (never adds any).
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub max_steps: Option<u32>,
}

fn default_agent_base() -> String {
    "scout".into()
}

/// Spend caps in USD, enforced between model requests. They need catalog pricing for the model and
/// reported token usage: a model with no known price is never stopped by them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Stop the turn once the session has cost this much.
    pub max_session_cost_usd: Option<f64>,
    /// Stop the turn once it alone has cost this much.
    pub max_turn_cost_usd: Option<f64>,
}

/// A program run at a lifecycle event. Trusted and unsandboxed, like MCP servers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// `user_prompt_submit`, `pre_tool_use`, `post_tool_use` or `stop`.
    pub event: String,
    /// Anchored regex on the tool name (tool events only); omitted matches every tool.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Program and arguments; the event payload arrives as JSON on stdin.
    pub command: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

fn default_hook_timeout() -> u64 {
    30
}

/// An MCP server: a stdio program or a streamable-HTTP endpoint. Like `[tools.*]` it is a trusted program: it runs unsandboxed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stdio server executable. Exactly one of `program` and `url` is set.
    #[serde(default)]
    pub program: PathBuf,
    /// Streamable-HTTP endpoint (https, or http on localhost).
    #[serde(default)]
    pub url: Option<String>,
    /// Name of an environment variable holding a bearer token for `url` servers.
    #[serde(default)]
    pub bearer_env: Option<String>,
    /// Extra non-secret request headers for `url` servers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Per request timeout.
    #[serde(default = "default_mcp_timeout")]
    pub timeout_secs: u64,
    /// Lifecycle events to forward to this server as `notifications/dial/event`.
    #[serde(default)]
    pub lifecycle_hooks: Vec<String>,
}

fn default_mcp_timeout() -> u64 {
    60
}

/// A stdio language server used by the `lsp` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions (without the dot) this server handles.
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Search endpoint override; empty uses the built-in default provider.
    pub search_url: String,
    /// Environment variable holding the search endpoint's API key, when it needs one.
    pub search_api_key_env: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationConfig {
    /// Run focused static checks after edited source files, then allow a bounded model repair turn.
    pub enabled: bool,
    pub repair_attempts: u8,
    pub commands: Vec<VerificationCommand>,
    /// Names of configured commands that check behavioral correctness, not only syntax or buildability.
    /// Explicit operator declaration; only failing-before/passing-after runs qualify as repair evidence.
    pub behavioral_checks: Vec<String>,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repair_attempts: 1,
            commands: Vec::new(),
            behavioral_checks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCommand {
    pub name: String,
    pub command: String,
    /// File extensions without a dot that trigger this command; empty means any source edit.
    #[serde(default)]
    pub file_extensions: Vec<String>,
    #[serde(default = "default_verification_timeout")]
    pub timeout_secs: u64,
}

fn default_verification_timeout() -> u64 {
    120
}

/// Operator-owned limits; harness improvement cannot increase them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningGuardConfig {
    pub enabled: bool,
    pub max_pre_action_secs: u64,
    /// Bytes of streamed reasoning, not an estimate of hidden reasoning tokens.
    pub max_reasoning_bytes: usize,
}

impl Default for ReasoningGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_pre_action_secs: 60,
            max_reasoning_bytes: 16384,
        }
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
}
impl Default for AssistanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            syntax_guard: true,
            selective_verification: true,
            compact_tools: true,
            initial_tool_limit: 16,
            context_max_bytes: 4096,
            teacher_enabled: false,
            teacher_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodebaseConfig {
    pub enabled: bool,
    pub context_max_bytes: usize,
    /// Build a bounded architecture synthesis after successful interactive turns.
    pub synthesize_architecture: bool,
    /// Local MLX model for architecture summaries; blank follows main, explicit roles override.
    pub architecture_model: String,
}
impl Default for CodebaseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_max_bytes: 8000,
            synthesize_architecture: true,
            architecture_model: "mlx-community/Qwen2.5-3B-Instruct-4bit".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub context_limit: usize,
    pub context_max_bytes: usize,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_limit: 8,
            context_max_bytes: 6000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::new(),
            project_root: PathBuf::new(),
            user_dir: PathBuf::new(),
            python: PathBuf::new(),
            worker: PathBuf::new(),
            model: String::new(),
            provider: None,
            endpoint: None,
            use_case: String::new(),
            providers: BTreeMap::new(),
            model_roles: BTreeMap::new(),
            ui: crate::llm::model_config::UiConfig::default(),
            model_profiles: BTreeMap::new(),
            model_profile: None,
            worker_timeout_secs: 3600,
            context_window: 32768,
            remote_model_idle_timeout_secs: 180,
            reasoning_guard: ReasoningGuardConfig::default(),
            codebase: CodebaseConfig::default(),
            assistance: AssistanceConfig::default(),
            steering: crate::steering::Config::default(),
            adaptive: crate::adaptive::Config::default(),
            reliability: crate::reliability::Config::default(),
            laya: Default::default(),
            harness: Harness::default(),
            gate: None,
            training: TrainingConfig::default(),
            improve: ImproveConfig::default(),
            approval: Approval::default(),
            sandbox: SandboxConfig::default(),
            edit_mode: EditModeConfig::default(),
            tools: BTreeMap::new(),
            subagents: BTreeMap::new(),
            integrations: vec![
                "bg".into(),
                "web_fetch".into(),
                "web_search".into(),
                "lsp".into(),
                "ast_search".into(),
                "ast_edit".into(),
                "dap".into(),
                "git".into(),
            ],
            mcp: BTreeMap::new(),
            lsp: BTreeMap::new(),
            dap: BTreeMap::new(),
            web: WebConfig::default(),
            verification: VerificationConfig::default(),
            solve: Default::default(),
            escalation: Default::default(),
            browser: Default::default(),
            kernel: Default::default(),
            memory: Default::default(),
            hooks: Vec::new(),
            budget: BudgetConfig::default(),
            agents: BTreeMap::new(),
        }
    }
}

/// Automatic self-improvement. On by default: dial records turns, runs verified self-play on
/// procedurally generated tasks, and proposes harness and LoRA changes that only a sealed gate can admit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImproveConfig {
    pub enabled: bool,
    /// Minimum seconds between background cycles.
    pub interval_secs: u64,
    /// A cycle needs at least this many new recorded turns (or tool-call repairs) since the last one.
    pub min_new_turns: u32,
    /// Verified self-play tasks per cycle (training data and weakness signal).
    pub selfplay_tasks: u32,
    pub gate_primary_cases: u32,
    pub gate_guard_cases: u32,
    /// One-sided exact sign-test level for the primary suite; admission also needs `min_net_wins`.
    pub alpha: f64,
    pub min_net_wins: u32,
    /// Candidates evaluated per cycle (harness and weight changes compete).
    pub max_candidates: u32,
    /// Percentage points a version may lose against the very first version on the frozen anchor suite.
    pub drift_tolerance: f64,
    /// Learn from live turns whose outcome was verified (your /outcome, passing checks, no /undo).
    pub learn_from_sessions: bool,
    /// Minimum verified training examples before a LoRA candidate is attempted.
    pub lora_min_examples: u32,
    /// Roll back automatically when a new version's live results fall this many points below its parent's.
    pub monitor_drop: f64,
    pub monitor_min_turns: u32,
    /// Run the background cycle at session start/exit. When false, only `dial improve cycle` runs it.
    pub background: bool,
    /// Start a background cycle only after this many minutes without a coding turn (0 = any time).
    pub idle_minutes: u32,
    /// Do not start background cycles on battery power.
    pub require_ac_power: bool,
    /// Background compute allowed per day, in minutes, enforced during a cycle too (0 = unlimited).
    pub daily_compute_minutes: u32,
}

impl Default for ImproveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 6 * 3600,
            min_new_turns: 5,
            selfplay_tasks: 24,
            gate_primary_cases: 30,
            gate_guard_cases: 20,
            alpha: 0.1,
            min_net_wins: 2,
            max_candidates: 3,
            drift_tolerance: 3.0,
            learn_from_sessions: true,
            lora_min_examples: 24,
            monitor_drop: 15.0,
            monitor_min_turns: 8,
            background: true,
            idle_minutes: 10,
            require_ac_power: true,
            daily_compute_minutes: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: i64,
    pub parent: Option<i64>,
    pub kind: String,
    pub harness: Harness,
    pub adapter: Option<PathBuf>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub trajectory: i64,
    pub version: i64,
    pub domain: String,
    pub task: String,
    pub step: u32,
    pub kind: String,
    pub data: serde_json::Value,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub trajectory: i64,
    /// Externally assigned score on a 0..100 scale.
    pub score: f64,
    pub source: String,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteScore {
    pub name: String,
    pub primary: bool,
    pub baseline: f64,
    pub candidate: f64,
    pub cases: usize,
    /// Paired outcomes: cases only the candidate passed / only the baseline passed.
    #[serde(default)]
    pub candidate_only: usize,
    #[serde(default)]
    pub baseline_only: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admission {
    pub candidate: i64,
    pub baseline: i64,
    pub passed: bool,
    pub reason: String,
    pub scores: Vec<SuiteScore>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub diagnosis: String,
    /// "harness" or "lora". Other operations must be rejected.
    pub operation: String,
    pub harness: Option<Harness>,
}
