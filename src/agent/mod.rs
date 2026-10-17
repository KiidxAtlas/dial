//! The agent runtime: one conversation, native tool calling, sessions, approvals, checkpoints,
//! compaction and (optionally) trajectory recording for the self-improvement loop.
//!
//! The same loop serves three callers: the interactive REPL and print mode (full context: memory,
//! skills, project files, persistent session) and the sealed gate (`run`: hermetic, ephemeral,
//! auto-approved, strict sandbox, nothing read from the user's environment).
pub mod agent_defs;
pub mod approval;
mod assistance;
mod auto_escalation;
pub mod calls;
mod child;
pub mod child_transcript;
mod completion;
mod constrained_repair;
mod context_meter;
mod context_notes;
mod development_copy;
mod edit_runtime;
mod engine;
pub mod exploration;
pub mod hooks;
pub mod plan_files;
mod provider_slots;
mod request_prefix;
mod task_slots;
mod tool_compat;
mod tool_runner;
mod turn;
pub mod ui_event;
mod worker_resume;
pub use completion::TurnOutcome;
pub mod compaction;
pub mod compaction_runtime;
pub mod context;
mod efficiency;
mod evidence;
pub mod guard;
mod investigation;
pub(crate) mod isolation;
pub mod laya;
mod live_tasks;
mod local_profile;
pub(crate) mod mailbox;
mod memory_capture;
mod memory_controller;
pub mod modes;
mod mutation_cycles;
pub mod output_schema;
mod progress;
pub mod prompt;
mod prompt_template;
pub mod questions;
mod reasoning;
mod recovery_checkpoint;
pub(crate) mod repair_evidence;
mod reviewworkflow;
mod rules;
mod security_tool;
pub mod snapcompact;
pub mod solve;
mod state_ui;
mod stream_guards;
pub mod subagents;
mod task_packet;
pub mod todo;
mod todo_recovery;
mod training;
mod turn_recovery;
mod verification;
mod verification_receipts;
#[cfg(test)]
mod verification_state_tests;
mod verified_examples;
mod visual;
pub use live_tasks::LiveTaskControl;
pub(crate) use verification::{plan_with_runner, related_tests_with_runner};

// Agent impl and helpers split out of this file by concern; private items are glob-imported so
// sibling modules keep resolving them through `super::`.
mod accessors;
mod backend;
mod call_summary;
mod chat;
mod checks;
mod compaction_ops;
mod delegation;
mod dispatch;
mod helpers;
mod knowledge;
mod lifecycle;
mod projection;
mod repair;
mod roles;
mod route;
mod selfplay;
mod steering;
mod surface;
#[cfg(test)]
mod tests;
mod tool_exposure;
mod turn_loop;

pub use backend::{Runtime, make_backend, make_session_backend};
pub use call_summary::{legacy_tool_presentation, legacy_tool_result, summarize_call};
pub use helpers::{json_text, truncate};
pub use selfplay::{check_case, check_case_measured, selfplay_run};
pub use surface::{Decision, NullUi, RequestPhase, Ui};

use backend::*;
use call_summary::*;
use chat::*;
use dispatch::*;
use helpers::*;
use repair::*;
use roles::*;
use selfplay::*;
use steering::*;

use crate::config::paths::Layout;
use crate::config::types::{Approval, Config, Event, SandboxMode, Version};
use crate::learn::verify::Check;
use crate::learn::worker;
use crate::llm::provider::{Backend, ProviderKind};
use crate::llm::{self, Chat, ToolCall};
use crate::storage::checkpoint::Checkpoints;
use crate::storage::codebase::Codebase;
use crate::storage::provenance::ProvenanceMemory;
use crate::storage::session::{self, Session};
use crate::storage::skills;
use crate::storage::store::Store;
use crate::tools::ext::{LifecycleEvent, Registry, ToolContext};
use crate::tools::{self, ToolOutput, Workspace};
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
use todo::TodoList;

pub struct TurnReport {
    pub text: String,
    pub outcome: TurnOutcome,
    pub steps: u32,
    pub interrupted: bool,
    pub trajectory: Option<i64>,
}
impl engine::Terminal for TurnReport {
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
    context: Vec<context::ContextFile>,
    skills: String,
    memory: String,
    system_template: Option<prompt_template::Template>,
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
    pub reasoning_interceptions: u64,
    pub turns_completed: u64,
}

/// Per-turn bookkeeping for what `todo done` checks at step boundaries.
#[derive(Default)]
struct StepChecks {
    /// Consultation allowance and failure ledger span all repair reentries in this user turn.
    auto_escalation: auto_escalation::AutoEscalation,
    visual: visual::Review,
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
    /// Exact configured checks whose most recent run failed; retain them across new edits.
    failed_checks: Vec<verification::FocusedCheck>,
    failed: bool,
}
impl StepChecks {
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
    fn include_failed_checks(&mut self, root: &Path, checks: &mut Vec<verification::FocusedCheck>) {
        // A Python syntax obligation for a file renamed or deleted since the failure is obsolete.
        self.failed_checks
            .retain(|failed| !verification::obsolete_check(root, failed));
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
    route: route::ActiveRoute,
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
    history: Option<Arc<crate::storage::history::ChangeHistory>>,
    experience: Option<Arc<crate::storage::experience::Experience>>,
    assistance: assistance::State,
    checkpoints: Option<Arc<Checkpoints>>,
    store: Option<&'a Store>,
    version: Version,
    system: String,
    system_messages: Vec<Value>,
    agents_messages: Vec<Value>,
    parts: PromptParts,
    approval: Approval,
    always: Vec<approval::PermissionRule>,
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
    pending_context_rollover: Option<context_notes::NewContextRequest>,
    exploration: exploration::Lifecycle,
    modes: modes::State,
    context_notes_reminder_boundary: Option<String>,
    /// Side-request cache namespace of the maintenance run in progress, so every summary chunk of
    /// one compaction shares a cache identity instead of minting a fresh one per call.
    active_side_namespace: Option<String>,
    context_rule_catalog: Option<Arc<rules::Catalog>>,
    interrupt: Arc<AtomicBool>,
    hermetic: bool,
    /// Overrides the trajectory domain (self-play records as `selfplay:<family>`).
    domain_override: Option<String>,
    trajectory: Option<(i64, String, String)>,
    last_trajectory: Option<i64>,
    /// Actual inference requests paired with eligible targets from the current turn.
    training_capture: training::Capture,
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
    context_meter: context_meter::ContextMeter,
    /// How many conversation messages the request behind `last_prompt_tokens` carried.
    reported_messages: usize,
    step_probe: Option<crate::learn::probe::StepProbe>,
    enabled_tools: HashSet<String>,
    lean_tools: bool,
    /// `[assistance.local]` escalation reached this session (see `local_profile`).
    local_escalation: local_profile::Escalation,
    /// `[assistance.local]` per-session server facts, phase state and unsaved evidence.
    local_session: local_profile::LocalSession,
    metrics: AgentMetrics,
    usage_scope: llm::usage::Scope,
    compaction_runtime: compaction_runtime::CompactionRuntime,
    memory_controller: memory_controller::Controller,
    usage_cost_base: f64,
    last_auto_thinking: Option<String>,
    workflow_metrics: std::cell::RefCell<crate::learn::workflow::Metrics>,
    efficiency_metrics: efficiency::Metrics,
    workflow_started: Instant,
    task_policy: crate::learn::task_policy::Policy,
    request_phase: crate::adaptive::State,
    repair_tracker: repair_evidence::Tracker,
    repair_sequence: u64,
    task_packet: task_packet::PacketCache,
    reasoning_recovery: reasoning::Recovery,
    live_tasks: live_tasks::LiveTasks,
    superseded: evidence::Supersession,
    legacy_memory_migrated: bool,
    /// Live steering the server accepted for the reply in flight; journaled after that reply.
    accepted_live_steering: Vec<(Value, String)>,
    pending_tool_steering: Vec<String>,
    request_prefix: request_prefix::Tracker,
    /// True while this agent owns the session lifecycle: `session_start` has been fired for the
    /// session and `session_end` is due when the agent goes away.
    session_lifecycle: bool,
}
