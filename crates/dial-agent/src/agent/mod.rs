//! The agent runtime: one conversation, native tool calling, sessions, approvals, checkpoints,
//! compaction and (optionally) trajectory recording for the self-improvement loop.
//!
//! The same loop serves three callers: the interactive REPL and print mode (full context: memory,
//! skills, project files, persistent session) and the sealed gate (`run`: hermetic, ephemeral,
//! auto-approved, strict sandbox, nothing read from the user's environment).
pub mod base;
pub mod compact;
pub mod memory;
pub mod model;
pub(crate) mod prompts;
pub mod run;
pub mod safety;
pub mod tool_trace;
pub mod ui;
pub mod verify;
pub mod window;
pub mod workers;
pub mod workflow;
pub use verify::completion::TurnOutcome;
pub mod modes;
#[cfg(test)]
mod verification_state_tests;
pub use verify::verification::{plan_with_runner, related_tests_with_runner};
pub use workers::live_tasks::LiveTaskControl;

// Agent impl and helpers split out of this file by concern; private items are glob-imported so
// sibling modules keep resolving them through `crate::agent::`.
#[cfg(test)]
mod tests;

pub use base::helpers::{json_text, truncate};
pub use compact::compaction;
pub use memory::selfplay::{check_case, check_case_measured, selfplay_run};
pub use model::backend::{Runtime, make_backend, make_session_backend};
pub use model::laya;
pub use run::call_summary::{legacy_tool_presentation, legacy_tool_result, summarize_call};
pub use safety::approval;
pub use ui::surface::{Decision, NullUi, RequestPhase, Ui};
pub use workflow::todo;

use base::helpers::*;
use memory::selfplay::*;
use model::backend::*;
use model::roles::*;
use run::call_summary::*;
use run::dispatch::*;
use run::steering::*;
use ui::chat::*;
use verify::repair::*;

use crate::config::paths::Layout;
use crate::config::types::{Approval, Config, Event, SandboxMode, Version};
use crate::learn::verify::Check;
use crate::learn::worker;
use crate::llm::provider::{Backend, ProviderKind};
use crate::llm::{self, Chat, ToolCall};
use crate::runtime::tool_output::ToolOutput;
use crate::storage::checkpoint::Checkpoints;
use crate::storage::session::{self, Session};
use crate::storage::skills;
use crate::storage::store::Store;
use crate::tools::integrations::ext::{LifecycleEvent, Registry, ToolContext};
use crate::tools::knowledge::codebase::Codebase;
use crate::tools::knowledge::provenance::ProvenanceMemory;
use crate::tools::{self, Workspace};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};
use workflow::todo::TodoList;

pub struct TurnReport {
    pub text: String,
    pub outcome: TurnOutcome,
    pub steps: u32,
    pub interrupted: bool,
    pub trajectory: Option<i64>,
}
impl run::engine::Terminal for TurnReport {
    fn prevent_completion(&mut self, reason: &str) {
        if self.outcome == TurnOutcome::Completed {
            self.outcome = TurnOutcome::Incomplete;
            self.text.push_str(&format!("\n{reason}"));
        }
    }
}

struct PromptParts {
    soul: Option<String>,
    overrides: (Option<String>, Option<String>),
    context: Vec<window::context::ContextFile>,
    skills: String,
    memory: String,
    system_template: Option<window::prompt_template::Template>,
    system_template_error: Option<String>,
}

#[derive(Default, Debug, serde::Serialize)]
pub struct AgentMetrics {
    pub cached_tokens: u64,
    /// Main-agent responses in this runtime; transport retries and workers are not included.
    pub responses: u64,
    pub usage_reported_responses: u64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub compactions: u64,
    pub tool_contract_errors: u64,
    pub artifacts_retrieved: u64,
    pub dynamic_tools_used: u64,
    pub truncated_recoveries: u64,
    /// Automatic focused-verification repair rounds actually executed.
    pub repair_attempts: u64,
    /// Main-agent schema offers and admitted executions; worker and extension calls are not included.
    pub tool_trace: tool_trace::ToolTraceCounts,
    pub reasoning_interceptions: u64,
    pub turns_completed: u64,
}

/// Per-turn bookkeeping for what `todo done` checks at step boundaries.
#[derive(Default)]
struct StepChecks {
    /// Consultation allowance and failure ledger span all repair reentries in this user turn.
    auto_escalation: verify::auto_escalation::AutoEscalation,
    visual: verify::visual::Review,
    /// A successful finish call can request review without accepting the turn's outcome.
    finish_accepted: bool,
    /// The checkpoint turn whose changed files the checks cover.
    turn: Option<u64>,
    /// Successful file edits this turn, and how many of them the last check run covered.
    mutations: u64,
    checked: u64,
    /// Number of checkpointed edits whose bounded diff has been surfaced for review.
    diff_surfaced: u64,
    /// Non-read-only shell attempts outside checkpoint observation.
    uncheckpointed_shell_commands: u64,
    disclosed_shell_commands: u64,
    /// Approval-requiring extension attempts that may have effects outside checkpoint observation.
    uncheckpointed_extension_calls: u64,
    disclosed_extension_calls: u64,
    /// Side-effect epochs covered by the last focused verification pass.
    checked_shell_commands: u64,
    checked_extension_calls: u64,
    uncheckpointed_extension_tools: std::collections::BTreeSet<String>,
    /// Potentially mutating reproduction attempts. They stale evidence like any shell effect,
    /// but cannot themselves count as the intervening change that fixes a reproduction.
    repro_shell_commands: u64,
    /// Exact configured checks whose most recent run failed; retain them across new edits.
    failed_checks: Vec<verify::verification::FocusedCheck>,
    failed: bool,
    evidence: verify::evidence::Ledger,
}
impl StepChecks {
    fn revision(&self) -> verify::evidence::Revision {
        [
            self.mutations,
            self.uncheckpointed_shell_commands,
            self.uncheckpointed_extension_calls,
        ]
    }
    fn fix_revision(&self) -> verify::evidence::Revision {
        let mut revision = self.revision();
        revision[1] -= self.repro_shell_commands;
        revision
    }
    /// A model-written reproduction may have effects a checkpoint does not observe.
    fn note_repro_run(&mut self) {
        self.repro_shell_commands += 1;
        self.uncheckpointed_shell_commands += 1;
        self.disclosed_shell_commands += 1;
    }
    fn clean(&self) -> bool {
        self.checks_current() && !self.uncheckpointed_pending()
    }
    fn checks_current(&self) -> bool {
        (self.checked > 0 || self.checked_shell_commands > 0 || self.checked_extension_calls > 0)
            && self.checked == self.mutations
            && !self.failed
            && self.failed_checks.is_empty()
            && self.checked_shell_commands == self.uncheckpointed_shell_commands
            && self.checked_extension_calls == self.uncheckpointed_extension_calls
    }
    fn record_checked(&mut self) {
        self.checked = self.mutations;
        self.checked_shell_commands = self.uncheckpointed_shell_commands;
        self.checked_extension_calls = self.uncheckpointed_extension_calls;
    }
    fn uncheckpointed_pending(&self) -> bool {
        self.uncheckpointed_shell_commands > self.disclosed_shell_commands
            || self.uncheckpointed_extension_calls > self.disclosed_extension_calls
    }
    fn has_uncheckpointed_operations(&self) -> bool {
        self.uncheckpointed_shell_commands > 0 || self.uncheckpointed_extension_calls > 0
    }
    fn include_failed_checks(
        &mut self,
        root: &Path,
        checks: &mut Vec<verify::verification::FocusedCheck>,
    ) {
        // A Python syntax obligation for a file renamed or deleted since the failure is obsolete.
        self.failed_checks
            .retain(|failed| !verify::verification::obsolete_check(root, failed));
        for failed in &self.failed_checks {
            if !checks.iter().any(|check| check.command == failed.command) {
                checks.push(failed.clone());
            }
        }
    }
    fn status(&self) -> Value {
        let phase = if self.failed {
            "checks_failed"
        } else if self.uncheckpointed_pending() {
            "uncheckpointed_operations_pending_review"
        } else if self.has_uncheckpointed_operations() {
            "uncheckpointed_operations_disclosed_not_verified"
        } else if self.mutations == 0 {
            "no_edits"
        } else if self.checked < self.mutations {
            "edited_unchecked"
        } else if self.diff_surfaced < self.mutations {
            "checks_passed_diff_surfaced_review_pending"
        } else {
            "checks_passed_diff_surfaced"
        };
        json!({
            "phase":phase,
            "mutations":self.mutations,
            "checked_mutations":self.checked,
            "diff_surfaced_mutations":self.diff_surfaced,
            "uncheckpointed_shell_commands":self.uncheckpointed_shell_commands,
            "disclosed_shell_commands":self.disclosed_shell_commands,
            "uncheckpointed_extension_calls":self.uncheckpointed_extension_calls,
            "disclosed_extension_calls":self.disclosed_extension_calls,
            "uncheckpointed_extension_tools":self.uncheckpointed_extension_tools,
            "failed_checks":self.failed_checks.iter().map(|check| &check.name).collect::<Vec<_>>()
        })
    }
}

pub struct Agent<'a> {
    cancellation: crate::runtime::cancellation::SessionCancellation,
    config: &'a Config,
    auth_config: Config,
    /// Owned so a turn can move it into `run_loop` without conflicting with `&mut self`.
    backend: Option<Backend>,
    /// Cached local backend and the model it serves, for a role that has its own model.
    subagent_backend: Option<(String, Backend)>,
    /// Lazily created, cached side backends for role-routed requests (compaction, …).
    role_backends: BTreeMap<String, Backend>,
    /// Context the serving endpoint reports as currently loaded; may be far below the configured window.
    context_cap: Option<u32>,
    /// The configured route's backend-bound state: fallback cooldown and the primary's loaded cap.
    route: model::route::ActiveRoute,
    layout: Option<Layout>,
    session: Session,
    workspace: Workspace,
    /// Pluggable tools (web, MCP, LSP, …); empty for hermetic agents.
    ext: Registry,
    /// `bash` job completions not yet delivered to the model; sent once as a transient message
    /// in the next request (see `pump_subagents`, `run_loop`).
    background_notices: Vec<String>,
    todo: TodoList,
    memory: Option<Arc<ProvenanceMemory>>,
    memory_runtime: Option<Arc<crate::storage::memory_backend::MemoryRuntime>>,
    codebase: Option<Arc<Codebase>>,
    /// Per-turn code diffs searchable across sessions (`code_history`); `None` when disabled.
    history: Option<Arc<crate::tools::knowledge::history::ChangeHistory>>,
    experience: Option<Arc<crate::storage::experience::Experience>>,
    assistance: model::assistance::State,
    checkpoints: Option<Arc<Checkpoints>>,
    store: Option<&'a Store>,
    version: Version,
    system: String,
    system_messages: Vec<Value>,
    agents_messages: Vec<Value>,
    parts: PromptParts,
    approval: Approval,
    always: Vec<safety::approval::PermissionRule>,
    plan_mode: bool,
    session_cost: Option<f64>,
    tan_jobs: modes::tan::Jobs,
    pending_plan: Option<String>,
    /// Session-only model override set by `/switch`; the config on disk is never written.
    model_override: Option<Config>,
    /// While plan mode applies the `plan` model role: the `model_override` in force before it, restored
    /// when plan mode ends. `None` when the plan model is not in force.
    plan_model_prior: Option<Option<Config>>,
    /// Most recent plan submitted through `exit_plan`, so `/plan-review` can re-surface it.
    latest_plan: Option<String>,
    /// Actual nested human input awaiting projection onto its enclosing durable tool message.
    callback_question_results: Vec<Value>,
    pending_context_rollover: Option<window::context_notes::NewContextRequest>,
    exploration: workflow::exploration::Lifecycle,
    modes: modes::State,
    context_notes_reminder_boundary: Option<String>,
    /// Side-request cache namespace of the maintenance run in progress, so every summary chunk of
    /// one compaction shares a cache identity instead of minting a fresh one per call.
    active_side_namespace: Option<String>,
    context_rule_catalog: Option<Arc<safety::rules::Catalog>>,
    interrupt: Arc<AtomicBool>,
    hermetic: bool,
    /// Overrides the trajectory domain (self-play records as `selfplay:<family>`).
    domain_override: Option<String>,
    trajectory: Option<(i64, String, String)>,
    last_trajectory: Option<i64>,
    /// Actual inference requests paired with eligible targets from the current turn.
    training_capture: memory::training::Capture,
    logging: bool,
    /// Outcomes of recognised check commands (tests, builds, linters) run during the turn.
    verifications: Vec<bool>,
    /// Pass/fail of each behavioral check run this turn; all-passing clean turns record `checks:behavioral`.
    behavioral_checks: Vec<bool>,
    step_checks: StepChecks,
    /// Controller-owned, never derived from untrusted tool output metadata.
    portable_callback_intercepted: bool,
    edit_preview_intercepted: bool,
    cur_step: u32,
    pub last_prompt_tokens: u64,
    context_meter: window::context_meter::ContextMeter,
    /// How many conversation messages the request behind `last_prompt_tokens` carried.
    reported_messages: usize,
    step_probe: Option<crate::learn::probe::StepProbe>,
    enabled_tools: HashSet<String>,
    /// Task-scoped specialist selection; never durable pins or execution authority.
    tool_routing: run::tool_exposure::ToolRouting,
    lean_tools: bool,
    /// `[assistance.local]` escalation reached this session (see `local_profile`).
    local_escalation: model::local_profile::Escalation,
    /// `[assistance.local]` per-session server facts, phase state and unsaved evidence.
    local_session: model::local_profile::LocalSession,
    metrics: AgentMetrics,
    usage_scope: llm::usage::Scope,
    compaction_runtime: compact::compaction_runtime::CompactionRuntime,
    memory_controller: memory::memory_controller::Controller,
    usage_cost_base: f64,
    last_auto_thinking: Option<String>,
    workflow_metrics: std::cell::RefCell<crate::learn::workflow::Metrics>,
    efficiency_metrics: workflow::efficiency::Metrics,
    workflow_started: Instant,
    task_policy: crate::learn::task_policy::Policy,
    request_phase: crate::adaptive::State,
    repair_tracker: verify::repair_evidence::Tracker,
    repair_sequence: u64,
    task_packet: workers::task_packet::PacketCache,
    reasoning_recovery: workflow::reasoning::Recovery,
    live_tasks: workers::live_tasks::LiveTasks,
    superseded: workflow::evidence::Supersession,
    legacy_memory_migrated: bool,
    /// Live steering the server accepted for the reply in flight; journaled after that reply.
    accepted_live_steering: Vec<(Value, String)>,
    pending_tool_steering: Vec<String>,
    request_prefix: window::request_prefix::Tracker,
    /// True while this agent owns the session lifecycle: `session_start` has been fired for the
    /// session and `session_end` is due when the agent goes away.
    session_lifecycle: bool,
}

#[cfg(test)]
mod subscription_tests;
