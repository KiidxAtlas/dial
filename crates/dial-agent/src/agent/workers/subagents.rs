//! Typed, parallel subagents behind the `task` tool.
//!
//! A subagent is an independent run: its own system prompt, its own throwaway conversation, its own
//! tool allowlist and (for remote endpoints) its own backend, so several can work at once. It shares
//! only the workspace, the extension tools and the interrupt flag with its parent, and nothing it
//! says becomes the parent's training data. Subagents cannot start subagents.
//!
//! Agent types are fixed here ([`AGENTS`]). A `[subagents]` config preset is a *custom scout*: the
//! preset text is appended to the scout prompt and selected with `agent = "<preset>"`. Presets may
//! not reuse a type name (config validation enforces it), so `agent` is never ambiguous.
//!
//! Scheduling: read-only tasks and isolated (`isolation = "copy"`) workers of a batch run concurrently
//! when their role's model is a remote endpoint. A local model server is a single shared resource, so
//! everything runs in order there. In-place workers (which write the live workspace) always run one at
//! a time after those, so two writers never race on the workspace. Isolated copies need no such
//! ordering: each works in its own directory and merges back under the workspace's serial lock with
//! per-file conflict checks. Approval prompts from concurrent children are funnelled
//! to the calling thread, the only one that owns the [`Ui`].
mod approver;
mod brief;
mod handoff;
mod launch;
mod outcome;
mod parse;
mod rebase;
mod remote;
mod routes;
mod schema;
pub(in crate::agent) mod seance;

use approver::Direct;
pub(in crate::agent) use approver::{Approver, recheck_acceptance};
pub(in crate::agent) use launch::{
    Parent, nested, preload_skills, run, run_child, spawn_policy, update_model_route,
    with_file_agents,
};
use launch::{announce_end, announce_start, finish_worker, worker_body};
pub(in crate::agent) use outcome::{ChildConversation, MutationScope, Outcome, Status};
pub(in crate::agent) use parse::{Request, check_isolation_backends, parse};
pub(in crate::agent) use rebase::{Rebase, rebase_text};
use remote::{Message, Remote};
pub(in crate::agent) use routes::ModelRoute;
use routes::{model_routes, role_for_agent};
pub(in crate::agent) use schema::nested_schema;
pub use schema::schema;

use crate::config::subagents::AgentSpec;
use crate::runtime::tool_output::ToolOutput;

use crate::agent::{
    Ui, make_backend,
    safety::approval::{self, Denial},
    window::prompt,
};
use crate::{
    config::paths::Layout,
    config::types::{Config, Harness},
    llm::provider::Backend,
    tools::{Workspace, integrations::ext::Registry},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

/// Most tasks one `task` call may launch.
pub const MAX_BATCH: usize = 64;
/// `isolation_backend` values: the `[isolation] backend` names, for one task.
const ISOLATION_BACKENDS: [&str; 6] = ["auto", "clone", "overlay", "worktree", "copy", "projfs"];

/// Whether a `task` call would launch an agent that can change files, so the call needs approval.
pub fn launches_worker(args: &Value) -> bool {
    if args["action"] == "resume" {
        return true;
    }
    // Built-in names are fixed; any other name consults the process-wide writer registry (fail-closed).
    let mutating = |entry: &Value| {
        entry
            .get("agent")
            .and_then(Value::as_str)
            .is_some_and(crate::agent::base::agent_defs::launches_writer)
    };
    mutating(args)
        || args
            .get("tasks")
            .and_then(Value::as_array)
            .is_some_and(|tasks| tasks.iter().any(mutating))
}

/// The tools offered to an agent of this type: its allowlist as enabled by the harness, plus the
/// extension tools it may use. Read-only agents get only the extension tools that need no approval;
/// a worker gets all of them (each call still goes through approval).
pub fn toolset(spec: &AgentSpec, enabled: &[String], ext: &Registry) -> Vec<String> {
    if enabled.is_empty() {
        return Vec::new();
    }
    let own = spec
        .tools
        .iter()
        .filter(|tool| enabled.iter().any(|e| e == **tool))
        .map(|tool| (*tool).to_owned());
    // A consultant answers from the quoted record and the files it can read: extension, artifact
    // and other tools are withheld whatever the harness enabled.
    if spec.name == "seance" {
        return own.collect();
    }
    // Read-only agents get recall and code-intelligence extensions (codebase, memory, `find`,
    // `dep_source`, `image_search`, `lsp`, read-only `git`, `ast_search`, `code_history`), not
    // external side-effect integrations such as browsers, even when a probe needs no approval.
    // Every actual call by a read-only child is still gated by `approval::needs_approval` in `child.rs`.
    let extra = ext
        .names()
        .filter(|name| {
            let probe = if *name == "git" {
                json!({"action": "status"})
            } else if let Some(probe) = crate::tools::surface::facade::probe(name) {
                probe
            } else {
                json!({})
            };
            enabled.iter().any(|n| n == *name)
                && (spec.mutates
                    || (approval::is_read_only(ext, name, &probe)
                        && (matches!(
                            *name,
                            "find"
                                | "dep_source"
                                | "image_search"
                                | "lsp"
                                | "git"
                                | "ast_search"
                                | "code_history"
                                | "history"
                                | "inspect"
                        ) || !crate::config::tools::integrations::INTEGRATION_TOOLS
                            .contains(name))))
        })
        .map(str::to_owned);
    own.chain(extra).collect()
}

/// The call as the user sees it in approval prompts and tool lines.
pub fn summarize(args: &Value) -> String {
    let line = |entry: &Value| {
        format!(
            "{}: {}",
            entry["agent"].as_str().unwrap_or("scout"),
            entry["task"].as_str().unwrap_or("")
        )
    };
    match args.get("tasks").and_then(Value::as_array) {
        Some(tasks) => tasks.iter().map(line).collect::<Vec<_>>().join("\n"),
        None => line(args),
    }
}

/// Cut `text` to at most `max` bytes on a character boundary, marking the cut.
pub(in crate::agent) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    format!(
        "{} [cut]",
        crate::agent::base::helpers::byte_prefix(text, max)
    )
}

/// User-authored scope and acceptance conditions for a delegated job.
#[derive(Debug, Clone)]
pub(in crate::agent) struct WorkPacket {
    pub(in crate::agent) scope: String,
    pub(in crate::agent) interface: String,
    pub(in crate::agent) acceptance: Vec<String>,
}

/// Parent state a child receives with its assignment, captured when the `task` call starts.
#[derive(Clone, Default)]
pub struct Handoff {
    /// The parent's current request, bounded: the assignment context the worker serves.
    pub current_instruction: Option<String>,
    /// The session's first user goal, bounded. Historical context only once a newer request exists.
    pub goal: Option<String>,
    /// Open todo items (in progress first), as `[status] content` lines.
    pub todos: Vec<String>,
    /// The parent's durable working state as compact JSON, bounded.
    pub working_state: Option<String>,
    /// Admitted original user messages; historical context, not inferred active scope.
    pub user_intent: Option<String>,
}

const MAX_HANDOFF_GOAL: usize = 4 * 1024;
const MAX_HANDOFF_STATE: usize = 2 * 1024;
const MAX_HANDOFF_TODOS: usize = 12;

impl Handoff {
    /// Bounded snapshot of the parent's goal, unfinished todos and working state.
    pub fn capture(control: &Value, tasks: &[crate::agent::workflow::todo::Task]) -> Self {
        use crate::agent::workflow::todo::Status;
        let mut open: Vec<&crate::agent::workflow::todo::Task> = tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.status,
                    Status::Pending | Status::InProgress | Status::Blocked
                )
            })
            .collect();
        open.sort_by_key(|task| task.status != Status::InProgress);
        let todos = open
            .into_iter()
            .take(MAX_HANDOFF_TODOS)
            .map(|task| {
                let status = match task.status {
                    Status::InProgress => "in_progress",
                    Status::Blocked => "blocked",
                    _ => "pending",
                };
                format!("[{status}] {}", prompt::truncate_chars(&task.content, 300))
            })
            .collect();
        let state = &control["working_state"];
        Self {
            current_instruction: control["current_instruction"]
                .as_str()
                .map(|text| bounded_instruction(text, MAX_HANDOFF_GOAL)),
            goal: control["original_goal"]
                .as_str()
                .map(|goal| bounded_instruction(goal, MAX_HANDOFF_GOAL)),
            todos,
            working_state: (!state.is_null())
                .then(|| handoff::pack_state(state, MAX_HANDOFF_STATE)),
            user_intent: handoff::intent_trail(control, MAX_HANDOFF_GOAL)
                .map(|intent| handoff::pack_state(&intent, MAX_HANDOFF_STATE)),
        }
    }

    /// Fresh runs receive current parent background; resumed conversations keep their saved brief.
    pub(in crate::agent) fn apply(&self, jobs: &mut [Job<'_>]) {
        for job in jobs {
            // A consultant answers about the source record, not the parent's current request.
            if job.seance.is_none() && job.resume_tools.is_none() && job.resume_messages.is_empty()
            {
                job.handoff = self.clone();
            }
        }
    }
}

fn bounded_instruction(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    format!(
        "{}\n[CRITICAL HANDOFF OVERFLOW: current request exceeds snapshot budget; ask parent for complete request before acting on omitted constraints.]",
        clip(text, budget)
    )
}

pub(in crate::agent) fn result_projection(
    report: &str,
    payload: Option<&Value>,
    acceptance: &[String],
    workspace: Option<&Workspace>,
) -> Value {
    handoff::project(report, payload, acceptance, workspace)
}

/// A complete report must never be advertised through a silently elided artifact.
pub(in crate::agent) fn retain_report(
    store: &crate::storage::artifact::ArtifactStore,
    data: &Value,
) -> anyhow::Result<String> {
    use std::io::Write;
    let mut spool = store.spool()?;
    serde_json::to_writer_pretty(&mut spool, data)?;
    spool.flush()?;
    anyhow::ensure!(
        spool.elided_bytes() == 0,
        "complete task report exceeds artifact retention limit"
    );
    spool.finish()
}

/// One validated unit of work.
pub(in crate::agent) struct Job<'c> {
    pub index: usize,
    pub depth: u32,
    pub resume_messages: Vec<Value>,
    pub resume_tools: Option<Vec<String>>,
    pub label: String,
    /// Name shown to the user and the model: the agent type, or the preset that customised a scout.
    pub kind: &'c str,
    pub spec: &'static AgentSpec,
    /// Model role this job runs on, chosen by agent type (see `role_for_agent`).
    pub role: &'static str,
    pub model_routes: Vec<ModelRoute>,
    preset: Option<&'c str>,
    /// Custom agents may only narrow their base agent's tools.
    pub allowed_tools: Option<&'c [String]>,
    pub prompt: String,
    /// How open-ended the problem is, required per item.
    pub solution_space: String,
    /// Shared batch `context`, passed to the child with its assignment.
    pub shared_context: Option<String>,
    /// Parent goal, open todos and working state, captured when the `task` call starts.
    pub handoff: Handoff,
    pub max_steps: u32,
    pub schema: Option<Value>,
    /// Runs against a private project copy that is merged back afterwards.
    pub isolated: bool,
    /// Backend this task asked for with `isolation_backend`; `None` uses `[isolation] backend`.
    pub isolation_backend: Option<crate::config::isolation::Preference>,
    /// Largest answer (bytes) this task may return.
    pub limit: usize,
    pub(in crate::agent) work_packet: Option<WorkPacket>,
    pub acceptance_checks: Vec<String>,
    pub(in crate::agent) promotion: Option<crate::agent::workers::contracts::Promotion>,
    /// Skills the agent definition autoloads; `preloaded` holds their rendered bodies once resolved.
    pub autoload_skills: Vec<String>,
    pub preloaded: String,
    /// Set for `agent: seance`: the source it consults and the model it runs on.
    pub(in crate::agent) seance: Option<seance::Seance>,
}

// ----------------------------------------------------------------------------------- execution

/// What a subagent may touch, shared by every child of one `task` call.
#[derive(Clone, Copy)]
pub(in crate::agent) struct Env<'a> {
    pub config: &'a Config,
    pub workspace: &'a Workspace,
    pub ext: &'a Registry,
    pub layout: Option<&'a Layout>,
    pub harness: &'a Harness,
    pub interrupt: &'a std::sync::Arc<AtomicBool>,
    pub usage: Option<&'a crate::llm::usage::Scope>,
    pub context_cap: Option<u32>,
    pub memory: Option<&'a Arc<crate::storage::memory_backend::MemoryRuntime>>,
    pub plan_mode: bool,
    pub agents_messages: &'a [Value],
}

/// Every child owns fresh mutable native-tool sessions, whether live, read-only,
/// nonisolated or copied. Copy recovery/merge metadata never changes schema data.
pub(in crate::agent) fn run_job(
    env: Env,
    job: &Job,
    approver: &mut dyn Approver,
    body: impl FnOnce(Env, &mut dyn Approver) -> Outcome,
) -> Outcome {
    let Some(_slot) = crate::agent::workers::task_slots::acquire(env.config, env.interrupt) else {
        return if env.interrupt.load(Ordering::SeqCst) {
            Outcome::interrupted(0)
        } else {
            Outcome::incomplete(
                0,
                "Worker concurrency capacity is full; wait for active tasks before retrying this unstarted assignment.",
            )
        };
    };
    let registry = env.ext.fork_session();
    if !job.isolated {
        let owner_env = Env {
            ext: &registry,
            ..env
        };
        let mut outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(owner_env, approver)))
                .unwrap_or_else(|_| Outcome::failed(0, "subagent worker panicked"));
        outcome.acceptance = crate::agent::verify::worker_acceptance::Acceptance::unverified(
            "advisory worker output is untrusted; execution completion is not acceptance",
        );
        return outcome;
    }
    let isolated = match crate::agent::workers::isolation::Isolated::with_config(
        env.workspace,
        &env.config.task,
        job.isolation_backend
            .unwrap_or(env.config.isolation.backend),
    ) {
        Ok(isolated) => isolated,
        Err(error) => {
            return Outcome::failed(0, format!("could not create an isolated copy: {error:#}"));
        }
    };
    let copy_env = Env {
        workspace: isolated.workspace(),
        ext: &registry,
        ..env
    };
    let mut rebase = Rebase {
        descendant: None,
        inner: approver,
        from: isolated.root(),
        to: env.workspace.root(),
    };
    // Catch the worker while its copy is still owned, so a panic cannot discard edits.
    let mut outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(copy_env, &mut rebase)))
            .unwrap_or_else(|_| Outcome::failed(0, "subagent worker panicked"));
    // Stop owned processes before inspecting changes: no background writer/client
    // may keep modifying the copy while it is being merged or retained for recovery.
    drop(registry);
    if env.interrupt.load(Ordering::SeqCst)
        && outcome.status.phase == crate::runtime::Phase::Completed
    {
        outcome.status = Status::interrupted();
    }
    outcome.acceptance = if outcome.status.phase == crate::runtime::Phase::Completed
        && !job.acceptance_checks.is_empty()
    {
        crate::agent::verify::worker_acceptance::evaluate(
            Env {
                workspace: isolated.workspace(),
                ..env
            },
            &job.acceptance_checks,
            approver,
        )
    } else {
        crate::agent::verify::worker_acceptance::Acceptance::unverified(
            "no completed configured acceptance contract",
        )
    };
    let isolation = match outcome.status.phase {
        crate::runtime::Phase::Completed
            if !job.acceptance_checks.is_empty()
                && outcome.acceptance.state
                    != crate::agent::verify::worker_acceptance::State::Accepted =>
        {
            isolated.recover("controller acceptance checks did not authorize promotion")
        }
        crate::runtime::Phase::Completed => {
            // Acquire ledger authority only inside the workspace transaction. Retain it through
            // both validation calls and any rollback so a controller update cannot race commit.
            let mut authority = None;
            isolated.merge_checked(env.workspace, env.interrupt, |workspace| {
                if let Some(promotion) = &job.promotion {
                    authority.get_or_insert_with(|| promotion.lock())(
                        env.workspace,
                        &env.config.verification,
                    )?;
                }
                if job.acceptance_checks.is_empty() {
                    return Ok(());
                }
                outcome
                    .acceptance
                    .revalidate(workspace, &env.config.verification);
                anyhow::ensure!(
                    outcome.acceptance.state
                        == crate::agent::verify::worker_acceptance::State::Accepted,
                    "candidate or integrated parent inputs invalidated acceptance"
                );
                Ok(())
            })
        }
        crate::runtime::Phase::Interrupted => {
            let mut recovery = isolated.recover("worker interrupted");
            recovery.interrupted = true;
            recovery
        }
        state => isolated.recover(&format!("worker {}", state.label())),
    };
    if isolation.interrupted {
        outcome.status = Status::interrupted();
    } else if outcome.status.phase == crate::runtime::Phase::Completed {
        if !isolation.errors.is_empty() {
            outcome.status = Status::failed(format!(
                "worker computation completed, but isolated integration encountered merge errors; see isolation metadata\n\nWorker answer:\n{}",
                outcome.status.text
            ));
        } else if !isolation.conflicts.is_empty() {
            outcome.status = Status::incomplete(format!(
                "worker computation completed, but isolated integration is incomplete due to parent conflicts; see isolation metadata and recovery_location\n\nWorker answer:\n{}",
                outcome.status.text
            ));
        }
    }
    if !isolation.errors.is_empty() || !isolation.conflicts.is_empty() || isolation.interrupted {
        outcome.acceptance.integration_failed(&isolation);
    } else {
        outcome
            .acceptance
            .revalidate(env.workspace, &env.config.verification);
    }
    approver.notice(&isolation.describe());
    outcome.isolation = Some(isolation);
    outcome
}

/// Run `job` on a fresh backend for the role; a crash in the child becomes a failed outcome.
fn run_isolated(env: Env, role: &Config, job: &Job, approver: &mut dyn Approver) -> Outcome {
    let attempt =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match make_backend(role) {
            Ok(mut backend) => run_job(env, job, approver, |env, approver| {
                crate::agent::model::local_profile::configure_worker_backend(
                    role,
                    &mut backend,
                    job.index,
                );
                run_child(env, job, &mut backend, None, approver)
            }),
            Err(error) => Outcome::failed(0, format!("subagent provider unavailable: {error:#}")),
        }));
    attempt.unwrap_or_else(|_| Outcome::failed(0, "subagent crashed unexpectedly"))
}

/// Generate patches in caller-owned copies without automatic worker merge. The caller owns
/// scope checks, verification and promotion. Local inference keeps the ordinary serial lane.
pub(in crate::agent) fn run_copies(
    parent: &mut Parent<'_>,
    env: Env<'_>,
    workspaces: &[Workspace],
    prompts: &[String],
    limit: usize,
    ui: &mut dyn Ui,
) -> Result<Vec<ToolOutput>, String> {
    let tasks: Vec<Value> = prompts.iter().map(|prompt| json!({"agent":"worker","task":prompt,"solution_space":"one implementation slice: the spec is given","max_steps":env.config.solve.max_steps})).collect();
    let request = parse(&json!({"tasks":tasks}), env.config)?;
    if request.jobs.len() != workspaces.len() {
        return Err("candidate workspace count mismatch".into());
    }
    let jobs: Vec<(&Job, Env)> = request
        .jobs
        .iter()
        .zip(workspaces)
        .map(|(job, workspace)| (job, Env { workspace, ..env }))
        .collect();
    let mut outcomes: Vec<Option<Outcome>> = request.jobs.iter().map(|_| None).collect();
    run_concurrent(
        parent.env,
        &jobs,
        limit.min(env.config.task.max_concurrency.max(1)),
        Direct {
            approval: parent.approval,
            always: &mut *parent.always,
            root: parent.env.workspace.root(),
            extra_roots: parent.env.workspace.additional_roots(),
            layout: parent.env.layout,
            plan_mode: parent.env.plan_mode,
            cost: &mut *parent.cost,
            ui,
        },
        jobs.len(),
        &mut outcomes,
    );
    Ok(outcomes
        .into_iter()
        .map(|outcome| {
            let outcome = outcome.unwrap_or_else(|| Outcome::failed(0, "candidate did not report"));
            ToolOutput {
                text: outcome.status.text,
                is_error: outcome.status.phase != crate::runtime::Phase::Completed,
                data: None,
                execution: Default::default(),
            }
        })
        .collect())
}

/// Run the concurrent jobs on at most `limit` threads, refilling each freed thread with the next job
/// as it finishes. The calling thread stays here to answer their approval requests and report
/// progress, because only it may use the UI.
fn run_concurrent(
    env: Env,
    jobs: &[(&Job, Env)],
    limit: usize,
    mut direct: Direct,
    total: usize,
    outcomes: &mut [Option<Outcome>],
) {
    if jobs.is_empty() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let refs: Vec<&Job> = jobs.iter().map(|&(job, _)| job).collect();
    for job in &refs {
        announce_start(direct.ui, job, total);
    }
    // Every job owns a report, so the channel disconnects only after each job has reported.
    let items: Vec<(&Job, Env, Option<crate::runtime::Body>, Report)> = jobs
        .iter()
        .map(|&(job, env)| {
            let report = Report::new(job.index, tx.clone());
            (job, env, worker_body(direct.ui, job, env.config), report)
        })
        .collect();
    drop(tx);
    let controller_root = env.workspace.root();
    rolling(
        items,
        limit,
        |(job, env, body, report)| {
            let mut remote = Remote(
                report.tx.clone(),
                body.clone(),
                job.index,
                env.interrupt.clone(),
            );
            let outcome = run_isolated(
                env,
                env.config,
                job,
                &mut Rebase {
                    descendant: None,
                    inner: &mut remote,
                    from: env.workspace.root(),
                    to: controller_root,
                },
            );
            finish_worker(&body, &outcome);
            report.finish(outcome);
        },
        || pump_messages(&rx, env, &refs, &mut direct, outcomes),
    );
}

/// A job's channel sender. Its outcome is reported exactly once: `finish` sends the real one. If the
/// job is dropped without reporting (it panicked), the drop sends a failure instead, so the pump hears
/// about every job and disconnects once they have all ended.
struct Report {
    index: usize,
    tx: mpsc::Sender<Message>,
    reported: bool,
}

impl Report {
    fn new(index: usize, tx: mpsc::Sender<Message>) -> Self {
        Self {
            index,
            tx,
            reported: false,
        }
    }

    fn finish(mut self, outcome: Outcome) {
        self.reported = true;
        let _ = self.tx.send(Message::Done(self.index, Box::new(outcome)));
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.reported {
            let _ = self.tx.send(Message::Done(
                self.index,
                Box::new(Outcome::failed(
                    0,
                    "subagent worker panicked before reporting its outcome",
                )),
            ));
        }
    }
}

/// Runs `work` over `items` on at most `limit` scoped threads. A thread takes the next unstarted item
/// as soon as it finishes one, so a slow item never idles the other slots. `pump` runs on the calling
/// thread while the workers are alive. A panic in `work` is contained so the queue keeps draining and
/// `pump` is released; the first such panic is resumed on the calling thread once everything has ended.
pub(in crate::agent) fn rolling<T: Send>(
    items: Vec<T>,
    limit: usize,
    work: impl Fn(T) + Sync,
    pump: impl FnOnce(),
) {
    let workers = limit.max(1).min(items.len());
    let queue = std::sync::Mutex::new(items.into_iter());
    let panicked: std::sync::Mutex<Option<Box<dyn std::any::Any + Send>>> =
        std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next();
                    let Some(item) = next else { break };
                    let attempt =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(item)));
                    if let Err(payload) = attempt {
                        panicked
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert(payload);
                    }
                }
            });
        }
        pump();
    });
    let first = panicked
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(payload) = first {
        std::panic::resume_unwind(payload);
    }
}

/// The UI owner must keep ticking even when every child is silent or blocked in network/tool I/O.
fn pump_messages(
    rx: &mpsc::Receiver<Message>,
    env: Env,
    jobs: &[&Job],
    direct: &mut Direct,
    outcomes: &mut [Option<Outcome>],
) {
    // Weak handles cannot keep an owned process alive after its registry is dropped
    // for copy merge/recovery. They are upgraded only to cancel the active operation.
    let mut active: BTreeMap<(usize, String), Weak<dyn crate::tools::integrations::ext::Tool>> =
        BTreeMap::new();
    loop {
        let message = rx.recv_timeout(std::time::Duration::from_millis(100));
        direct.ui.heartbeat();
        if env.interrupt.load(Ordering::SeqCst) {
            for (_, tool) in std::mem::take(&mut active) {
                if let Some(tool) = tool.upgrade() {
                    tool.cancel_operation();
                }
            }
        }
        match message {
            Ok(Message::CapturePolicy { cancel, reply }) => {
                let result = if env.interrupt.load(Ordering::SeqCst) {
                    Err("memory capture cancelled".into())
                } else {
                    direct.memory_capture_allowed(&cancel)
                };
                let _ = reply.send(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Message::Policy(call, explanation)) => {
                direct.policy_explanation(&call, &explanation)
            }
            Ok(Message::Notice(message)) => direct.notice(&message),
            Ok(Message::Cost(cost)) => direct.add_cost(cost),
            Ok(Message::ToolStarted(index, call_id, tool)) => {
                if env.interrupt.load(Ordering::SeqCst) {
                    if let Some(tool) = tool.upgrade() {
                        tool.cancel_operation();
                    }
                } else {
                    active.insert((index, call_id), tool);
                }
            }
            Ok(Message::ToolFinished(index, call_id)) => {
                active.remove(&(index, call_id));
            }
            Ok(Message::Question {
                request,
                cancel,
                reply,
            }) => {
                let outcome =
                    if env.interrupt.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                        crate::agent::workflow::questions::QuestionOutcome::Cancelled
                    } else {
                        direct.ask_questions(&request, &cancel)
                    };
                let _ = reply.send(outcome);
            }
            Ok(Message::Approve {
                call,
                summary,
                needs_approval,
                provenance,
                cancel,
                reply,
            }) => {
                let prior_rules = direct.always.len();
                let mut decision =
                    if env.interrupt.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                        Err(Denial::User)
                    } else {
                        direct.authorize_from(&call, &summary, needs_approval, provenance, &cancel)
                    };
                if env.interrupt.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                    cancel.store(true, Ordering::SeqCst);
                    direct.always.truncate(prior_rules);
                    approval::explain(direct.ui, &call, approval::PolicyReason::Cancelled);
                    decision = Err(Denial::User);
                }
                let _ = reply.send(decision);
            }
            Ok(Message::NativeMutation {
                original,
                tool,
                paths,
                unbounded,
                scope,
                cancel,
                reply,
            }) => {
                let mut result = if env.interrupt.load(Ordering::SeqCst)
                    || cancel.load(Ordering::SeqCst)
                {
                    Err("native mutation cancelled".into())
                } else {
                    direct.authorize_mutation(&original, &tool, &paths, unbounded, &cancel, scope)
                };
                if env.interrupt.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                    cancel.store(true, Ordering::SeqCst);
                    result = Err("native mutation cancelled".into());
                }
                let _ = reply.send(result);
            }
            Ok(Message::Done(index, outcome)) => {
                active.retain(|(job, _), _| *job != index);
                if let Some(job) = jobs.iter().find(|job| job.index == index) {
                    announce_end(direct.ui, job, &outcome);
                }
                outcomes[index] = Some(*outcome);
            }
        }
    }
}

/// Where sequential children get their backend.
struct Lanes<'a> {
    main: &'a mut Backend,
    side: &'a mut Option<(String, Backend)>,
    adapter: Option<&'a Path>,
    /// The job's role is served by a local model (shared resource), not a remote endpoint.
    shared: bool,
    /// The role has its own assigned model, so a local one is swapped in for its batch.
    assigned: bool,
}

/// The local backend that serves `role`'s model: the cached one when it serves that model, else a
/// fresh load. A cached backend for another model is released first.
fn take_own_backend(
    side: &mut Option<(String, Backend)>,
    role: &Config,
) -> anyhow::Result<(String, Backend)> {
    let key = serde_json::to_string(&(
        role.selection(),
        role.active_model_profile(),
        role.context_window,
    ))?;
    if let Some((cached, mut backend)) = side.take() {
        if cached == key {
            return Ok((cached, backend));
        }
        backend.release();
    }
    Ok((key, make_backend(role)?))
}

fn run_serial(
    env: Env,
    role: &Config,
    jobs: &[&Job],
    lanes: Lanes,
    mut direct: Direct,
    total: usize,
    outcomes: &mut [Option<Outcome>],
) {
    let Lanes {
        main,
        side,
        adapter,
        shared,
        assigned,
    } = lanes;
    // A local role on its own model is swapped in (freeing the main model's memory) for the batch.
    let mut own = if shared && assigned {
        match take_own_backend(side, role) {
            Ok(entry) => {
                main.release();
                Some(entry)
            }
            Err(error) => {
                for job in jobs {
                    outcomes[job.index] = Some(Outcome::failed(
                        0,
                        format!("subagent provider unavailable: {error:#}"),
                    ));
                }
                return;
            }
        }
    } else {
        None
    };
    for &job in jobs {
        if env.interrupt.load(Ordering::SeqCst) {
            outcomes[job.index] = Some(Outcome::interrupted(0));
            continue;
        }
        announce_start(direct.ui, job, total);
        let body = worker_body(direct.ui, job, role);
        let (tx, rx) = mpsc::channel();
        // The controller waits on this one child, so the child runs under the slot of the worker
        // that is delegating (if any) instead of needing a second one at a full ceiling.
        let lineage = crate::agent::workers::task_slots::lineage();
        std::thread::scope(|scope| {
            let own = &mut own;
            let main = &mut *main;
            scope.spawn(move || {
                let _borrowed = lineage.enter();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut remote =
                        Remote(tx.clone(), body.clone(), job.index, env.interrupt.clone());
                    match (own, shared) {
                        (Some((_, backend)), _) => {
                            run_job(env, job, &mut remote, |env, approver| {
                                run_child(env, job, backend, None, approver)
                            })
                        }
                        (None, true) => run_job(env, job, &mut remote, |env, approver| {
                            run_child(env, job, main, adapter, approver)
                        }),
                        (None, false) => run_isolated(env, role, job, &mut remote),
                    }
                }));
                let outcome =
                    result.unwrap_or_else(|_| Outcome::failed(0, "subagent crashed unexpectedly"));
                finish_worker(&body, &outcome);
                let _ = tx.send(Message::Done(job.index, Box::new(outcome)));
            });
            pump_messages(&rx, env, &[job], &mut direct, outcomes);
        });
    }
    if let Some((key, mut backend)) = own {
        backend.release();
        *side = Some((key, backend));
    }
}

pub(in crate::agent) fn structured_result_note(output: &ToolOutput) -> Option<Value> {
    handoff::structured_note(output)
}

/// Completion (execution lifecycle) and controller acceptance are reported separately. Neither
/// implies that the work packet's acceptance conditions were met.
pub(super) fn candidate_status(
    completion: &str,
    acceptance: crate::agent::verify::worker_acceptance::State,
    checks: &[String],
) -> Value {
    json!({
        "completion": completion,
        "acceptance": acceptance,
        "check_owner": if checks.is_empty() { "none" } else { "controller" },
        "acceptance_checks": checks,
        "note": "completion is the worker's lifecycle; acceptance covers only the controller-run configured checks on stable inputs",
    })
}

/// A lone `prompt` returns just the answer; a batch returns one labelled section per task, in input order.
fn render(
    request: &Request,
    outcomes: Vec<Option<Outcome>>,
    models: &std::collections::BTreeMap<usize, String>,
    workspace: Option<&Workspace>,
) -> ToolOutput {
    let artifacts = workspace.and_then(Workspace::artifacts);
    let outcomes: Vec<Outcome> = outcomes
        .into_iter()
        .map(|o| o.unwrap_or_else(|| Outcome::failed(0, "subagent produced no result")))
        .collect();
    let mut data = json!({"jobs": request.jobs.iter().zip(&outcomes).map(|(job,outcome)| {
        let status = outcome.status.wire();
        let text = outcome.status.text.as_str();
        let mut result = json!({"name":job.label,"agent":job.kind,"status":status,"steps":outcome.steps,"result":outcome.data.clone().unwrap_or_else(|| json!(text))});
        result["inspections"] = json!(outcome.inspections);
        result["scheduling"] = json!(outcome.scheduling);
        result["acceptance"] = json!(outcome.acceptance);
        result["candidate"] = candidate_status(status, outcome.acceptance.state, &job.acceptance_checks);
        result["projection"] = result_projection(text, outcome.data.as_ref(), job.work_packet.as_ref().map(|p| p.acceptance.as_slice()).unwrap_or_default(), workspace);
        result["role"] = json!(job.role);
        result["model"] = json!(models.get(&job.index));
        if let Some(route) = &outcome.model_route {
            for key in ["role", "model", "model_source", "model_explanation"] { result[key] = route[key].clone(); }
        }
        if let Some(rules) = &outcome.portable_rules { result["portable_rules"] = rules.clone(); }
        if let Some(isolation) = &outcome.isolation {
            result["isolation"] = json!(isolation);
        }
        result
    }).collect::<Vec<_>>()});
    // Retain the complete typed report before any presentation clipping, as live tasks do.
    // Without storage (or on failure), keep the full typed outcome rather than lose evidence.
    let artifact = artifacts.map(|store| retain_report(store, &data));
    match &artifact {
        Some(Ok(uri)) => {
            data["artifact"] = json!(uri);
            for ((job, outcome), result) in request
                .jobs
                .iter()
                .zip(&outcomes)
                .zip(data["jobs"].as_array_mut().expect("job results"))
            {
                if job.schema.is_none()
                    && outcome.data.is_none()
                    && outcome.status.text.len() > job.limit
                {
                    result["result"] = json!(clip(&outcome.status.text, job.limit));
                    result["preview"] = json!(true);
                    result["artifact"] = json!(uri);
                }
            }
        }
        Some(Err(error)) => data["artifact_error"] = json!(format!("{error:#}")),
        None => {}
    }
    let answer = |job: &Job, outcome: &Outcome| {
        let text = &outcome.status.text;
        let mut answer = if outcome.status.phase == crate::runtime::Phase::Completed {
            if job.schema.is_some() {
                // The legacy synchronous output-schema contract includes the exact JSON text.
                // The separate typed projection above must not wrap or mutate that value.
                return Ok(text.clone());
            }
            if text.trim().is_empty() {
                Err("subagent incomplete: no report returned".into())
            } else {
                Ok(text.clone())
            }
        } else {
            Err(format!("subagent {}: {text}", outcome.status.phase.label()))
        };
        if text.len() > job.limit {
            let body = match &mut answer {
                Ok(body) | Err(body) => body,
            };
            match &artifact {
                Some(Ok(uri)) => {
                    *body = clip(body, job.limit);
                    body.push_str(&format!(
                        "\n\n[Preview only; complete task report: {uri}. Read it with read.]"
                    ));
                }
                Some(Err(error)) => body.push_str(&format!(
                    "\n\n[Artifact retention failed: {error:#}. Report above is unabridged.]"
                )),
                None => {}
            }
        }
        if let Some(isolation) = &outcome.isolation {
            let text = match &mut answer {
                Ok(text) | Err(text) => text,
            };
            text.push_str(&format!("\n\n[{}]", isolation.describe()));
        }
        match &mut answer {
            Ok(text) | Err(text) => text.push_str(&format!(
                "\n\n[Controller acceptance: {}]",
                serde_json::to_string(&outcome.acceptance).unwrap_or_default()
            )),
        }
        answer
    };
    if !request.batch {
        return match answer(&request.jobs[0], &outcomes[0]) {
            Ok(text) => ToolOutput {
                execution: Default::default(),
                data: Some(data),
                text,
                is_error: false,
            },
            Err(text) => ToolOutput {
                execution: Default::default(),
                data: Some(data),
                text,
                is_error: true,
            },
        };
    }
    let mut failed = 0;
    let mut sections = String::new();
    for (job, outcome) in request.jobs.iter().zip(&outcomes) {
        let (status, body) = match answer(job, outcome) {
            Ok(text) => ("ok", text),
            Err(text) => {
                failed += 1;
                ("failed", text)
            }
        };
        let model = outcome
            .model_route
            .as_ref()
            .and_then(|route| route["model"].as_str())
            .unwrap_or_else(|| models.get(&job.index).map_or("default", String::as_str));
        let role = outcome
            .model_route
            .as_ref()
            .and_then(|route| route["role"].as_str())
            .unwrap_or(job.role);
        sections.push_str(&format!(
            "\n## {}. {} ({}, role {}, model {model}) - {status}, {} step(s)\n{body}\n",
            job.index + 1,
            job.label,
            job.kind,
            role,
            outcome.steps
        ));
    }
    let total = outcomes.len();
    ToolOutput {
        execution: Default::default(),
        data: Some(data),
        text: format!(
            "{total} subagent task(s): {} ok, {failed} failed.\n{sections}",
            total - failed
        ),
        is_error: failed == total,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod scheduling;
