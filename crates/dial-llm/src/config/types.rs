//! Configuration data model: the root [`Config`] and its sections. Sections are grouped by area in
//! private submodules and re-exported here, so every type keeps its `config::types::` path.
use crate::config;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

mod assistance;
mod context_files;
mod execution;
mod harness;
mod integrations;
mod learning;
mod memory;
mod model;
mod recovery;
mod verification;
mod workflow;

pub use assistance::*;
pub use context_files::*;
pub use execution::*;
pub use harness::*;
pub use integrations::*;
pub use learning::*;
pub use memory::*;
pub use model::*;
pub use recovery::*;
pub use verification::*;
pub use workflow::*;

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
    /// `[message_routing]`: how a message typed during a running turn is routed.
    pub message_routing: config::message_routing::Config,
    pub local_runtime: crate::llm::local_runtime::LocalRuntime,
    pub adaptive: config::adaptive::Config,
    pub reliability: config::reliability::Config,
    pub laya: crate::config::laya::Config,
    pub stats: config::stats::StatsConfig,
    pub telemetry: config::telemetry::TelemetryConfig,
    pub voice: config::voice::VoiceConfig,
    pub image: config::image::ImageConfig,
    pub collab: config::collab::Config,
    pub coordination: config::coordination::CoordinationConfig,
    pub share: crate::config::share::Config,
    pub secrets: crate::config::secrets::Config,
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
    pub cache: crate::config::cache::CacheConfig,
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
    /// `[ida]` IDA Pro runtime settings for the `ida` tool.
    pub ida: crate::config::tools::ida::IdaConfig,
    /// Git hook/signing policy.
    pub git: crate::config::tools::git::GitConfig,
    /// `[worktree]`: what `/wt` does to the source checkout after moving the session into a worktree.
    pub worktree: WorktreeConfig,
    /// `[commit]`: persistent reply cache for `/commit` and `dial commit`.
    pub commit: CommitConfig,
    /// `[context_files]`: extra instruction file names read beside each project `AGENTS.md`.
    pub context_files: ContextFilesConfig,
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
            message_routing: config::message_routing::Config::default(),
            local_runtime: Default::default(),
            adaptive: config::adaptive::Config::default(),
            reliability: config::reliability::Config::default(),
            isolation: Default::default(),
            laya: Default::default(),
            stats: config::stats::StatsConfig::default(),
            telemetry: config::telemetry::TelemetryConfig::default(),
            voice: config::voice::VoiceConfig::default(),
            image: config::image::ImageConfig::default(),
            collab: config::collab::Config::default(),
            coordination: Default::default(),
            share: crate::config::share::Config::default(),
            secrets: crate::config::secrets::Config::default(),
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
            cache: crate::config::cache::CacheConfig::default(),
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
            ida: crate::config::tools::ida::IdaConfig::default(),
            git: Default::default(),
            worktree: Default::default(),
            commit: CommitConfig::default(),
            context_files: ContextFilesConfig::default(),
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

/// Dial's user directory when no config is loaded, using the same resolution as Layout.
/// Empty only when no home is available, never an implicit relative `.dial` directory.
pub fn default_user_dir() -> PathBuf {
    config::paths::user_dir().unwrap_or_default()
}

#[cfg(test)]
mod lean_local_tests;
