use crate::config;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Native edit payload contract. Workspace defaults remain hashline; config defaults preserve replace.
/// Lives in `config` so `llm`, `tools` and `learn` can share it without crossing module boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditMode {
    #[default]
    Hashline,
    Replace,
    Patch,
    ApplyPatch,
    Sloppy,
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
    /// Model steps per turn; zero means unlimited for interactive turns.
    pub max_steps: u32,
    pub temperature: f64,
    pub max_tokens: u32,
    /// `None` follows `[edit]`; the improvement loop may pin one mode per model.
    pub edit_mode: Option<EditMode>,
}

impl Harness {
    /// Apply a finite cap for evaluation and bounded automatic repair, including unlimited harnesses.
    pub fn capped_steps(&self, cap: u32) -> u32 {
        if self.max_steps == 0 {
            cap
        } else {
            self.max_steps.min(cap)
        }
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            system_prompt: "Prioritize correctness and maintainability. Deliver the requested behavior, use evidence already available, and report only verification actually performed.".into(),
            memory: vec![],
            tools: crate::config::tools::default_tools(),
            subagents: vec![],
            max_steps: 0,
            temperature: 0.2,
            max_tokens: 0,
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
    /// Sibling model DBs (matched by model name, or `provider/model`) whose externally verified
    /// trajectories and tool repairs may be learned from for this model's training data.
    #[serde(alias = "distill_from")]
    pub learn_from: Vec<String>,
    /// Learn from every sibling model DB in the project, not only `learn_from`.
    #[serde(alias = "distill_from_all_verified")]
    pub learn_from_all_verified: bool,
    /// Maximum share of a harvested batch that may come from sibling models (0-50).
    #[serde(alias = "distill_max_percent")]
    pub learn_from_max_percent: u32,
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
            learn_from: Vec::new(),
            learn_from_all_verified: false,
            learn_from_max_percent: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Zero disables the execution deadline.
    #[serde(default)]
    pub timeout_secs: u64,
    /// What the tool does, shown to the model. Empty uses a generic description.
    #[serde(default)]
    pub description: String,
    /// JSON Schema (an object schema) for the arguments. When set, the model's arguments are
    /// validated against its required properties and passed to the program as one JSON object on
    /// stdin; when omitted the tool takes a single string `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// Which shell runs `bash` commands. `auto` uses bash when one is installed: `/bin/bash` on Unix, and
/// Git for Windows' `bash.exe` on Windows (WSL's launcher does not count). Without bash it uses the
/// built-in shell (`embedded`), and on Windows without an embedded build it uses PowerShell.
/// `system` requires bash. `powershell` runs PowerShell on Windows only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellBackend {
    #[default]
    Auto,
    System,
    Embedded,
    PowerShell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Maximum bash timeout in seconds; zero disables the deadline.
    pub bash_timeout_secs: u64,
    /// Transfer a still-running foreground command to `bg` after this duration; zero disables.
    pub auto_background_secs: u64,
    /// `--no-pty`: background commands run without a pseudo-terminal.
    pub no_pty: bool,
    /// Shell that runs `bash` commands: `auto`, `system`, or `embedded` (built-in POSIX shell).
    pub backend: ShellBackend,
}
impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            bash_timeout_secs: 300,
            auto_background_secs: 15,
            no_pty: false,
            backend: ShellBackend::Auto,
        }
    }
}

/// One host added with `/ssh add`. `~/.ssh/config` hosts are merged with these; an entry here wins on a name clash.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshHostConfig {
    /// Hostname or address to connect to.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Identity file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A named model preset (`/modelpreset`): role model assignments plus the thinking level.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPreset {
    pub roles: BTreeMap<String, crate::llm::model_config::ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<crate::llm::profiles::ReasoningEffort>,
}

/// Use an OpenAI-compatible server (LM Studio, Ollama, llama.cpp, a hosted API) instead of the
/// supervised local MLX model. Weight updates are unavailable for endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub base_url: String,
    /// Name of the environment variable holding the bearer token (the token itself is never stored).
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub allow_insecure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<crate::llm::model_config::ApiProtocol>,
    #[serde(
        default,
        deserialize_with = "crate::llm::model_config::deserialize_headers",
        skip_serializing_if = "Option::is_none"
    )]
    pub headers: Option<crate::llm::model_config::RequestHeaders>,
    /// ChatGPT workspace pin. Other providers reject this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    /// Prompt before writes, edits and shell commands.
    Ask,
    /// Workspace edits, file writes and non-destructive shell commands run without a prompt;
    /// destructive commands (recursive deletes, history-rewriting git, privilege escalation,
    /// piping downloads into a shell) and tools that leave the workspace still ask.
    #[default]
    Write,
    Auto,
    /// Read-only tools only.
    Readonly,
}

impl Approval {
    /// Canonical policy names for command-line choices, completion and settings views.
    pub const NAMES: &'static [&'static str] = &["ask", "write", "auto", "readonly"];

    pub const fn description(self) -> &'static str {
        match self {
            Self::Ask => "asks before every edit and command",
            Self::Write => {
                "edits and safe commands run; destructive or outside-the-project actions ask"
            }
            Self::Auto => "runs everything without asking",
            Self::Readonly => "read-only tools only",
        }
    }
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

pub use crate::llm::model_config::MAX_CONTEXT_WINDOW;

/// Operator context budget. Auto follows model metadata; a number caps retained history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextWindow {
    #[default]
    Auto,
    Tokens(u32),
}

impl ContextWindow {
    pub fn tokens(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Tokens(n) => Some(n),
        }
    }
}

impl Serialize for ContextWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Tokens(n) => serializer.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for ContextWindow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Number(u32),
            Text(String),
        }
        match Input::deserialize(deserializer)? {
            Input::Text(s) if s.eq_ignore_ascii_case("auto") => Ok(Self::Auto),
            Input::Number(n) if (2048..=MAX_CONTEXT_WINDOW).contains(&n) => Ok(Self::Tokens(n)),
            _ => Err(serde::de::Error::custom(
                "context window must be 'auto' or 2048..2097152 tokens",
            )),
        }
    }
}

/// `[skills]`: skill registries. No registry is built in; nothing is contacted until one is configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Registry name → index URL (`https://` or `file://`). Used by `/skills install <name>/<skill>` and search.
    pub registries: BTreeMap<String, String>,
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
    /// Effective private operator policy, never supplied by a project or serialized.
    #[serde(skip)]
    pub chatgpt_allowed_accounts: Option<std::collections::BTreeSet<String>>,
    #[serde(skip)]
    pub chatgpt_pinned_accounts: std::collections::BTreeSet<String>,
    #[serde(skip)]
    pub provider_cache_namespace: Option<String>,
    /// Python with mlx-lm for local models; defaults to the environment `dial setup` creates.
    pub python: PathBuf,
    pub worker: PathBuf,
    pub model: String,
    /// Explicit "mlx" clears any inherited endpoint when a project selects a local model.
    pub provider: Option<String>,
    pub endpoint: Option<EndpointConfig>,
    pub providers: BTreeMap<String, crate::llm::model_config::ProviderSpec>,
    pub model_roles: BTreeMap<String, crate::llm::model_config::ModelSelection>,
    /// Favorite model identities, independent of role assignments.
    pub model_favorites: Vec<crate::llm::model_config::ModelSelection>,
    /// Exact provider/model profiles; capabilities must be declared by the operator.
    pub model_profiles: BTreeMap<String, crate::llm::profiles::ModelProfile>,
    pub model_profile: Option<String>,
    /// `thinking = "auto"`: each user turn's reasoning effort is judged from its prompt, clamped to the
    /// model profile's declared efforts, and falls back to the model default when no judge is usable.
    pub thinking_auto: bool,
    pub ui: crate::llm::model_config::UiConfig,
    /// Defaults to the project directory name.
    pub use_case: String,
    pub worker_timeout_secs: u64,
    /// Coordinate local model work across sessions. Disable to allow concurrent compute.
    pub compute_locks: bool,
    /// Silence limit for remote model responses, independent of long local workers. Covers both the
    /// wait for the first event and silence between events. `0` disables it explicitly; local model
    /// servers are never limited.
    pub remote_model_idle_timeout_secs: u64,
    pub reasoning_guard: ReasoningGuardConfig,
    pub request_recovery: RequestRecoveryConfig,
    pub retry: RetryConfig,
    pub portable_rules: PortableRulesConfig,
    pub codebase: CodebaseConfig,
    pub assistance: AssistanceConfig,
    pub efficiency: EfficiencyConfig,
    pub steering: config::steering::Config,
    pub local_runtime: crate::llm::local_runtime::LocalRuntime,
    pub adaptive: config::adaptive::Config,
    pub reliability: config::reliability::Config,
    pub laya: crate::config::laya::Config,
    pub stats: config::stats::StatsConfig,
    pub telemetry: config::telemetry::TelemetryConfig,
    pub voice: config::voice::VoiceConfig,
    pub collab: config::collab::Config,
    pub share: crate::config::share::Config,
    /// Effective model budget, resolved after loading. Never persisted as an operator override.
    #[serde(skip)]
    pub context_window: u32,
    #[serde(rename = "context_window")]
    pub context_limit: ContextWindow,
    pub cloud_context_window: ContextWindow,
    pub local_context_window: ContextWindow,
    pub harness: Harness,
    /// `[skills]`: registries for `/skills install <registry>/<skill>` and `/skills search`.
    pub skills: SkillsConfig,
    /// Prefer OpenAI Codex's Responses WebSocket transport (default on; falls back to SSE only on handshake failure).
    pub codex_websocket: bool,
    pub gate: Option<GateConfig>,
    pub training: TrainingConfig,
    pub improve: ImproveConfig,
    pub approval: Approval,
    pub sandbox: SandboxConfig,
    pub shell: ShellConfig,
    pub edit: crate::config::tools::EditSettings,
    /// Process/CLI mode pin; never persisted as an operator configuration field.
    #[serde(skip)]
    pub edit_mode_override: Option<EditMode>,
    pub compaction: crate::config::compaction::CompactionConfig,
    pub ask: crate::config::ask::AskConfig,
    pub checkpoint: crate::config::checkpoint::CheckpointConfig,
    pub tools: BTreeMap<String, ToolConfig>,
    pub subagents: BTreeMap<String, String>,
    /// Operator ceiling for running live subagents across `task start` batches (1..=32).
    pub subagent_max_live_jobs: usize,
    /// Integration tools to offer (`bg`, `web_fetch`, `web_search`, `lsp`, `ast_search`, `ast_edit`).
    /// Every tool schema costs the model context and attention; trim this list for small models.
    /// MCP tools follow `[mcp.*]` instead.
    pub integrations: Vec<String>,
    /// `[bash_interceptor]`: points bash commands that duplicate a dedicated tool at it. On by
    /// default in `hint` mode (the command runs and gains a one-line pointer); `block` refuses it.
    pub bash_interceptor: crate::config::tools::bash_interceptor::BashInterceptorConfig,
    /// `[ssh]`: host key policy and connection timing for the ssh tool. Hosts come from `~/.ssh/config`.
    pub ssh: crate::config::tools::ssh::SshConfig,
    /// `[ssh_hosts.<name>]`: hosts added with `/ssh add`; `~/.ssh/config` hosts are merged with these.
    pub ssh_hosts: BTreeMap<String, SshHostConfig>,
    /// Provider request tier: `none` omits it; `priority` (`/fast`), `flex` (`/slow`), or a provider-accepted tier.
    pub service_tier: String,
    /// `/fast` on: request priority processing unless an explicit `service_tier` is set.
    pub fast_mode: bool,
    /// `/slow` on: request flex processing unless `fast_mode` or an explicit `service_tier` is set.
    pub slow_mode: bool,
    /// Request the extended context window where the provider offers one (`/extended-context`).
    pub extended_context: bool,
    /// `[model_presets.<name>]`: saved role models and thinking level (`/modelpreset`).
    pub model_presets: BTreeMap<String, ModelPreset>,
    /// Snowflake account identifier, substituted for `<account>` in the Snowflake endpoint.
    #[serde(default)]
    pub snowflake_account: Option<String>,
    /// Cloudflare account id, substituted for `<account>` in the AI Gateway endpoint.
    #[serde(default)]
    pub cloudflare_account_id: Option<String>,
    /// Cloudflare AI Gateway id, substituted for `<gateway>` in the AI Gateway endpoint.
    #[serde(default)]
    pub cloudflare_gateway_id: Option<String>,
    /// Foreign tools' user-level rules and context files to load (for example `codex`, `gemini`). Opt-in: empty loads none.
    pub foreign_sources: Vec<String>,
    /// MCP servers whose tools are offered as `mcp__<server>__<tool>`. Trusted operator programs.
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// MCP behavior shared by all servers. `discover` and `project_approval` are reserved: they load
    /// but nothing reads them yet.
    pub mcp_settings: crate::config::tools::mcp::McpSettings,
    /// `[lsp]` write-through switches plus servers overriding or extending the built-in defaults.
    pub lsp: crate::config::tools::lsp::LspConfig,
    /// Debug Adapter Protocol adapters keyed by adapter id.
    pub dap: BTreeMap<String, crate::config::tools::dap::DapAdapterConfig>,
    /// Git hook/signing policy.
    pub git: crate::config::tools::git::GitConfig,
    /// `[worktree]`: what `/wt` does to the source checkout after moving the session into a worktree.
    pub worktree: WorktreeConfig,
    /// `[commit]`: persistent reply cache for `/commit` and `dial commit`.
    pub commit: CommitConfig,
    pub web: WebConfig,
    pub verification: VerificationConfig,
    /// Bounded isolated patch search. Requires explicit paths and executable checks.
    pub solve: crate::config::solve::SolveConfig,
    /// Narrow stronger-model advice; remote providers require explicit operator opt-in.
    pub escalation: crate::config::solve::EscalationConfig,
    pub browser: crate::config::tools::browser::BrowserConfig,
    pub kernel: crate::config::tools::kernel::KernelConfig,
    pub eval: crate::config::tools::eval::EvalConfig,
    pub memory: MemoryConfig,
    /// Operator-owned lifecycle hooks (`[[hooks]]`); see `agent::hooks`. Arrays from every config
    /// layer are appended, so a project layer can add hooks but never drop global ones.
    pub hooks: Vec<HookConfig>,
    /// Runtime extension plugins (`[extensions]`).
    pub extensions: ExtensionsConfig,
    pub budget: BudgetConfig,
    /// Named subagents (`[agents.<name>]`) built on a built-in type, with their own instructions,
    /// tool subset, step budget, spawn policy and model. Markdown definitions: `agent::agent_defs`.
    pub agents: BTreeMap<String, AgentConfig>,
    pub task: crate::config::task::TaskConfig,
    /// Private view for `task` workers: backend choice (`[isolation] backend`).
    pub isolation: crate::config::isolation::IsolationConfig,
    /// Opt-in behaviors that are still under evaluation.
    pub experimental: ExperimentalConfig,
}

/// `[worktree]`: behavior of `/wt` worktrees.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// After `/wt` moves uncommitted changes into the new worktree, reset tracked files and remove
    /// untracked, non-ignored files from the source checkout.
    pub clean_source: bool,
}

/// `[commit]`: persistent reply cache for `/commit` and `dial commit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommitConfig {
    /// Reuse stored model replies for identical requests.
    pub cache_enabled: bool,
    /// Lifetime of a stored reply in days.
    pub cache_ttl_days: f64,
}

impl Default for CommitConfig {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            cache_ttl_days: 14.0,
        }
    }
}

/// Experimental agent workflow features; every feature is disabled unless explicitly enabled.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExperimentalConfig {
    pub work_packet_contracts: bool,
    pub verification_receipts: bool,
    pub context_overlap_dedup: bool,
    pub profile_matching: bool,
    pub tool_speculation: bool,
}

/// A user-defined subagent built on a built-in type. Writers (`base = "worker"`) are allowed:
/// launching one still needs approval (agent names are registered as writers, fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// `scout`, `reviewer`, `planner`, `worker` or `test-runner`.
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
    /// Agents this one may start (`["*"]` any; `[]` none); default: the base type's policy.
    #[serde(default)]
    pub spawns: Option<Vec<String>>,
    /// Model fallback list: `@role`, `provider/model[:level]` or a bare model name.
    #[serde(default)]
    pub model: Vec<String>,
    /// Thinking level: off, minimal, low, medium, high, xhigh or max.
    #[serde(default)]
    pub thinking_level: Option<String>,
    /// Default JSON Schema for the agent's structured result.
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    /// Skills whose instructions are loaded into the agent's context at start.
    #[serde(default)]
    pub autoload_skills: Vec<String>,
}

fn default_agent_base() -> String {
    "scout".into()
}

/// Spend caps in USD, enforced between model requests. They need catalog pricing for the model and
/// reported token usage: a model with no known price is never stopped by them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Stop the turn once the session has cost this much. `0` (the default) or unset disables this cap.
    pub max_session_cost_usd: Option<f64>,
    /// Stop the turn once it alone has cost this much. `0` (the default) or unset disables this cap.
    pub max_turn_cost_usd: Option<f64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_session_cost_usd: Some(0.0),
            max_turn_cost_usd: Some(0.0),
        }
    }
}

/// A program run at a lifecycle event. Trusted and unsandboxed, like MCP servers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Lifecycle event; see `config::hooks::EVENTS` (alias names such as `tool_call` are accepted).
    pub event: String,
    /// Anchored regex on the tool name (tool events only); omitted matches every tool.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Program and arguments; the event payload arrives as JSON on stdin.
    pub command: Vec<String>,
    /// Seconds before the hook is killed (default 30); 0 disables the deadline.
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    /// Whether a crash, timeout or start failure blocks the action. Defaults to true for
    /// `pre_tool_use` (policy hooks) and false for every other event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_closed: Option<bool>,
}

fn default_hook_timeout() -> u64 {
    30
}

/// Runtime extension plugins: executables speaking JSON-RPC over stdio (`tools::plugin`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionsConfig {
    /// Start runtime plugins at all.
    pub enabled: bool,
    /// Also run executables found in `~/.dial/extensions` (trusted) and, once explicitly trusted,
    /// in the project's `.dial/extensions`.
    pub discover: bool,
    /// Deadline for each plugin request (initialize, events, commands, tool calls); 0 disables it.
    pub timeout_secs: u64,
    /// Explicitly configured plugins keyed by name. Trusted operator programs: they run unsandboxed.
    pub plugins: BTreeMap<String, PluginConfig>,
    /// Values for flags registered by plugins (`registerFlag`); `--ext-flag name=value` sets them.
    pub flags: BTreeMap<String, toml::Value>,
}

impl Default for ExtensionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            discover: true,
            timeout_secs: 30,
            plugins: BTreeMap::new(),
            flags: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// An MCP server: a stdio program or a streamable-HTTP endpoint. Like `[tools.*]` it
/// is a trusted program: it runs unsandboxed. See the "HTTP MCP" section of README.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stdio server executable. Exactly one of `program` and `url` is set.
    pub program: PathBuf,
    /// Remote endpoint (https, or http on localhost).
    pub url: Option<String>,
    /// Wire protocol for `url` servers: `http` (streamable HTTP, the default) or `sse` (legacy
    /// HTTP+SSE: the event stream announces where requests are POSTed). Stdio servers leave it unset or `stdio`.
    pub transport: Option<McpTransport>,
    /// Name of an environment variable holding a bearer token for `url` servers.
    pub bearer_env: Option<String>,
    /// Extra non-secret request headers for `url` servers.
    pub headers: BTreeMap<String, String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Working directory for stdio servers; defaults to dial's own.
    pub cwd: Option<PathBuf>,
    /// Per request timeout; 0 waits until the server answers or the call is cancelled (Esc).
    pub timeout_secs: u64,
    /// Budget for spawn/handshake/tool listing. Connections run in the background either way.
    pub connect_timeout_secs: u64,
    /// Let tools annotated `readOnlyHint: true` run without approval (and in read-only subagents). Default off: every MCP tool requires approval unless this is set.
    pub trust_read_only_hint: bool,
    /// Set false to keep the entry without starting it.
    pub enabled: bool,
    /// OAuth 2.1 settings for `url` servers that require authorization.
    pub oauth: McpOAuthConfig,
    /// Lifecycle events to forward to this server as `notifications/dial/event`.
    pub lifecycle_hooks: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::new(),
            url: None,
            transport: None,
            bearer_env: None,
            headers: BTreeMap::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            timeout_secs: 120,
            connect_timeout_secs: 30,
            trust_read_only_hint: false,
            enabled: true,
            oauth: McpOAuthConfig::default(),
            lifecycle_hooks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

/// OAuth for one remote MCP server. Discovery (RFC 9728 + RFC 8414) and dynamic client
/// registration need no settings; a pre-registered client sets `client_id`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpOAuthConfig {
    /// Pre-registered client id; empty uses dynamic client registration.
    pub client_id: Option<String>,
    /// Environment variable holding the pre-registered client's secret, if it has one.
    pub client_secret_env: Option<String>,
    /// Space-separated scopes; empty uses what the server advertises.
    pub scopes: Option<String>,
    /// Loopback callback port; 0 picks a free port.
    pub callback_port: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Search endpoint override; empty uses the built-in default provider.
    pub search_url: String,
    /// Environment variable holding the search endpoint's API key, when it needs one.
    pub search_api_key_env: Option<String>,
    /// Reader chain for HTML pages, tried in order after the markdown alternates. Known readers:
    /// `native`, `trafilatura`, `lynx`, `jina` (jina also needs `jina_reader = true`). Empty uses
    /// `native`, `trafilatura`, `lynx`.
    pub reader_chain: Vec<String>,
    /// Allow the `jina` reader, which sends the page URL to r.jina.ai. Off by default.
    pub jina_reader: bool,
    /// Search backend: `auto` (default; empty also means auto) picks the first provider whose
    /// environment key is set, else DuckDuckGo. Or name one: brave, tavily, exa, serper, kagi,
    /// jina, perplexity, google, bing, searxng, duckduckgo. Ignored when `search_url` is set.
    pub search_provider: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationConfig {
    /// Record check results the model runs and gate `finish` on failed checks that were never repaired.
    pub enabled: bool,
    /// Also run focused checks on the harness's own initiative: a behavioral baseline before edits,
    /// checks at `finish` and after the turn, and up to `repair_attempts` automatic repair rounds.
    /// Off leaves verification to the model, which the system prompt requires before yielding.
    pub automatic_checks: bool,
    pub repair_attempts: u8,
    pub commands: Vec<VerificationCommand>,
    /// Names of configured commands that check behavioral correctness, not only syntax or buildability.
    /// Explicit operator declaration; only failing-before/passing-after runs qualify as repair evidence.
    pub behavioral_checks: Vec<String>,
    /// Operator-owned Python argv override; empty selects the owning project's runner.
    pub python_command: Vec<String>,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            automatic_checks: false,
            repair_attempts: 0,
            commands: Vec::new(),
            behavioral_checks: Vec::new(),
            python_command: Vec::new(),
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
    0
}

/// Operator-owned limits; harness improvement cannot increase them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningGuardConfig {
    pub enabled: bool,
    pub max_pre_action_secs: u64,
    /// Bytes of streamed reasoning, not an estimate of hidden reasoning tokens.
    pub max_reasoning_bytes: usize,
    /// After a gate hit with no open todo, only `todo` is offered until a plan exists.
    pub require_plan: bool,
}

impl Default for ReasoningGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_pre_action_secs: 0,
            max_reasoning_bytes: 0,
            require_plan: false,
        }
    }
}

/// Operator-owned recovery for a silent inference attempt, not a task/step limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestRecoveryConfig {
    /// Seconds without a model-worker event before cancelling just that attempt; 0 disables the watchdog.
    pub silence_secs: u64,
    /// Retry the same request once automatically; subsequent recovery needs user direction.
    pub retry_once: bool,
}
impl Default for RequestRecoveryConfig {
    fn default() -> Self {
        Self {
            silence_secs: 120,
            retry_once: true,
        }
    }
}

/// Provider-request retry policy (`[retry]`). Applies to transient transport failures, throttles and
/// incomplete streams within one model request; quota exhaustion is never waited out unless opted in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    /// Attempts per model request, including the first (1..=50).
    pub max_attempts: u32,
    /// Longest single server-requested or backoff wait honoured before giving up (1..=3600 s).
    pub max_delay_secs: u64,
    /// Wait for a usage-quota window to reset (bounded by `max_delay_secs`) instead of failing fast.
    pub wait_for_quota_reset: bool,
    /// After a refreshed subscription credential is still rejected (or its quota is exhausted), retry
    /// the request once with another signed-in, policy-allowed account of the same provider.
    pub rotate_accounts: bool,
    /// Ordered selectors attempted after a terminal provider failure. Keys are role names or
    /// exact `provider/model` identities; values may be bare models, qualified models, or `@role`.
    pub fallback_chains: std::collections::BTreeMap<String, Vec<String>>,
}
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            max_delay_secs: 300,
            wait_for_quota_reset: false,
            rotate_accounts: true,
            fallback_chains: std::collections::BTreeMap::new(),
        }
    }
}

/// Non-executing scoped guidance and bounded inference-only rule interception.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortableRulesConfig {
    pub enabled: bool,
    pub max_regenerations: u32,
}
impl Default for PortableRulesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_regenerations: 8,
        }
    }
}

/// `assistance.lean_local`. `auto` (default) turns the lean profile on for local routes whose planned
/// context window is at most [`LEAN_AUTO_MAX_WINDOW`]; TOML `true` forces it on for every local route
/// and `false` opts out, so boolean configurations keep their meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeanLocal {
    #[default]
    Auto,
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
    #[default]
    Phase,
    /// Leave it to the model, the server and the model profile.
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
            thinking: LocalThinking::Phase,
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
    /// guidance omitted when the local server runs one request at a time. `auto` (default) enables it
    /// only when the planned window is at most `LEAN_AUTO_MAX_WINDOW`; `true` forces it on, `false`
    /// opts out. Hosted routes are unchanged. Measure with `dial benchmark --ablate-assistance`.
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
            lean_local: LeanLocal::Auto,
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
    /// 0 plans against the full window. Every step resends the history, so a 1M-token window must
    /// not mean a 600k-token request: at the default the auto trigger fires near 178k instead of
    /// 680k. Smaller model windows are unaffected.
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
    /// Prepend saved codebase context (architecture notes, source outlines) to every user turn.
    /// Off leaves retrieval to the explicit `codebase_memory` tool.
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
            ambient_context: false,
            synthesize_architecture: false,
            history: true,
            architecture_model: "mlx-community/Qwen2.5-3B-Instruct-4bit".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryConfig {
    /// Legacy migration input; backend is the sole runtime selector.
    #[serde(skip_serializing)]
    pub enabled: bool,
    pub backend: crate::config::memory_backend::MemoryBackendId,
    #[serde(skip)]
    pub backend_explicit: bool,
    pub context_limit: usize,
    pub context_max_bytes: usize,
    pub request_max_bytes: usize,
    pub autolearn: bool,
    pub autolearn_auto_continue: bool,
    pub autolearn_min_tool_calls: usize,
    pub local: crate::config::memory_backend::LocalConfig,
    pub hindsight: crate::config::memory_backend::HindsightConfig,
    pub sharpshooter: crate::config::memory_backend::SharpshooterConfig,
    pub semantic: crate::config::memory_backend::SemanticConfig,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: Default::default(),
            backend_explicit: false,
            context_limit: 0,
            context_max_bytes: 0,
            request_max_bytes: 1048576,
            autolearn: false,
            autolearn_auto_continue: false,
            autolearn_min_tool_calls: 5,
            local: Default::default(),
            hindsight: Default::default(),
            sharpshooter: Default::default(),
            semantic: Default::default(),
        }
    }
}
impl<'de> Deserialize<'de> for MemoryConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Wire {
            enabled: bool,
            backend: Option<crate::config::memory_backend::MemoryBackendId>,
            context_limit: usize,
            context_max_bytes: usize,
            request_max_bytes: usize,
            autolearn: bool,
            autolearn_auto_continue: bool,
            autolearn_min_tool_calls: usize,
            local: crate::config::memory_backend::LocalConfig,
            hindsight: crate::config::memory_backend::HindsightConfig,
            sharpshooter: crate::config::memory_backend::SharpshooterConfig,
            semantic: crate::config::memory_backend::SemanticConfig,
        }
        impl Default for Wire {
            fn default() -> Self {
                Self {
                    enabled: true,
                    backend: None,
                    context_limit: 0,
                    context_max_bytes: 0,
                    request_max_bytes: 1048576,
                    autolearn: false,
                    autolearn_auto_continue: false,
                    autolearn_min_tool_calls: 5,
                    local: Default::default(),
                    hindsight: Default::default(),
                    sharpshooter: Default::default(),
                    semantic: Default::default(),
                }
            }
        }
        let value = Wire::deserialize(deserializer)?;
        Ok(Self {
            enabled: value.enabled,
            backend_explicit: value.backend.is_some(),
            backend: value.backend.unwrap_or(if value.enabled {
                crate::config::memory_backend::MemoryBackendId::Provenance
            } else {
                crate::config::memory_backend::MemoryBackendId::Off
            }),
            context_limit: value.context_limit,
            context_max_bytes: value.context_max_bytes,
            request_max_bytes: value.request_max_bytes,
            autolearn: value.autolearn,
            autolearn_auto_continue: value.autolearn_auto_continue,
            autolearn_min_tool_calls: value.autolearn_min_tool_calls,
            local: value.local,
            hindsight: value.hindsight,
            sharpshooter: value.sharpshooter,
            semantic: value.semantic,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::new(),
            project_root: PathBuf::new(),
            user_dir: default_user_dir(),
            chatgpt_allowed_accounts: None,
            chatgpt_pinned_accounts: Default::default(),
            provider_cache_namespace: None,
            python: PathBuf::new(),
            worker: PathBuf::new(),
            model: String::new(),
            provider: None,
            endpoint: None,
            use_case: String::new(),
            providers: BTreeMap::new(),
            model_roles: BTreeMap::new(),
            model_favorites: Vec::new(),
            ui: crate::llm::model_config::UiConfig::default(),
            model_profiles: BTreeMap::new(),
            model_profile: None,
            thinking_auto: false,
            worker_timeout_secs: 0,
            compute_locks: true,
            context_window: 32768,
            context_limit: ContextWindow::Auto,
            cloud_context_window: ContextWindow::Auto,
            local_context_window: ContextWindow::Auto,
            remote_model_idle_timeout_secs: DEFAULT_REMOTE_IDLE_TIMEOUT_SECS,
            reasoning_guard: ReasoningGuardConfig::default(),
            request_recovery: RequestRecoveryConfig::default(),
            retry: RetryConfig::default(),
            portable_rules: PortableRulesConfig::default(),
            codebase: CodebaseConfig::default(),
            assistance: AssistanceConfig::default(),
            efficiency: EfficiencyConfig::default(),
            steering: config::steering::Config::default(),
            local_runtime: Default::default(),
            adaptive: config::adaptive::Config::default(),
            reliability: config::reliability::Config::default(),
            isolation: Default::default(),
            laya: Default::default(),
            stats: config::stats::StatsConfig::default(),
            telemetry: config::telemetry::TelemetryConfig::default(),
            voice: config::voice::VoiceConfig::default(),
            collab: config::collab::Config::default(),
            share: crate::config::share::Config::default(),
            harness: Harness::default(),
            skills: SkillsConfig::default(),
            codex_websocket: true,
            gate: None,
            training: TrainingConfig::default(),
            improve: ImproveConfig::default(),
            approval: Approval::default(),
            sandbox: SandboxConfig::default(),
            shell: ShellConfig::default(),
            edit: crate::config::tools::EditSettings::default(),
            edit_mode_override: None,
            compaction: crate::config::compaction::CompactionConfig::default(),
            ask: crate::config::ask::AskConfig::default(),
            checkpoint: crate::config::checkpoint::CheckpointConfig::default(),
            tools: BTreeMap::new(),
            subagents: BTreeMap::new(),
            subagent_max_live_jobs: 32,
            integrations: crate::config::tools::integrations::default_integrations(),
            bash_interceptor: Default::default(),
            ssh: Default::default(),
            ssh_hosts: BTreeMap::new(),
            service_tier: "none".into(),
            fast_mode: false,
            slow_mode: false,
            extended_context: false,
            snowflake_account: None,
            cloudflare_account_id: None,
            cloudflare_gateway_id: None,
            model_presets: BTreeMap::new(),
            foreign_sources: Vec::new(),
            mcp: BTreeMap::new(),
            lsp: crate::config::tools::lsp::LspConfig::default(),
            dap: BTreeMap::new(),
            git: Default::default(),
            worktree: Default::default(),
            commit: CommitConfig::default(),
            web: WebConfig::default(),
            verification: VerificationConfig::default(),
            solve: Default::default(),
            escalation: Default::default(),
            browser: Default::default(),
            mcp_settings: Default::default(),
            kernel: Default::default(),
            eval: Default::default(),
            memory: Default::default(),
            hooks: Vec::new(),
            extensions: ExtensionsConfig::default(),
            budget: BudgetConfig::default(),
            agents: BTreeMap::new(),
            task: Default::default(),
            experimental: ExperimentalConfig::default(),
        }
    }
}

/// Dial's user directory when no config is loaded: `DIAL_HOME`, else `$HOME/.dial`. Empty only when
/// neither is set, so a relative `user_dir` never resolves against the current directory.
pub(crate) fn default_user_dir() -> PathBuf {
    match std::env::var_os("DIAL_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::var_os("HOME")
            .map_or_else(PathBuf::new, |home| PathBuf::from(home).join(".dial")),
    }
}

/// Default silence limit for remote model streams (Anthropic, ChatGPT, OpenAI-compatible hosts).
/// A stalled connection fails and retries instead of waiting forever.
pub const DEFAULT_REMOTE_IDLE_TIMEOUT_SECS: u64 = 300;

/// Automatic self-improvement. On by default: dial records turns, runs verified self-play on
/// procedurally generated tasks, and proposes harness and LoRA changes that only a sealed gate can admit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImproveConfig {
    pub enabled: bool,
    /// "Teach this model": coding sessions with a local model record verified turns as training
    /// examples and run on the latest admitted version (harness and LoRA adapter). Off by default.
    pub teach: bool,
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
    /// Run the background cycle at session start/exit, subject to idle, AC-power, budget and /compute pause gates. When false, only `dial improve cycle` runs it.
    pub background: bool,
    /// Start a background cycle only after this many minutes without a coding turn (0 = any time).
    pub idle_minutes: u32,
    /// Do not start background cycles on battery power.
    pub require_ac_power: bool,
    /// Background compute allowed per day, in minutes, enforced during a cycle too (0 = unlimited).
    pub daily_compute_minutes: u32,
    /// Maximum observed endpoint-model spend per background cycle/day in USD; zero disables background cloud work.
    pub max_daily_usd: f64,
    /// Failure-driven distillation: the `coach` role solves fresh tasks from weak train families and
    /// only verifier-passed teacher runs become training records (source `teacher-<model>`). Requires
    /// an assigned coach role. Off by default.
    pub distill: bool,
    /// Teacher self-play tasks per cycle when `distill` is on (1..=64).
    pub distill_tasks: u32,
}

impl Default for ImproveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            teach: false,
            interval_secs: 6 * 3600,
            min_new_turns: 5,
            selfplay_tasks: 24,
            gate_primary_cases: 30,
            gate_guard_cases: 20,
            alpha: 0.1,
            min_net_wins: 2,
            max_candidates: 0,
            drift_tolerance: 3.0,
            learn_from_sessions: true,
            lora_min_examples: 24,
            monitor_drop: 15.0,
            monitor_min_turns: 8,
            background: true,
            idle_minutes: 10,
            require_ac_power: true,
            daily_compute_minutes: 0,
            max_daily_usd: 0.0,
            distill: false,
            distill_tasks: 4,
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
    /// Primary-suite difficulty tier the baseline settled on (auto cycles only; `None` otherwise).
    #[serde(default)]
    pub tier: Option<u8>,
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

#[cfg(test)]
mod lean_local_tests;
