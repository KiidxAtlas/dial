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
mod handoff;

use crate::config::subagents::AGENTS;
use crate::config::subagents::AgentSpec;
use crate::config::subagents::builtin;
use crate::runtime::tool_output::ToolOutput;

use crate::agent::{
    Ui, make_backend,
    safety::approval::{self, Denial, PermissionRule},
    window::prompt,
    workers::child,
};
use crate::{
    config::paths::Layout,
    config::types::{Approval, Config, Harness},
    llm::{ToolCall, provider::Backend},
    tools::{Workspace, integrations::ext::Registry},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

/// Most tasks one `task` call may launch.
pub const MAX_BATCH: usize = 64;
/// Longest task prompt or `output_schema` (bytes).
const MAX_PROMPT: usize = 32 * 1024;
const MAX_SCHEMA: usize = 16 * 1024;
/// Longest shared batch `context` (bytes).
const MAX_CONTEXT: usize = 16 * 1024;
/// Most skills one item may preload.
const MAX_SKILLS: usize = 8;
const MAX_NAME: usize = 40;
/// Budget for all answers in one `task` result; the parent clips tool output beyond this anyway.
const RESULT_BUDGET: usize = 14_000;

/// Default specialist follows the base agent purpose, including custom definitions.
/// Implementation workers use task; an assigned specialist wins over task in model_routes.
fn role_for_agent(spec: &AgentSpec, _custom: bool) -> &'static str {
    match spec.name {
        "reviewer" | "test-runner" => "tester",
        "planner" => "planner",
        _ if spec.mutates => "task",
        _ => "small",
    }
}

/// Resolve missing task roles through configured helpers, never an arbitrary catalogue model.
/// These chains are delegation-only: maintenance requests keep their own role semantics.
fn default_task_role(config: &Config, preferred: &'static str) -> &'static str {
    let candidates: &[&str] = match preferred {
        "task" => &["task", "coach", "small"],
        "small" => &["small", "task"],
        "tester" => &["tester", "coach", "task", "small"],
        "planner" => &["planner", "coach", "task", "small"],
        _ => return preferred,
    };
    candidates
        .iter()
        .copied()
        .find(|role| config.role_selection(role).is_some())
        // Retain the requested role in diagnostics when its final fallback is Main.
        .unwrap_or(preferred)
}

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
    // Read-only agents get recall extensions (codebase, memory, `find`, `dep_source`), not
    // integrations such as browsers or archive inspection, even when those need no approval.
    let extra = ext
        .names()
        .filter(|name| {
            enabled.iter().any(|n| n == *name)
                && (spec.mutates
                    || (approval::is_read_only(ext, name, &json!({}))
                        && (matches!(*name, "find" | "dep_source")
                            || !crate::config::tools::integrations::INTEGRATION_TOOLS
                                .contains(name))))
        })
        .map(str::to_owned);
    own.chain(extra)
        .chain(
            enabled
                .iter()
                .filter(|name| name.as_str() == "artifact_read")
                .cloned(),
        )
        .collect()
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
    let end = (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    format!("{} [cut]", &text[..end])
}

/// The `task` tool as offered to the model.
pub fn schema(config: &Config) -> Value {
    let types: String = AGENTS
        .iter()
        .map(|a| format!("\n- {}: {}", a.name, a.description))
        .collect();
    let presets = if config.subagents.is_empty() {
        String::new()
    } else {
        format!(
            "\nCustom read-only presets (scout with extra instructions): {}.",
            config
                .subagents
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let presets = if config.agents.is_empty() {
        presets
    } else {
        let custom: String = config
            .agents
            .iter()
            .map(|(name, a)| {
                format!(
                    "\n- {name}: {}",
                    if a.description.is_empty() {
                        "custom read-only agent"
                    } else {
                        &a.description
                    }
                )
            })
            .collect();
        format!("{presets}\nCustom agents:{custom}")
    };
    let description = format!(
        "Delegate self-contained slices to subagents. Batch independent slices in one call: `context` is shared; each item gives `task`, `solution_space`, and optionally `skills` and `agent`. Default model-role preference chains (first configured role, otherwise the conversation model): worker task → coach → small; scout small → task; reviewer and test-runner tester → coach → task → small; planner planner → coach → task → small. Custom agents inherit their base role unless explicitly overridden. Agent types:{types}{presets}"
    );
    let mut entry_properties = json!({
        "agent":{"type":"string","description":"Agent type or preset (default scout)."},
        "task":{"type":"string","description":"Complete assignment for this item: what to do, scope and acceptance criteria. Shared background belongs in context."},
        "solution_space":{"type":"string","description":"How open this item is: fixed decisions the child must follow versus choices it may make."},
        "skills":{"type":"array","items":{"type":"string"},"description":"Skill names to load before this item starts."},
        "max_steps":{"type":"integer","minimum":0,"description":"Model step limit; default 0 means unlimited."},
        "name":{"type":"string","description":"Optional label shown to the user."},
        "output_schema":{"type":"object","description":"JSON Schema the final answer must satisfy."},
        "isolation":{"type":"string","enum":["copy"],"description":"Workers only: use a private copy and native-tool session. Successful changed files merge unless the parent changed them. Failed/interrupted edits remain in a recovery copy. The result's separate isolation metadata reports applied files, conflicts, I/O errors and recovery_location; output_schema data is never altered."}
    });
    if config.experimental.work_packet_contracts {
        entry_properties.as_object_mut().unwrap().insert("work_packet".into(), json!({
            "type":"object",
            "description":"Scoped handoff contract. The child reports evidence for each acceptance item and does not claim unrun checks.",
            "properties":{
                "scope":{"type":"string","minLength":1,"maxLength":2048},
                "interface":{"type":"string","minLength":1,"maxLength":2048},
                "acceptance":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":512}}
            },
            "required":["scope","interface","acceptance"],
            "additionalProperties":false
        }));
    }
    let entry = json!({"type":"object","properties":entry_properties,
        "required":["task","solution_space"],"additionalProperties":false});
    let mut properties = serde_json::Map::from_iter([
        (
            "action".into(),
            json!({"type":"string","enum":["run","start","resume","status","wait","cancel"],"description":"Default run waits for results and blocks you until every task finishes; for work that may take more than a few minutes, use start and then wait, so you can keep working and check progress. start returns live job IDs. resume continues a retained worker by id and message, only after its previous execution stopped safely. Use the message tool to talk to a live worker."}),
        ),
        (
            "id".into(),
            json!({"type":"string","description":"Job ID for resume/status/wait/cancel; omit status/wait ID for all jobs."}),
        ),
        (
            "message".into(),
            json!({"type":"string","description":"Steering text for resume, delivered before the resumed job's next model request."}),
        ),
        (
            "timeout".into(),
            json!({"type":"integer","minimum":0,"maximum":120,"description":"wait only: seconds to block for results before returning current status (default 30). Jobs keep running after a timeout."}),
        ),
        (
            "task".into(),
            json!({"type":"string","description":"Complete assignment for a single task: what to do, scope and acceptance criteria."}),
        ),
        (
            "solution_space".into(),
            json!({"type":"string","description":"How open a single task is: fixed decisions versus choices the child may make."}),
        ),
        (
            "context".into(),
            json!({"type":"string","description":"Shared background for every task in this call: goal, constraints, interfaces and conventions. Keep it concise and factual."}),
        ),
        (
            "skills".into(),
            json!({"type":"array","items":{"type":"string"},"description":"Skill names to load for a single task before it starts."}),
        ),
        (
            "agent".into(),
            json!({"type":"string","description":"Agent type or preset for a single task (default scout)."}),
        ),
        (
            "max_steps".into(),
            json!({"type":"integer","minimum":0,"description":"Model step limit for a single task; default 0 means unlimited."}),
        ),
        (
            "name".into(),
            json!({"type":"string","description":"Optional label for a single task."}),
        ),
        (
            "output_schema".into(),
            json!({"type":"object","description":"JSON Schema the final answer must satisfy; in a batch, the default for tasks without their own."}),
        ),
        (
            "isolation".into(),
            json!({"type":"string","enum":["copy"],"description":"Single worker task: run in an isolated project copy and merge back (see tasks[].isolation)."}),
        ),
        (
            "tasks".into(),
            json!({"type":"array","maxItems":MAX_BATCH,"items":entry,"description":"Independent tasks to run instead of `task`; each needs its own task and solution_space."}),
        ),
    ]);
    if config.experimental.work_packet_contracts {
        properties.insert("work_packet".into(), json!({
            "type":"object",
            "description":"Scoped handoff contract for this single task.",
            "properties":{
                "scope":{"type":"string","minLength":1,"maxLength":2048},
                "interface":{"type":"string","minLength":1,"maxLength":2048},
                "acceptance":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":512}}
            },
            "required":["scope","interface","acceptance"],
            "additionalProperties":false
        }));
    }
    if config.experimental.profile_matching && !config.model_profiles.is_empty() {
        let names = config.model_profiles.keys().cloned().collect::<Vec<_>>();
        properties.insert("model_profile".into(), json!({
            "type":"string",
            "enum":names,
            "description":"Explicitly override the task's configured role with this operator-configured model profile only when the assignment requests it. Omit this field to honor role assignments. No arbitrary model identifiers are accepted."
        }));
    }
    json!({"type":"function","function":{"name":"task","description":description,
        "parameters":{"type":"object","properties":properties,"additionalProperties":false}}})
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
        }
    }

    /// Fresh runs receive current parent background; resumed conversations keep their saved brief.
    pub(in crate::agent) fn apply(&self, jobs: &mut [Job<'_>]) {
        for job in jobs {
            if job.resume_tools.is_none() && job.resume_messages.is_empty() {
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
) -> Value {
    handoff::project(report, payload, acceptance)
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
    /// Largest answer (bytes) this task may return.
    pub limit: usize,
    pub(in crate::agent) work_packet: Option<WorkPacket>,
    /// Skills the agent definition autoloads; `preloaded` holds their rendered bodies once resolved.
    pub autoload_skills: Vec<String>,
    pub preloaded: String,
}

#[derive(Clone)]
pub(in crate::agent) struct ModelRoute {
    pub role: &'static str,
    pub config: Config,
    pub source: &'static str,
}

impl ModelRoute {
    pub fn metadata(&self) -> Value {
        json!({"role":self.role,"model":self.config.selection().display_name(),"model_source":self.source})
    }
}

fn model_routes(
    config: &Config,
    job: &Job,
    profile: Option<&str>,
) -> Result<Vec<ModelRoute>, String> {
    let definition = config.agents.get(job.kind);
    let selectors = definition.map(|d| d.model.as_slice()).unwrap_or_default();
    let selectors: Vec<Option<&str>> = if selectors.is_empty() || profile.is_some() {
        vec![None]
    } else {
        selectors.iter().map(|s| Some(s.as_str())).collect()
    };
    selectors
        .into_iter()
        .map(|selector| {
            let mut role = default_task_role(config, job.role);
            let mut suffix_thinking = None;
            let mut selected = config.for_role(role);
            if let Some(selector) = selector {
                let (model, thinking) = selector
                    .rsplit_once(':')
                    .filter(|(_, level)| crate::config::agent_defs::valid_thinking_level(level))
                    .map_or((selector, None), |(model, level)| (model, Some(level)));
                suffix_thinking = thinking;
                if let Some(named) = model.strip_prefix('@') {
                    let named = if named == "default" {
                        "main"
                    } else {
                        crate::llm::model_config::LEGACY_ROLE_MIGRATIONS
                            .iter()
                            .find_map(|(old, new)| (*old == named).then_some(*new))
                            .unwrap_or(named)
                    };
                    role = crate::llm::model_config::ROLES
                        .iter()
                        .find_map(|(key, _)| (*key == named && *key != "decision").then_some(*key))
                        .ok_or_else(|| {
                            format!("subagent model role {named:?} is not a chat role")
                        })?;
                    selected = config.for_role(role);
                } else {
                    selected = config
                        .memory_role_config(role, Some(model))
                        .map_err(|e| format!("subagent model: {e:#}"))?;
                }
            }
            if let Some(profile) = profile {
                selected = selected
                    .with_named_model_profile(profile)
                    .map_err(|e| format!("subagent model profile: {e:#}"))?;
            }
            if let Some(level) = definition
                .and_then(|d| d.thinking_level.as_deref())
                .or(suffix_thinking)
            {
                if level == "ultra" {
                    let mut profile = selected
                        .active_model_profile()
                        .cloned()
                        .ok_or("subagent ultra thinking requires a declared model profile")?;
                    profile.reasoning_effort = Some(crate::llm::profiles::ReasoningEffort::Ultra);
                    selected = crate::config::thinking::install_thinking_profile(selected, profile)
                        .map_err(|e| format!("subagent thinking: {e:#}"))?;
                } else {
                    let level = crate::config::thinking::parse_thinking(if level == "none" {
                        "off"
                    } else {
                        level
                    })
                    .map_err(|e| format!("subagent thinking: {e}"))?;
                    selected = crate::config::thinking::apply_thinking(selected, level)
                        .map_err(|e| format!("subagent thinking: {e:#}"))?;
                }
            }
            let source = if profile.is_some() {
                "profile"
            } else if selector.is_some() {
                "definition"
            } else if role == "main" {
                "main"
            } else if config.role_selection(role).is_some() {
                "assigned"
            } else {
                "follows main"
            };
            Ok(ModelRoute {
                role,
                config: selected,
                source,
            })
        })
        .collect()
}

impl Job<'_> {
    /// The child's first message: parent background first, then shared context, then this item's
    /// assignment. The child cannot see the parent's conversation, so the brief carries what it needs.
    pub fn brief(&self) -> String {
        let mut out = String::new();
        // The current request leads: once a newer instruction exists, the session's first goal is
        // historical context and must not frame the assignment.
        match (&self.handoff.current_instruction, &self.handoff.goal) {
            (Some(current), goal) => {
                out.push_str(&format!(
                    "Parent's current request (background for your assignment; it takes precedence):\n{current}\n\n"
                ));
                if let Some(goal) = goal.as_ref().filter(|goal| *goal != current) {
                    out.push_str(&format!(
                        "Session-start context (historical, may be unrelated to the current request):\n{goal}\n\n"
                    ));
                }
            }
            (None, Some(goal)) => {
                out.push_str(&format!(
                    "Parent goal (background, not your assignment):\n{goal}\n\n"
                ));
            }
            (None, None) => {}
        }
        if !self.handoff.todos.is_empty() {
            out.push_str("Parent open todos:\n");
            for todo in &self.handoff.todos {
                out.push_str(&format!("- {todo}\n"));
            }
            out.push('\n');
        }
        if let Some(state) = &self.handoff.working_state {
            out.push_str(&format!(
                "Parent working state (recorded claims, not verified):\n{state}\n\n"
            ));
        }
        if let Some(context) = &self.shared_context {
            out.push_str(&format!("Shared context:\n{context}\n\n"));
        }
        out.push_str(&format!(
            "Scope: {}\n\nAssignment:\n{}",
            self.solution_space, self.prompt
        ));
        out
    }

    pub fn system(&self, root: &Path, tools: &[String]) -> String {
        let mut out = format!(
            "You are a {} subagent working for another agent in {}. You cannot see its conversation; rely on the instructions you are given. {}\n",
            self.kind,
            root.display(),
            self.spec.prompt
        );
        if let Some(extra) = self.preset {
            out.push_str(&format!("{}\n", extra.trim()));
        }
        if !self.preloaded.is_empty() {
            out.push_str("\nPRELOADED SKILLS\nThe following skills were loaded for this agent type. Follow them where they apply; the user's instructions take precedence.\n");
            out.push_str(&self.preloaded);
            out.push('\n');
        }

        if let Some(packet) = &self.work_packet {
            out.push_str(&format!(
                "\nWORK PACKET CONTRACT\nScope: {}\nInterface/handoff: {}\nAcceptance conditions:\n",
                packet.scope, packet.interface
            ));
            for (index, condition) in packet.acceptance.iter().enumerate() {
                out.push_str(&format!("acceptance_{}: {}\n", index + 1, condition));
            }
            out.push_str("Keep changes within the scope unless a necessary dependency forces a deviation; explain any deviation. In your final handoff, report each acceptance condition as met, unmet, or not verified, with concrete evidence. Never claim a check or interface contract passed unless you actually verified it.\n");
            if self.schema.is_none() {
                out.push_str("Prefer a JSON report with summary, blockers, acceptance keyed by acceptance_N (each with status met|unmet|not_verified), evidence_refs, changed_paths, and integration_risks. Report statuses as claims with evidence, not execution lifecycle. Missing statuses are not_verified.\n");
            } else {
                out.push_str("The output_schema remains authoritative for the final answer; do not add fields it forbids. Acceptance coverage absent from that payload remains not_verified in the parent projection.\n");
            }
        }
        if tools.iter().any(|name| name == "codebase_memory") {
            out.push_str("Use saved codebase context to locate the affected area without rediscovering the whole repository. Reuse current source evidence; inspect only missing or stale context. Save reusable architectural findings as facts with codebase_memory remember and supporting source paths; update findings whose sources changed.\n");
            out.push_str("Before proposing or making nontrivial changes, identify the owning module, relevant architecture documents, and existing helpers or analogous implementations to reuse. Cite source locations; justify any new abstraction. Review changes for duplicated logic and boundary violations. Distinguish documented intent from observed code and surface conflicts. Keep investigation task-local.\n");
        }
        out.push_str(&format!("Tools available: {}.\n", tools.join(", ")));
        if let Some(schema) = &self.schema {
            out.push_str(&format!("Your final message MUST be only a JSON value (no prose, no code fence) that validates against this JSON Schema:\n{schema}\n"));
        }
        out
    }
}

pub(in crate::agent) struct Request<'c> {
    pub batch: bool,
    pub jobs: Vec<Job<'c>>,
    pub model_profile: Option<String>,
}

/// Turn raw `task` arguments into validated jobs, or say precisely what is wrong. Nothing runs
/// unless every task in the call is valid.
pub(in crate::agent) fn parse<'c>(args: &Value, config: &'c Config) -> Result<Request<'c>, String> {
    let model_profile = optional_str(args, "model_profile")?.map(str::to_owned);
    if let Some(name) = &model_profile {
        if !config.experimental.profile_matching {
            return Err("`model_profile` requires experimental.profile_matching = true".into());
        }
        if !config.model_profiles.contains_key(name) {
            return Err(format!("unknown configured model profile {name:?}"));
        }
    }
    let (entries, batch) = match (
        crate::tools::present(args, "task"),
        crate::tools::present(args, "tasks"),
    ) {
        (true, true) => return Err("pass either `task` or `tasks`, not both".into()),
        (false, false) => return Err("task needs a non-empty `task` or a `tasks` array".into()),
        (true, false) => (vec![args.clone()], false),
        (false, true) => {
            let tasks = args["tasks"].as_array().ok_or("`tasks` must be an array")?;
            if tasks.is_empty() || tasks.len() > MAX_BATCH {
                return Err(format!(
                    "`tasks` needs 1 to {MAX_BATCH} entries (got {}); split larger batches across calls",
                    tasks.len()
                ));
            }
            (tasks.clone(), true)
        }
    };
    let shared_context = optional_str(args, "context")?;
    if let Some(text) = shared_context
        && text.len() > MAX_CONTEXT
    {
        return Err(format!("`context` exceeds {MAX_CONTEXT} bytes"));
    }
    let default_schema = if batch {
        args.get("output_schema").filter(|value| !value.is_null())
    } else {
        None
    };
    let limit = if batch {
        (RESULT_BUDGET / entries.len()).max(1_500)
    } else {
        RESULT_BUDGET
    };
    let mut jobs: Vec<Job> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let place = if batch {
                format!("tasks[{index}]")
            } else {
                "task".to_owned()
            };
            parse_job(
                index,
                entry,
                shared_context,
                default_schema,
                batch,
                limit,
                config,
            )
            .map_err(|why| format!("{place}: {why}"))
        })
        .collect::<Result<_, _>>()?;
    for job in &mut jobs {
        job.model_routes = model_routes(config, job, model_profile.as_deref())?;
        job.role = job.model_routes[0].role;
    }
    Ok(Request {
        batch,
        jobs,
        model_profile,
    })
}

fn parse_job<'c>(
    index: usize,
    entry: &Value,
    shared_context: Option<&str>,
    default_schema: Option<&Value>,
    batch: bool,
    limit: usize,
    config: &'c Config,
) -> Result<Job<'c>, String> {
    let prompt = entry
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or("needs a non-empty `task`")?;
    if prompt.len() > MAX_PROMPT {
        return Err(format!("`task` exceeds {MAX_PROMPT} bytes"));
    }
    let solution_space = optional_str(entry, "solution_space")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("needs a non-empty `solution_space`: how open-ended the problem is")?;
    let skills = parse_skills(entry.get("skills"))?;
    let requested = optional_str(entry, "agent")?.unwrap_or("scout");
    let mut allowed_tools = None;
    let mut custom_steps = None;
    let mut autoload: &[String] = &[];
    let custom = builtin(requested).is_none();
    let (spec, preset, kind) = match builtin(requested) {
        Some(spec) => (spec, None, spec.name),
        None => match config.subagents.get_key_value(requested) {
            Some((name, text)) => (
                builtin("scout").ok_or("scout agent missing")?,
                Some(text.as_str()),
                name.as_str(),
            ),
            None if config.agents.contains_key(requested) => {
                let (name, agent) = config
                    .agents
                    .get_key_value(requested)
                    .ok_or("custom agent missing")?;
                allowed_tools = agent.tools.as_deref();
                custom_steps = agent.max_steps;
                autoload = agent.autoload_skills.as_slice();
                (
                    builtin(&agent.base).ok_or("custom agent base is not a built-in agent")?,
                    Some(agent.prompt.as_str()),
                    name.as_str(),
                )
            }
            None => {
                let known: Vec<&str> = AGENTS
                    .iter()
                    .map(|a| a.name)
                    .chain(config.subagents.keys().map(String::as_str))
                    .chain(config.agents.keys().map(String::as_str))
                    .collect();
                return Err(format!(
                    "unknown agent {requested:?}; available: {}",
                    known.join(", ")
                ));
            }
        },
    };
    let max_steps = match entry.get("max_steps") {
        None | Some(Value::Null) => custom_steps.unwrap_or(spec.max_steps),
        Some(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("`max_steps` must be a nonnegative 32-bit integer; 0 means unlimited")?,
    };
    let label = optional_str(entry, "name")?
        .map(|name| {
            name.chars()
                .filter(|c| !c.is_control())
                .take(MAX_NAME)
                .collect::<String>()
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            if batch {
                format!("{kind}#{}", index + 1)
            } else {
                kind.to_owned()
            }
        });
    let schema = entry
        .get("output_schema")
        .filter(|value| !value.is_null())
        .or(default_schema)
        .cloned();
    if let Some(schema) = &schema {
        if schema.to_string().len() > MAX_SCHEMA {
            return Err(format!("`output_schema` exceeds {MAX_SCHEMA} bytes"));
        }
        crate::agent::window::output_schema::check_schema(schema)
            .map_err(|why| format!("invalid `output_schema`: {why}"))?;
    }
    let isolated =
        match optional_str(entry, "isolation")? {
            None => false,
            Some("copy") if spec.mutates => true,
            Some("copy") => return Err(
                "`isolation` applies only to workers; read-only agents never change the project"
                    .into(),
            ),
            Some(other) => return Err(format!("unknown isolation {other:?}; use \"copy\"")),
        };
    let work_packet = match entry.get("work_packet") {
        None | Some(Value::Null) => None,
        Some(_) if !config.experimental.work_packet_contracts => {
            return Err("`work_packet` requires experimental.work_packet_contracts = true".into());
        }
        Some(value) => Some(parse_work_packet(value)?),
    };
    let mut autoload_skills = autoload.to_vec();
    for skill in skills {
        if !autoload_skills.contains(&skill) {
            autoload_skills.push(skill);
        }
    }
    Ok(Job {
        depth: 1,
        resume_messages: Vec::new(),
        resume_tools: None,
        index,
        label,
        kind,
        spec,
        role: role_for_agent(spec, custom),
        model_routes: Vec::new(),
        preset,
        allowed_tools,
        prompt: prompt.to_owned(),
        solution_space: solution_space.to_owned(),
        shared_context: shared_context.map(str::to_owned),
        handoff: Handoff::default(),
        max_steps,
        schema,
        isolated,
        limit,
        work_packet,
        autoload_skills,
        preloaded: String::new(),
    })
}

/// Per-item `skills`: names of skills to preload before the child starts.
fn parse_skills(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or("`skills` must be an array of skill names")?;
    if items.len() > MAX_SKILLS {
        return Err(format!("`skills` takes at most {MAX_SKILLS} names"));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "`skills` entries must be non-empty strings".to_owned())
        })
        .collect()
}

fn parse_work_packet(value: &Value) -> Result<WorkPacket, String> {
    let object = value.as_object().ok_or("`work_packet` must be an object")?;
    if object.len() != 3
        || !["scope", "interface", "acceptance"]
            .iter()
            .all(|key| object.contains_key(*key))
    {
        return Err(
            "`work_packet` must contain only `scope`, `interface`, and `acceptance`".into(),
        );
    }
    let text = |key: &str, max: usize| -> Result<String, String> {
        let value = object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("`work_packet.{key}` must be a non-empty string"))?;
        if value.len() > max {
            return Err(format!("`work_packet.{key}` exceeds {max} bytes"));
        }
        Ok(value.to_owned())
    };
    let acceptance = object
        .get("acceptance")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty() && items.len() <= 16)
        .ok_or("`work_packet.acceptance` needs 1 to 16 conditions")?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let value = value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!("`work_packet.acceptance[{index}]` must be a non-empty string")
                })?;
            if value.len() > 512 {
                return Err(format!(
                    "`work_packet.acceptance[{index}]` exceeds 512 bytes"
                ));
            }
            Ok(value.to_owned())
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(WorkPacket {
        scope: text("scope", 2048)?,
        interface: text("interface", 2048)?,
        acceptance,
    })
}

fn optional_str<'a>(entry: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match entry.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
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

/// Worker result payload; its state is the shared execution lifecycle.
pub(in crate::agent) struct Status {
    pub phase: crate::runtime::Phase,
    pub text: String,
}
impl Status {
    pub fn done(text: String) -> Self {
        Self {
            phase: crate::runtime::Phase::Completed,
            text,
        }
    }
    pub fn failed(text: String) -> Self {
        Self {
            phase: crate::runtime::Phase::Failed,
            text,
        }
    }
    pub fn incomplete(text: String) -> Self {
        Self {
            phase: crate::runtime::Phase::Incomplete,
            text,
        }
    }
    pub fn interrupted() -> Self {
        Self {
            phase: crate::runtime::Phase::Interrupted,
            text: "interrupted".into(),
        }
    }
    /// Keep the established task-tool wire spelling; runtime nodes use canonical phases.
    pub fn wire(&self) -> &'static str {
        match self.phase {
            crate::runtime::Phase::Completed => "done",
            crate::runtime::Phase::NeedsInput => "needs_input",
            state => state.label(),
        }
    }
}

pub(in crate::agent) struct Outcome {
    pub status: Status,
    pub data: Option<Value>,
    /// Execution lifecycle, never mixed into an output-schema payload.
    pub isolation: Option<crate::agent::workers::isolation::Merge>,
    /// Non-executing guidance identities/reminders, separate from validated child output.
    pub portable_rules: Option<Value>,
    /// Model requests made.
    pub steps: u32,
    pub retryable_provider: bool,
    pub model_route: Option<Value>,
    /// Controller-owned exact inspection identities; never part of the worker's validated answer.
    pub inspections: crate::agent::workflow::efficiency::Inspections,
}
impl crate::agent::run::engine::Terminal for Outcome {
    fn prevent_completion(&mut self, reason: &str) {
        if self.status.phase == crate::runtime::Phase::Completed {
            self.status.phase = crate::runtime::Phase::Incomplete;
            self.status.text.push_str(&format!("\n{reason}"));
        }
    }
}

impl Outcome {
    pub fn incomplete(steps: u32, why: impl Into<String>) -> Self {
        Self {
            status: Status::incomplete(why.into()),
            data: None,
            isolation: None,
            portable_rules: None,
            steps,
            retryable_provider: false,
            model_route: None,
            inspections: Default::default(),
        }
    }
    pub fn failed(steps: u32, why: impl Into<String>) -> Self {
        Self {
            status: Status::failed(why.into()),
            data: None,
            isolation: None,
            portable_rules: None,
            steps,
            retryable_provider: false,
            model_route: None,
            inspections: Default::default(),
        }
    }
    pub fn interrupted(steps: u32) -> Self {
        Self {
            status: Status::interrupted(),
            data: None,
            isolation: None,
            portable_rules: None,
            steps,
            retryable_provider: false,
            model_route: None,
            inspections: Default::default(),
        }
    }
    pub fn provider_failed(steps: u32, error: &anyhow::Error) -> Self {
        let mut outcome = Self::failed(
            steps,
            crate::agent::run::failure_guidance::provider_failure(error),
        );
        outcome.retryable_provider = crate::llm::fallback::should_fallback(error);
        outcome
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum MutationScope {
    Workspace,
    ManagedMemory,
    SourceRead,
}

/// Asks the user (or applies the mode) for a child's approval-requiring call.
pub(in crate::agent) trait Approver {
    fn tool_signals(
        &mut self,
        _call: &ToolCall,
        _registry: &Registry,
    ) -> Option<crate::tools::exec::signals::CallSignals> {
        None
    }

    fn model_route(&mut self, _route: &Value) {}
    fn progress(&mut self, _steps: u32, _phase: &str, _text: &str) {}
    fn ask_questions(
        &mut self,
        _request: &crate::agent::workflow::questions::QuestionRequest,
        _cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        crate::agent::workflow::questions::QuestionOutcome::Unsupported(
            "This child has no connected owner question input".into(),
        )
    }
    fn memory_capture_allowed(&mut self, _cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        Err("memory capture owner policy is unavailable".into())
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial>;
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        if cancel.load(Ordering::SeqCst) {
            return Err(Denial::User);
        }
        let result = self.authorize(call, summary, needs_approval);
        if cancel.load(Ordering::SeqCst) {
            Err(Denial::User)
        } else {
            result
        }
    }
    /// Recheck computed native targets against the controller's current permissions.
    /// The enclosing operation was already approved; this must never widen its grant.
    fn authorize_mutation(
        &mut self,
        _original: &ToolCall,
        _tool: &str,
        _paths: &[PathBuf],
        _unbounded: bool,
        _cancel: &Arc<AtomicBool>,
        _scope: MutationScope,
    ) -> Result<(), String> {
        Err("native mutation authorization is unavailable for this approver".into())
    }
    fn authorize_managed_memory(
        &mut self,
        original: &ToolCall,
        paths: &[PathBuf],
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), String> {
        self.authorize_mutation(
            original,
            &original.name,
            paths,
            false,
            cancel,
            MutationScope::ManagedMemory,
        )
    }
    fn notice(&mut self, _message: &str) {}
    fn add_cost(&mut self, _cost: f64) {}
    fn take_messages(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn live_coordination(&self) -> bool {
        false
    }
    fn peer_message(&mut self, _args: &Value) -> Result<Value, String> {
        Err("peer messaging unavailable in a synchronous task".into())
    }
    fn tool_started(&mut self, _name: &str, _registry: &Registry) {}
    fn tool_finished(&mut self) {}
    /// Durable transcript for this job, when the runtime records one.
    fn transcript_sink(
        &self,
    ) -> Option<std::sync::Arc<crate::agent::workers::child_transcript::ChildTranscript>> {
        None
    }
}

/// Approves on the calling thread, straight through the parent's policy and UI.
struct Direct<'a> {
    approval: Approval,
    always: &'a mut Vec<PermissionRule>,
    root: &'a Path,
    extra_roots: &'a [PathBuf],
    layout: Option<&'a Layout>,
    plan_mode: bool,
    cost: &'a mut Option<f64>,
    ui: &'a mut dyn Ui,
}

impl Approver for Direct<'_> {
    fn notice(&mut self, message: &str) {
        self.ui.notice(message);
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        self.ui.ask_questions(request, cancel)
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        if cancel.load(Ordering::SeqCst) {
            return Err("memory capture cancelled".into());
        }
        Ok(!self.plan_mode && self.approval != Approval::Readonly)
    }
    fn add_cost(&mut self, cost: f64) {
        *self.cost = Some(self.cost.unwrap_or(0.0) + cost);
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial> {
        if approval::denied_by_rule(self.always, call, self.root) {
            return Err(Denial::Rule);
        }
        if !needs_approval {
            return Ok(());
        }
        approval::authorize(
            self.approval,
            self.always,
            self.root,
            call,
            summary,
            self.ui,
        )
        // Children cannot save project rules; the grant lasts the session.
        .map(|_persist| ())
    }
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        if cancel.load(Ordering::SeqCst) {
            return Err(Denial::User);
        }
        if approval::denied_by_rule(self.always, call, self.root) {
            return Err(Denial::Rule);
        }
        if !needs_approval {
            return if cancel.load(Ordering::SeqCst) {
                Err(Denial::User)
            } else {
                Ok(())
            };
        }
        let prior_rules = self.always.len();
        let result = approval::authorize_with_cancel(
            self.approval,
            self.always,
            self.root,
            call,
            summary,
            self.ui,
            cancel,
        );
        if cancel.load(Ordering::SeqCst) {
            // Do not retain an AllowAlways grant if cancellation won the UI response race.
            self.always.truncate(prior_rules);
            Err(Denial::User)
        } else {
            result.map(|_persist| ())
        }
    }
    fn authorize_mutation(
        &mut self,
        original: &ToolCall,
        tool: &str,
        paths: &[PathBuf],
        unbounded: bool,
        cancel: &Arc<AtomicBool>,
        scope: MutationScope,
    ) -> Result<(), String> {
        if cancel.load(Ordering::SeqCst) {
            return Err("native mutation cancelled".into());
        }
        let result = match scope {
            MutationScope::Workspace => approval::authorize_mutation_paths(
                self.always,
                self.root,
                self.extra_roots,
                matches!(self.approval, Approval::Readonly),
                original,
                tool,
                paths,
                unbounded,
            ),
            MutationScope::SourceRead => approval::authorize_source_read_paths(
                self.always,
                self.root,
                self.extra_roots,
                original,
                tool,
                paths,
                unbounded,
            ),
            MutationScope::ManagedMemory => {
                if tool != original.name || unbounded {
                    return Err(
                        "managed memory authority does not match its declared operation".into(),
                    );
                }
                let layout = self.layout.ok_or("managed memory requires owning layout")?;
                approval::authorize_managed_memory_paths(
                    self.always,
                    layout,
                    matches!(self.approval, Approval::Readonly),
                    original,
                    paths,
                )
            }
        };
        if cancel.load(Ordering::SeqCst) {
            Err("native mutation cancelled".into())
        } else {
            result
        }
    }
}

enum Message {
    CapturePolicy {
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<bool, String>>,
    },
    Question {
        request: crate::agent::workflow::questions::QuestionRequest,
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<crate::agent::workflow::questions::QuestionOutcome>,
    },
    Approve {
        call: ToolCall,
        summary: String,
        needs_approval: bool,
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<(), Denial>>,
    },
    NativeMutation {
        original: ToolCall,
        tool: String,
        paths: Vec<PathBuf>,
        unbounded: bool,
        scope: MutationScope,
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Done(usize, Outcome),
    ToolStarted(usize, Weak<dyn crate::tools::integrations::ext::Tool>),
    ToolFinished(usize),
    Notice(String),
    Cost(f64),
}

/// Approves from a worker thread by asking the calling thread and waiting for its answer.
struct Remote(
    mpsc::Sender<Message>,
    Option<crate::runtime::Body>,
    usize,
    Arc<AtomicBool>,
);

impl Approver for Remote {
    fn model_route(&mut self, route: &Value) {
        if let Some(body) = &self.1 {
            update_model_route(body, route);
        }
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        let (reply, answer) = mpsc::channel();
        if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
            return Err("memory capture cancelled".into());
        }
        self.0
            .send(Message::CapturePolicy {
                cancel: cancel.clone(),
                reply,
            })
            .map_err(|_| "memory capture owner disconnected".to_owned())?;
        loop {
            if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
                return Err("memory capture cancelled".into());
            }
            match answer.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("memory capture owner disconnected".into());
                }
            }
        }
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        use crate::agent::workflow::questions::QuestionOutcome;
        if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
            return QuestionOutcome::Cancelled;
        }
        let (reply, answer) = mpsc::channel();
        if self
            .0
            .send(Message::Question {
                request: request.clone(),
                cancel: self.3.clone(),
                reply,
            })
            .is_err()
        {
            return QuestionOutcome::Disconnected;
        }
        loop {
            if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
                return QuestionOutcome::Cancelled;
            }
            match answer.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(outcome) => {
                    return if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
                        QuestionOutcome::Cancelled
                    } else {
                        outcome
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return QuestionOutcome::Disconnected,
            }
        }
    }
    fn tool_started(&mut self, name: &str, registry: &Registry) {
        if let Some(tool) = registry.get(name) {
            let _ = self
                .0
                .send(Message::ToolStarted(self.2, Arc::downgrade(tool)));
        }
        if let Some(body) = &self.1 {
            let steps = body.read(|n| {
                n.result
                    .data
                    .as_ref()
                    .and_then(|d| d["steps"].as_u64())
                    .unwrap_or(0) as u32
            });
            body.tool_started(name, steps);
        }
    }
    fn tool_finished(&mut self) {
        let _ = self.0.send(Message::ToolFinished(self.2));
    }
    fn progress(&mut self, steps: u32, phase: &str, text: &str) {
        if let Some(body) = &self.1 {
            body.progress(phase, steps);
            if !text.is_empty() {
                body.append(text);
            }
        }
    }
    fn notice(&mut self, message: &str) {
        let _ = self.0.send(Message::Notice(message.to_owned()));
    }
    fn add_cost(&mut self, cost: f64) {
        let _ = self.0.send(Message::Cost(cost));
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial> {
        let cancel = self.3.clone();
        self.authorize_with_cancel(call, summary, needs_approval, &cancel)
    }
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        let interrupted = || {
            if self.3.load(Ordering::SeqCst) {
                // Propagate whole-job interruption into this callback operation's owner flag,
                // so the parent's pending approve_with_cancel observes either source.
                cancel.store(true, Ordering::SeqCst);
            }
            cancel.load(Ordering::SeqCst)
        };
        if interrupted() {
            return Err(Denial::User);
        }
        let (reply, answer) = mpsc::channel();
        let ask = Message::Approve {
            call: call.clone(),
            summary: summary.to_owned(),
            needs_approval,
            cancel: cancel.clone(),
            reply,
        };
        if self.0.send(ask).is_err() {
            return Err(Denial::User);
        }
        loop {
            if interrupted() {
                return Err(Denial::User);
            }
            match answer.recv_timeout(std::time::Duration::from_millis(80)) {
                Ok(result) => {
                    return if interrupted() {
                        Err(Denial::User)
                    } else {
                        result
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Denial::User),
            }
        }
    }
    fn authorize_mutation(
        &mut self,
        original: &ToolCall,
        tool: &str,
        paths: &[PathBuf],
        unbounded: bool,
        cancel: &Arc<AtomicBool>,
        scope: MutationScope,
    ) -> Result<(), String> {
        let interrupted = || {
            if self.3.load(Ordering::SeqCst) {
                cancel.store(true, Ordering::SeqCst);
            }
            cancel.load(Ordering::SeqCst)
        };
        if interrupted() {
            return Err("native mutation cancelled".into());
        }
        let (reply, answer) = mpsc::channel();
        if self
            .0
            .send(Message::NativeMutation {
                original: original.clone(),
                tool: tool.to_owned(),
                paths: paths.to_vec(),
                unbounded,
                scope,
                cancel: cancel.clone(),
                reply,
            })
            .is_err()
        {
            return Err("native mutation controller unavailable".into());
        }
        loop {
            if interrupted() {
                return Err("native mutation cancelled".into());
            }
            match answer.recv_timeout(std::time::Duration::from_millis(80)) {
                Ok(result) => {
                    return if interrupted() {
                        Err("native mutation cancelled".into())
                    } else {
                        result
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("native mutation controller unavailable".into());
                }
            }
        }
    }
}

/// The parent's pieces a `task` call needs, borrowed out of the agent so the call can run while the
/// agent's other state (todo list, session, …) stays untouched.
pub(in crate::agent) struct Parent<'a> {
    pub env: Env<'a>,
    pub approval: Approval,
    pub always: &'a mut Vec<PermissionRule>,
    pub cost: &'a mut Option<f64>,
    pub main: &'a mut Backend,
    /// Cached local backend and the model it serves, for a role that has its own model.
    pub side: &'a mut Option<(String, Backend)>,
    /// LoRA adapter of the active version; only used when children share the main local backend.
    pub adapter: Option<&'a Path>,
    /// Parent goal, open todos and working state; each child receives them in its first message.
    pub handoff: Handoff,
}

/// Run a `task` call to completion and format the result for the model.
pub(in crate::agent) fn run(parent: Parent<'_>, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
    let Parent {
        env,
        approval,
        always,
        cost,
        main,
        side,
        adapter,
        handoff,
    } = parent;
    if !env.config.task.can_spawn_at(0) {
        return ToolOutput::err("task spawning is disabled by task.max_recursion_depth");
    }
    let config = with_file_agents(env.config);
    let mut request = match parse(args, &config) {
        Ok(request) => request,
        Err(why) => {
            return ToolOutput {
                execution: Default::default(),
                data: None,
                text: why,
                is_error: true,
            };
        }
    };
    handoff.apply(&mut request.jobs);
    if let Err(why) = preload_skills(&mut request.jobs, env.layout) {
        return ToolOutput {
            execution: Default::default(),
            data: None,
            text: why,
            is_error: true,
        };
    }
    let main_model = env.config.selection().display_name();
    // Every job runs on its role's model. All roles are resolved before any job starts, so a bad
    // model profile fails the whole call before anything runs.
    let mut plans: Vec<(&str, Config, Harness, Vec<&Job>)> = Vec::new();
    for job in &request.jobs {
        let role = job.model_routes[0].config.clone();
        let controls = serde_json::to_value(&role).expect("validated model controls serialize");
        if let Some((_, _, _, jobs)) = plans.iter_mut().find(|(key, config, _, _)| {
            *key == job.role && serde_json::to_value(config).ok().as_ref() == Some(&controls)
        }) {
            jobs.push(job);
            continue;
        }
        let harness = match role.effective_harness(env.harness) {
            Ok(harness) => harness,
            Err(e) => return ToolOutput::err(format!("subagent model profile: {e:#}")),
        };
        plans.push((job.role, role, harness, vec![job]));
    }
    for (_, _, _, jobs) in &mut plans {
        jobs.sort_by_key(|job| job.spec.mutates);
    }
    plans.sort_by_key(|(_, _, _, jobs)| jobs.iter().any(|job| job.spec.mutates));
    let total = request.jobs.len();
    let mut outcomes: Vec<Option<Outcome>> = request.jobs.iter().map(|_| None).collect();
    // Read-only and isolated jobs on a remote endpoint run concurrently across roles when there are several.
    // Everything else (in-place writers, local models) runs in order, one role group at a time.
    let parallel = plans
        .iter()
        .filter(|(_, role, _, _)| role.endpoint.is_some())
        .flat_map(|(_, _, _, jobs)| jobs.iter())
        .filter(|job| !job.spec.mutates || job.isolated)
        .count()
        > 1;
    let mut parallel_jobs: Vec<(&Job, Env)> = Vec::new();
    let mut serial: Vec<Vec<&Job>> = Vec::with_capacity(plans.len());
    for (key, role, harness, jobs) in &plans {
        ui.role(key, &role.selection().display_name());
        let env = Env {
            config: role,
            harness,
            ..env
        };
        let (concurrent, rest): (Vec<&Job>, Vec<&Job>) = jobs.iter().copied().partition(|job| {
            parallel && role.endpoint.is_some() && (!job.spec.mutates || job.isolated)
        });
        parallel_jobs.extend(concurrent.into_iter().map(|job| (job, env)));
        serial.push(rest);
    }
    run_concurrent(
        env,
        &parallel_jobs,
        config.task.max_concurrency.max(1),
        Direct {
            approval,
            always: &mut *always,
            root: env.workspace.root(),
            extra_roots: env.workspace.additional_roots(),
            layout: env.layout,
            plan_mode: env.plan_mode,
            cost: &mut *cost,
            ui: &mut *ui,
        },
        total,
        &mut outcomes,
    );
    for ((_key, role, harness, _), rest) in plans.iter().zip(serial) {
        if rest.is_empty() {
            continue;
        }
        let assigned = role.selection() != env.config.selection();
        let env = Env {
            config: role,
            harness,
            ..env
        };
        // A local model server is one shared resource; remote endpoints take concurrent requests.
        let shared = role.endpoint.is_none();
        let lanes = Lanes {
            main: &mut *main,
            side: &mut *side,
            adapter,
            shared,
            assigned,
        };
        run_serial(
            env,
            role,
            &rest,
            lanes,
            Direct {
                approval,
                always: &mut *always,
                root: env.workspace.root(),
                extra_roots: env.workspace.additional_roots(),
                layout: env.layout,
                plan_mode: env.plan_mode,
                cost: &mut *cost,
                ui: &mut *ui,
            },
            total,
            &mut outcomes,
        );
    }
    ui.role("main", &main_model);
    let models: std::collections::BTreeMap<usize, String> = request
        .jobs
        .iter()
        .map(|job| {
            (
                job.index,
                job.model_routes[0].config.selection().display_name(),
            )
        })
        .collect();
    render(&request, outcomes, &models, env.workspace.artifacts())
}

pub(in crate::agent) fn nested_schema(config: &Config) -> Value {
    let mut schema = schema(config);
    let props = schema["function"]["parameters"]["properties"]
        .as_object_mut()
        .expect("task properties");
    props.insert("action".into(), json!({"type":"string","enum":["run"],"description":"Nested tasks run synchronously within this worker's spawn policy and depth limit."}));
    for name in ["id", "message", "timeout"] {
        props.remove(name);
    }
    schema
}

/// Only explicit spawn permissions expose delegation to a child.
pub(in crate::agent) fn spawn_policy(
    config: &Config,
    job: &Job,
) -> crate::agent::base::agent_defs::Spawns {
    crate::agent::base::agent_defs::Spawns::from_list(
        config
            .agents
            .get(job.kind)
            .and_then(|agent| agent.spawns.clone())
            .unwrap_or_else(|| job.spec.spawns.iter().map(|s| (*s).to_owned()).collect()),
    )
}

pub(in crate::agent) fn nested(
    env: Env,
    parent: &Job,
    args: &Value,
    names: &[String],
    approver: &mut dyn Approver,
) -> ToolOutput {
    if args["action"].as_str().unwrap_or("run") != "run" {
        return ToolOutput::err("descendants support synchronous task run only");
    }
    if !env.config.task.can_spawn_at(parent.depth) {
        return ToolOutput::err("task recursion depth reached");
    }
    let plugins = env.ext.plugins();
    if let Err(reason) = crate::agent::safety::hooks::gate(
        &env.config.hooks,
        plugins.as_deref(),
        env.workspace.root(),
        "before_subagent_spawn",
        json!({"args":args,"agent":parent.kind,"depth":parent.depth}),
        env.interrupt,
        &mut |notice| approver.notice(notice),
    ) {
        return ToolOutput::err(reason).with_status(crate::tools::ToolStatus::Blocked);
    }
    let config = with_file_agents(env.config);
    let mut request = match parse(args, &config) {
        Ok(request) => request,
        Err(error) => return ToolOutput::err(error),
    };
    let policy = spawn_policy(&config, parent);
    if request
        .jobs
        .iter()
        .any(|job| !policy.allows(job.kind) || (!parent.spec.mutates && job.spec.mutates))
    {
        return ToolOutput::err(
            "descendant role is not permitted by the parent's spawn or write policy",
        );
    }
    let mut harness = env.harness.clone();
    harness.tools.retain(|name| names.contains(name));
    if let Err(error) = preload_skills(&mut request.jobs, env.layout) {
        return ToolOutput::err(error);
    }
    let mut outcomes = Vec::new();
    let mut models = BTreeMap::new();
    for job in &mut request.jobs {
        job.depth = parent.depth + 1;
        models.insert(
            job.index,
            job.model_routes[0].config.selection().display_name(),
        );
        let role = &job.model_routes[0].config;
        let mut role_harness = match role.effective_harness(&harness) {
            Ok(value) => value,
            Err(error) => return ToolOutput::err(format!("descendant model profile: {error:#}")),
        };
        role_harness.tools.retain(|name| names.contains(name));
        let mut descendant = Rebase {
            inner: approver,
            from: env.workspace.root(),
            to: env.workspace.root(),
            descendant: Some(&job.label),
        };
        outcomes.push(Some(run_isolated(
            Env {
                config: role,
                harness: &role_harness,
                ..env
            },
            role,
            job,
            &mut descendant,
        )));
    }
    render(&request, outcomes, &models, env.workspace.artifacts())
}

/// Loads each job's autoloaded skills through the same `skill` body loader the model uses, so a
/// spawned agent starts with that guidance in its system prompt. A missing skill fails the spawn.
pub(in crate::agent) fn preload_skills(
    jobs: &mut [Job<'_>],
    layout: Option<&Layout>,
) -> Result<(), String> {
    for job in jobs
        .iter_mut()
        .filter(|job| !job.autoload_skills.is_empty())
    {
        let layout = layout.ok_or("autoload_skills need a Dial layout to find skills")?;
        let mut bodies = Vec::new();
        for name in &job.autoload_skills {
            let output = crate::storage::skills::skill_tool::call(
                layout,
                &serde_json::json!({ "name": name }),
            );
            if output.is_error {
                return Err(format!("agent {}: {}", job.kind, output.text));
            }
            bodies.push(output.text);
        }
        job.preloaded = bodies.join("\n\n");
    }
    Ok(())
}

/// Adds file-defined agents (project and user definition directories) to a per-spawn copy of the
/// config as `[agents.<name>]`-shaped entries, so they get the same base, prompt, tool narrowing,
/// writer approval and autoloaded skills as configured agents. Built-in, preset and configured names
/// keep their precedence.
pub(in crate::agent) fn with_file_agents(config: &Config) -> Config {
    use crate::agent::base::agent_defs::{Catalog, Source, Spawns};
    use crate::config::types::AgentConfig;
    let mut merged = config.clone();
    for def in Catalog::load(config).iter() {
        if matches!(
            def.source,
            Source::Builtin | Source::Preset | Source::Config
        ) || merged.agents.contains_key(&def.name)
            || merged.subagents.contains_key(&def.name)
        {
            continue;
        }
        let spawns = match &def.spawns {
            Spawns::None => Vec::new(),
            Spawns::All => vec!["*".to_owned()],
            Spawns::Only(list) => list.clone(),
        };
        merged.agents.insert(
            def.name.clone(),
            AgentConfig {
                base: if def.mutates { "worker" } else { "scout" }.into(),
                description: def.description.clone(),
                prompt: def.prompt.clone(),
                tools: def.restrict.clone(),
                max_steps: (def.max_steps > 0).then_some(def.max_steps),
                spawns: Some(spawns),
                model: def.model.clone(),
                thinking_level: def.thinking.clone(),
                output: def.output.clone(),
                autoload_skills: def.autoload_skills.clone(),
            },
        );
    }
    merged
}

/// Text-only surfaces (print, RPC without a runtime tree) learn about workers from these notices; an
/// interactive transcript draws them inside the task card instead.
fn announce_start(ui: &mut dyn Ui, job: &Job, total: usize) {
    if ui.renders_workers() {
        return;
    }
    ui.notice(&format!(
        "subagent {} ({}/{total}): {}",
        job.label,
        job.index + 1,
        prompt::truncate_chars(&job.prompt, 70)
    ));
}

fn worker_body(ui: &mut dyn Ui, job: &Job, role: &Config) -> Option<crate::runtime::Body> {
    let tree = ui.runtime_tree()?;
    let parent = tree
        .bodies("tool")
        .into_iter()
        .rev()
        .find(|b| b.read(|n| n.label == "task" && n.result.phase == crate::runtime::Phase::Running))
        .map(|b| b.read(|n| n.id.clone()));
    let body = tree.start("worker", &job.label, parent);
    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    body.settle(String::new(),Some(json!({"name":job.label,"agent":job.kind,"role":job.role,"model":role.selection().display_name(),"model_source":job.model_routes[0].source,"status":"running","phase":"starting","steps":0,"tools":0,"started_ms":started_ms,"task":prompt::truncate_chars(&job.prompt, 120)})),crate::runtime::Phase::Running);
    ui.result_node(&body);
    Some(body)
}

pub(in crate::agent) fn update_model_route(body: &crate::runtime::Body, route: &Value) {
    let (text, mut data) = body.read(|node| {
        (
            node.result.text.clone(),
            node.result.data.clone().unwrap_or(json!({})),
        )
    });
    if let Some(fields) = route.as_object() {
        for (key, value) in fields {
            data[key] = value.clone();
        }
    }
    body.settle(text, Some(data), crate::runtime::Phase::Running);
}

/// Fallbacks are safe only before a child has performed any work. Never restart a
/// partially executed job: its owner receives the failure and retained evidence.
pub(in crate::agent) fn run_child(
    env: Env,
    job: &Job,
    backend: &mut Backend,
    adapter: Option<&Path>,
    approver: &mut dyn Approver,
) -> Outcome {
    if let Some(route) = job.model_routes.first() {
        approver.model_route(&route.metadata());
    }
    let mut outcome = child::run(env, job, backend, adapter, approver);
    let mut route_used = job.model_routes.first().map(ModelRoute::metadata);
    let mut attempts = outcome.steps;
    for route in job.model_routes.iter().skip(1) {
        if !job.resume_messages.is_empty()
            || !outcome.retryable_provider
            || outcome.steps > 1
            || env.interrupt.load(Ordering::SeqCst)
        {
            break;
        }
        backend.release();
        let mut harness = match route.config.effective_harness(env.harness) {
            Ok(harness) => harness,
            Err(error) => {
                return Outcome::failed(attempts, format!("subagent fallback: {error:#}"));
            }
        };
        harness
            .tools
            .retain(|name| env.harness.tools.contains(name));
        let mut alternate = match make_backend(&route.config) {
            Ok(backend) => backend,
            Err(error) => {
                return Outcome::failed(attempts, format!("subagent fallback: {error:#}"));
            }
        };
        approver.model_route(&route.metadata());
        route_used = Some(route.metadata());
        let earlier_inspections = outcome.inspections;
        outcome = child::run(
            Env {
                config: &route.config,
                harness: &harness,
                ..env
            },
            job,
            &mut alternate,
            None,
            approver,
        );
        alternate.release();
        outcome.inspections.extend(earlier_inspections);
        attempts = attempts.saturating_add(outcome.steps);
    }
    outcome.steps = attempts;
    outcome.model_route = route_used;
    outcome
}

fn finish_worker(body: &Option<crate::runtime::Body>, outcome: &Outcome) {
    if let Some(body) = body {
        let phase = outcome.status.phase;
        let text = outcome.status.text.clone();
        let mut data = body.read(|n| n.result.data.clone().unwrap_or(json!({})));
        data["steps"] = json!(outcome.steps);
        if let Some(started) = data["started_ms"].as_u64() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(started, |d| d.as_millis() as u64);
            data["elapsed_ms"] = json!(now.saturating_sub(started));
        }
        data["result"] = outcome.data.clone().unwrap_or_else(|| json!(text));
        if let Some(rules) = &outcome.portable_rules {
            data["portable_rules"] = rules.clone();
        }
        if let Some(isolation) = &outcome.isolation {
            data["isolation"] = json!(isolation);
        }
        data["status"] = json!(outcome.status.wire());
        body.settle(text, Some(data), phase);
    }
}

fn announce_end(ui: &mut dyn Ui, job: &Job, outcome: &Outcome) {
    if ui.renders_workers() {
        return;
    }
    let verdict = if outcome.status.phase == crate::runtime::Phase::Completed {
        "finished".to_owned()
    } else {
        format!(
            "{}: {}",
            outcome.status.phase.label(),
            prompt::truncate_chars(&outcome.status.text, 80)
        )
    };
    ui.notice(&format!(
        "subagent {} {verdict} after {} step(s)",
        job.label, outcome.steps
    ));
}

/// Rewrites paths inside the isolated copy to the project's, so permission rules (which are about
/// project paths) judge what the worker really targets even when it uses absolute paths.
pub(in crate::agent) struct Rebase<'a> {
    pub inner: &'a mut dyn Approver,
    pub from: &'a Path,
    pub to: &'a Path,
    pub descendant: Option<&'a str>,
}

/// Rewrites `from` to `to` only where `from` is a whole path: a match may not continue into a longer
/// name (`/tmp/ws` must not rewrite `/tmp/ws2/x`) nor start inside one (`/x/tmp/ws`). Returns `None`
/// when nothing changed.
pub(in crate::agent) fn rebase_text(text: &str, from: &str, to: &str) -> Option<String> {
    if from.is_empty() {
        return None;
    }
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, _) in text.match_indices(from) {
        let end = start + from.len();
        let starts_name = text[..start].chars().next_back().is_some_and(name_char);
        let continues_name =
            !from.ends_with(['/', '\\']) && text[end..].chars().next().is_some_and(name_char);
        if starts_name || continues_name {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(to);
        last = end;
    }
    (last > 0).then(|| {
        out.push_str(&text[last..]);
        out
    })
}

fn rebase_value(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) => {
            if let Some(rebased) = rebase_text(text, from, to) {
                *text = rebased;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rebase_value(v, from, to)),
        Value::Object(map) => map.values_mut().for_each(|v| rebase_value(v, from, to)),
        _ => {}
    }
}

impl Approver for Rebase<'_> {
    fn tool_signals(
        &mut self,
        call: &ToolCall,
        registry: &Registry,
    ) -> Option<crate::tools::exec::signals::CallSignals> {
        self.inner.tool_signals(call, registry)
    }

    fn model_route(&mut self, route: &Value) {
        if let Some(label) = self.descendant {
            self.inner.notice(&format!(
                "Descendant {label}: {}",
                route["model"].as_str().unwrap_or("configured model")
            ));
        } else {
            self.inner.model_route(route);
        }
    }
    fn progress(&mut self, steps: u32, phase: &str, text: &str) {
        if let Some(label) = self.descendant {
            self.inner
                .progress(steps, &format!("descendant {label}: {phase}"), text);
        } else {
            self.inner.progress(steps, phase, text);
        }
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial> {
        let mut call = call.clone();
        rebase_value(
            &mut call.arguments,
            &self.from.to_string_lossy(),
            &self.to.to_string_lossy(),
        );
        let summary = format!("[{}] {summary}", self.descendant.unwrap_or("isolated copy"));
        self.inner.authorize(&call, &summary, needs_approval)
    }
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        if cancel.load(Ordering::SeqCst) {
            return Err(Denial::User);
        }
        let mut call = call.clone();
        rebase_value(
            &mut call.arguments,
            &self.from.to_string_lossy(),
            &self.to.to_string_lossy(),
        );
        let summary = format!("[{}] {summary}", self.descendant.unwrap_or("isolated copy"));
        self.inner
            .authorize_with_cancel(&call, &summary, needs_approval, cancel)
    }
    fn authorize_mutation(
        &mut self,
        original: &ToolCall,
        tool: &str,
        paths: &[PathBuf],
        unbounded: bool,
        cancel: &Arc<AtomicBool>,
        scope: MutationScope,
    ) -> Result<(), String> {
        if cancel.load(Ordering::SeqCst) {
            return Err("native mutation cancelled".into());
        }
        if scope == MutationScope::ManagedMemory {
            return self
                .inner
                .authorize_mutation(original, tool, paths, unbounded, cancel, scope);
        }
        let paths: Vec<PathBuf> = paths
            .iter()
            .map(|path| {
                if !path.is_absolute()
                    || path.components().any(|component| {
                        matches!(component, Component::ParentDir | Component::CurDir)
                    })
                {
                    return Err("native mutation target is not canonical".to_owned());
                }
                let relative = path.strip_prefix(self.from).map_err(|_| {
                    "native mutation target is outside the isolated workspace".to_owned()
                })?;
                Ok(self.to.join(relative))
            })
            .collect::<Result<_, String>>()?;
        let mut original = original.clone();
        rebase_value(
            &mut original.arguments,
            &self.from.to_string_lossy(),
            &self.to.to_string_lossy(),
        );
        self.inner
            .authorize_mutation(&original, tool, &paths, unbounded, cancel, scope)
    }
    fn notice(&mut self, message: &str) {
        self.inner.notice(message)
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        self.inner.ask_questions(request, cancel)
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        self.inner.memory_capture_allowed(cancel)
    }
    fn add_cost(&mut self, cost: f64) {
        self.inner.add_cost(cost)
    }
    fn take_messages(&mut self) -> Vec<String> {
        self.inner.take_messages()
    }
    fn transcript_sink(
        &self,
    ) -> Option<std::sync::Arc<crate::agent::workers::child_transcript::ChildTranscript>> {
        self.inner.transcript_sink()
    }
    fn live_coordination(&self) -> bool {
        self.inner.live_coordination()
    }
    fn peer_message(&mut self, args: &Value) -> Result<Value, String> {
        self.inner.peer_message(args)
    }
    fn tool_started(&mut self, name: &str, registry: &Registry) {
        self.inner.tool_started(name, registry)
    }
    fn tool_finished(&mut self) {
        self.inner.tool_finished()
    }
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
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(owner_env, approver)))
                .unwrap_or_else(|_| Outcome::failed(0, "subagent worker panicked"));
        approver.tool_finished();
        return outcome;
    }
    let isolated = match crate::agent::workers::isolation::Isolated::with_config(
        env.workspace,
        &env.config.task,
        env.config.isolation.backend,
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
    rebase.tool_finished();
    // Stop owned processes before inspecting changes: no background writer/client
    // may keep modifying the copy while it is being merged or retained for recovery.
    drop(registry);
    if env.interrupt.load(Ordering::SeqCst)
        && outcome.status.phase == crate::runtime::Phase::Completed
    {
        outcome.status = Status::interrupted();
    }
    let isolation = match outcome.status.phase {
        crate::runtime::Phase::Completed => isolated.merge(env.workspace, env.interrupt),
        crate::runtime::Phase::Interrupted => {
            let mut recovery = isolated.recover("worker interrupted");
            recovery.interrupted = true;
            recovery
        }
        state => isolated.recover(&format!("worker {}", state.label())),
    };
    if isolation.interrupted {
        outcome.status = Status::interrupted();
    } else if !isolation.errors.is_empty()
        && outcome.status.phase == crate::runtime::Phase::Completed
    {
        outcome.status = Status::failed(format!(
            "worker finished, but isolated changes encountered merge errors; see isolation metadata\n\nWorker answer:\n{}",
            outcome.status.text
        ));
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
    rolling(
        items,
        limit,
        |(job, env, body, report)| {
            let outcome = run_isolated(
                env,
                env.config,
                job,
                &mut Remote(
                    report.tx.clone(),
                    body.clone(),
                    job.index,
                    env.interrupt.clone(),
                ),
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
        let _ = self.tx.send(Message::Done(self.index, outcome));
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.reported {
            let _ = self.tx.send(Message::Done(
                self.index,
                Outcome::failed(0, "subagent worker panicked before reporting its outcome"),
            ));
        }
    }
}

/// Runs `work` over `items` on at most `limit` scoped threads. A thread takes the next unstarted item
/// as soon as it finishes one, so a slow item never idles the other slots. `pump` runs on the calling
/// thread while the workers are alive. A panic in `work` is contained so the queue keeps draining and
/// `pump` is released; the first such panic is resumed on the calling thread once everything has ended.
fn rolling<T: Send>(items: Vec<T>, limit: usize, work: impl Fn(T) + Sync, pump: impl FnOnce()) {
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
    let mut active: BTreeMap<usize, Weak<dyn crate::tools::integrations::ext::Tool>> =
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
            Ok(Message::Notice(message)) => direct.notice(&message),
            Ok(Message::Cost(cost)) => direct.add_cost(cost),
            Ok(Message::ToolStarted(index, tool)) => {
                if env.interrupt.load(Ordering::SeqCst) {
                    if let Some(tool) = tool.upgrade() {
                        tool.cancel_operation();
                    }
                } else {
                    active.insert(index, tool);
                }
            }
            Ok(Message::ToolFinished(index)) => {
                active.remove(&index);
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
                cancel,
                reply,
            }) => {
                let prior_rules = direct.always.len();
                let mut decision =
                    if env.interrupt.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                        Err(Denial::User)
                    } else {
                        direct.authorize_with_cancel(&call, &summary, needs_approval, &cancel)
                    };
                if env.interrupt.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                    cancel.store(true, Ordering::SeqCst);
                    direct.always.truncate(prior_rules);
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
                active.remove(&index);
                if let Some(job) = jobs.iter().find(|job| job.index == index) {
                    announce_end(direct.ui, job, &outcome);
                }
                outcomes[index] = Some(outcome);
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
        std::thread::scope(|scope| {
            let own = &mut own;
            let main = &mut *main;
            scope.spawn(move || {
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
                let _ = tx.send(Message::Done(job.index, outcome));
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

/// A lone `prompt` returns just the answer; a batch returns one labelled section per task, in input order.
fn render(
    request: &Request,
    outcomes: Vec<Option<Outcome>>,
    models: &std::collections::BTreeMap<usize, String>,
    artifacts: Option<&crate::storage::artifact::ArtifactStore>,
) -> ToolOutput {
    let outcomes: Vec<Outcome> = outcomes
        .into_iter()
        .map(|o| o.unwrap_or_else(|| Outcome::failed(0, "subagent produced no result")))
        .collect();
    let mut data = json!({"jobs": request.jobs.iter().zip(&outcomes).map(|(job,outcome)| {
        let status = outcome.status.wire();
        let text = outcome.status.text.as_str();
        let mut result = json!({"name":job.label,"agent":job.kind,"status":status,"steps":outcome.steps,"result":outcome.data.clone().unwrap_or_else(|| json!(text))});
        result["inspections"] = json!(outcome.inspections);
        result["projection"] = result_projection(text, outcome.data.as_ref(), job.work_packet.as_ref().map(|p| p.acceptance.as_slice()).unwrap_or_default());
        result["role"] = json!(job.role);
        result["model"] = json!(models.get(&job.index));
        if let Some(route) = &outcome.model_route {
            for key in ["role", "model", "model_source"] { result[key] = route[key].clone(); }
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
                        "\n\n[Preview only; complete task report: {uri}. Read with artifact_read.]"
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
