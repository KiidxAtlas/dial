//! Subagent hub model: one projection of every worker in a session.
//!
//! Sources, merged by worker id:
//! - runtime `worker` bodies (live and inline workers); their `data` fields are the live truth,
//! - persisted child transcripts (background workers; header plus `status` record),
//! - `LiveTaskControl` (which background jobs still run, steering, cancellation, mailbox traffic).
//!
//! Body `data` keys read here (written by the worker runtime):
//! `id`, `name`, `agent`, `role`, `model`, `model_source`, `phase`, `task`, `steps`, `tools`
//! (tool calls), `current_tool`, `current`, `requests`, `tokens_in`, `tokens_out`, `cost_usd`,
//! `context` (`[used, window]`), `retry` (`[attempt, max]`), `started_ms`, `elapsed_ms`,
//! `last_activity_ms`, `parent` (worker id), `artifacts` (`[[label, path-or-uri], ...]`),
//! `isolation` (backend and branch or path), `config` (settings lifecycle metadata).
//!
//! Transcript fields are the same keys, in the header or the final `status` record.
//!
//! Everything here reads disk or locks; call [`snapshot`] at most about once a second and never
//! from draw code.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::agent::LiveTaskControl;
use crate::agent::workers::child_transcript::{
    self, Change, Follow, MessageFollower, TranscriptSummary,
};
use crate::config::paths::Layout;
use crate::runtime::{Phase, Tree};

/// Transcripts read for the activity feed; older workers keep their lifecycle events only.
const ACTIVITY_TRANSCRIPTS: usize = 12;
/// Activity events kept in a snapshot (newest last).
const ACTIVITY_LIMIT: usize = 300;
/// Follower reads one persistent [`HubCache`] refresh may spend on each transcript. One read
/// covers [`child_transcript::FOLLOW_CHUNK`] bytes, so a long history is followed over several
/// refreshes, each keeping its position. A free [`snapshot`] follows it to the end at once.
const PERSISTENT_FOLLOW_ROUNDS: usize = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentStatus {
    #[default]
    Running,
    /// Blocked on an approval or a question.
    Waiting,
    Done,
    Failed,
    Cancelled,
    /// Work ended without an explicit cancellation receipt.
    Interrupted,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    /// Roster order: work that needs the operator first (input requests, then failures), then
    /// live work, then the rest of the finished work.
    pub fn rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Failed | Self::Interrupted => 1,
            Self::Running => 2,
            Self::Done => 3,
            Self::Cancelled => 4,
        }
    }

    /// How urgently the operator should look: `0` an input request, `1` a failure, `2` nothing.
    /// Lists that keep a stable order still float these to the top.
    pub fn attention(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Failed | Self::Interrupted => 1,
            _ => 2,
        }
    }

    fn from_phase(phase: Phase) -> Self {
        match phase {
            Phase::Running => Self::Running,
            Phase::Blocked | Phase::NeedsInput => Self::Waiting,
            Phase::Completed => Self::Done,
            Phase::Failed | Phase::Incomplete => Self::Failed,
            Phase::Interrupted => Self::Interrupted,
        }
    }

    /// Status from a persisted wire string (`done`, `failed`, `cancelled`, `running`, ...).
    fn from_wire(status: &str) -> Self {
        if status == "cancelled" {
            Self::Cancelled
        } else {
            Self::from_phase(Phase::from_status(status))
        }
    }
}

/// Read-only settings receipt. Requested values never replace the worker's active `model`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerConfig {
    pub revision: u64,
    pub applied_revision: u64,
    pub state: String,
    pub requested_model: Option<String>,
    pub requested_thinking: Option<String>,
    pub failure: Option<String>,
}

impl WorkerConfig {
    fn from_data(data: &Value) -> Option<Self> {
        Some(Self {
            revision: data["revision"].as_u64()?,
            applied_revision: data["applied_revision"].as_u64()?,
            state: data["state"].as_str()?.to_owned(),
            requested_model: data["requested"]["model"].as_str().map(str::to_owned),
            requested_thinking: data["requested"]["thinking"].as_str().map(str::to_owned),
            failure: data["failure"].as_str().map(str::to_owned),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentEntry {
    pub id: String,
    pub name: String,
    /// Agent type, for example `scout` or `worker`.
    pub agent: String,
    /// Worker id of the spawning agent; `None` means the main agent.
    pub parent: Option<String>,
    pub role: String,
    pub model: String,
    pub model_source: String,
    /// Selection-time receipt; absent in older recordings, never inferred from config.
    pub model_explanation: Option<String>,
    pub config: Option<WorkerConfig>,
    pub status: AgentStatus,
    /// One line.
    pub task: String,
    /// `tool · args`, or the latest intent, while running.
    pub current: Option<String>,
    /// `(attempt, max)` of a provider retry in progress.
    pub retry: Option<(u32, u32)>,
    pub steps: u64,
    pub requests: u64,
    pub tool_calls: u64,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
    /// `(used, window)` context tokens.
    pub context: Option<(u64, u64)>,
    pub started_ms: Option<u64>,
    pub last_activity_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
    /// JSONL transcript (background workers only).
    pub transcript: Option<PathBuf>,
    /// `(label, path or artifact:// uri)`.
    pub artifacts: Vec<(String, String)>,
    /// Isolation backend plus branch or path.
    pub isolation: Option<String>,
    /// Inbound messages in the recent mailbox window.
    pub unread: usize,
    /// Can receive a steering message now.
    pub steerable: bool,
    /// Finished and continuable with `resume`.
    pub resumable: bool,
    /// Can be cancelled now.
    pub cancellable: bool,
}

impl AgentEntry {
    /// Only observed input requests, failed/interrupted work or rejected settings need attention.
    pub fn needs_attention(&self) -> bool {
        self.status.attention() < 2
            || self
                .config
                .as_ref()
                .is_some_and(|config| config.state == "not_applied")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Response,
    Tool,
    Error,
    Message,
    Lifecycle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityEvent {
    pub agent_id: String,
    pub at_ms: u64,
    pub kind: ActivityKind,
    pub title: String,
    pub summary: String,
    /// Index among all transcript message records from file start (`child_transcript::entries`).
    pub entry: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub requests: u64,
    pub tool_calls: u64,
    /// Summed elapsed time across agents, in milliseconds.
    pub agent_ms: u64,
    pub running: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HubSnapshot {
    pub agents: Vec<AgentEntry>,
    /// Oldest first, bounded to the newest events.
    pub activity: Vec<ActivityEvent>,
    pub totals: Totals,
}

/// Build the hub projection without keeping state between calls. A refresh that repeats should
/// hold a [`HubCache`], which follows each transcript incrementally instead of rereading it.
pub fn snapshot(
    layout: &Layout,
    session_id: &str,
    runtime: &Tree,
    live: Option<&LiveTaskControl>,
) -> HubSnapshot {
    HubCache {
        unbounded: true,
        ..HubCache::default()
    }
    .snapshot(layout, session_id, runtime, live)
}

/// Per-transcript projection kept between refreshes: the follower's position and at most
/// [`ACTIVITY_LIMIT`] activity events. It never holds message bodies.
struct TranscriptFeed {
    follower: MessageFollower,
    events: VecDeque<ActivityEvent>,
}

/// Projection state owned by one hub view. It follows the newest [`ACTIVITY_TRANSCRIPTS`]
/// transcripts incrementally, so an unchanged transcript costs a header-sized check and an
/// appended one parses only its new records. Feeds that leave that window are dropped, which
/// bounds the cache to the window.
#[derive(Default)]
pub struct HubCache {
    feeds: HashMap<PathBuf, TranscriptFeed>,
    bytes_read: u64,
    read_errors: u64,
    /// Free snapshots follow each transcript to its end in one call. The persistent cache stops
    /// after [`PERSISTENT_FOLLOW_ROUNDS`] reads per transcript and resumes on the next refresh.
    unbounded: bool,
}

/// Size and I/O counters of a [`HubCache`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HubCacheStats {
    /// Transcripts currently followed.
    pub transcripts: usize,
    /// Activity events held across those transcripts.
    pub cached_events: usize,
    /// Bytes read by the activity followers over the cache's lifetime: transcript records and
    /// rewrite checks. Header and status scans are not counted.
    pub bytes_read: u64,
    /// Transcript reads that failed; their activity is omitted until a later read succeeds.
    pub read_errors: u64,
}

impl HubCache {
    pub fn stats(&self) -> HubCacheStats {
        HubCacheStats {
            transcripts: self.feeds.len(),
            cached_events: self.feeds.values().map(|feed| feed.events.len()).sum(),
            bytes_read: self.bytes_read,
            read_errors: self.read_errors,
        }
    }

    /// Build the hub projection. Disk reads happen here, so throttle callers to about once a second.
    /// A persistent cache reads at most [`PERSISTENT_FOLLOW_ROUNDS`] chunks per transcript per call.
    pub fn snapshot(
        &mut self,
        layout: &Layout,
        session_id: &str,
        runtime: &Tree,
        live: Option<&LiveTaskControl>,
    ) -> HubSnapshot {
        let now = epoch_ms();
        let live_running: HashSet<String> = live
            .map(|control| {
                control
                    .list()
                    .into_iter()
                    .filter(|(_, running)| *running)
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default();
        let summaries = child_transcript::scan(layout, session_id);

        let mut agents: Vec<AgentEntry> = Vec::new();
        let mut recorded_nodes = HashMap::new();
        for summary in &summaries {
            let Some(id) = summary.header["id"].as_str() else {
                continue;
            };
            // `scan` is newest first: a resumed worker shows its latest transcript only.
            if agents.iter().any(|agent| agent.id == id) {
                continue;
            }
            if let Some(node) = summary.header["runtime_node"].as_str() {
                recorded_nodes.insert(id, node);
            }
            agents.push(entry_from_transcript(id, summary, &live_running));
        }

        for body in runtime.bodies("worker") {
            let (node_id, phase, data) =
                body.read(|node| (node.id.clone(), node.result.phase, node.result.data.clone()));
            let data = data.unwrap_or(Value::Null);
            let id = data["id"]
                .as_str()
                .map_or_else(|| node_id.clone(), str::to_owned);
            let owned_node = live.and_then(|control| control.current_node(&id));
            if owned_node
                .as_deref()
                .or_else(|| recorded_nodes.get(id.as_str()).copied())
                .is_some_and(|current| current != node_id.as_str())
            {
                continue;
            }
            let index = match agents.iter().position(|agent| agent.id == id) {
                Some(index) => index,
                None => {
                    agents.push(AgentEntry {
                        id: id.clone(),
                        ..AgentEntry::default()
                    });
                    agents.len() - 1
                }
            };
            apply_body(&mut agents[index], &data, phase, now);
        }

        for agent in &mut agents {
            finish_projection(agent, &live_running, now);
        }
        sort_agents(&mut agents);

        let mut unread_by_id: HashMap<String, usize> = HashMap::new();
        let mut traffic_events: Vec<ActivityEvent> = Vec::new();
        let mut seen_traffic: HashSet<(String, String, String)> = HashSet::new();
        if let Some(control) = live {
            for agent in &agents {
                for letter in control.traffic(&agent.id) {
                    if letter.to == agent.id {
                        *unread_by_id.entry(agent.id.clone()).or_default() += 1;
                    }
                    let key = (
                        letter.from.clone(),
                        letter.to.clone(),
                        letter.preview.clone(),
                    );
                    if seen_traffic.insert(key) {
                        traffic_events.push(ActivityEvent {
                            agent_id: agent.id.clone(),
                            at_ms: agent.last_activity_ms.or(agent.started_ms).unwrap_or(now),
                            kind: ActivityKind::Message,
                            title: format!("{} → {}", letter.from, letter.to),
                            summary: crate::ui::text::clip(
                                &crate::ui::text::one_line(&letter.preview),
                                160,
                            ),
                            entry: None,
                        });
                    }
                }
            }
        }
        for agent in &mut agents {
            agent.unread = unread_by_id.get(&agent.id).copied().unwrap_or(0);
        }

        let mut activity = Vec::new();
        let recent = &summaries[..summaries.len().min(ACTIVITY_TRANSCRIPTS)];
        for summary in recent {
            let Some(id) = summary.header["id"].as_str() else {
                continue;
            };
            let Some(agent) = agents.iter().find(|agent| agent.id == id) else {
                continue;
            };
            activity.extend(self.feed_activity(agent, summary));
        }
        self.feeds
            .retain(|path, _| recent.iter().any(|summary| &summary.path == path));
        for agent in &agents {
            if let Some(started) = agent.started_ms {
                activity.push(ActivityEvent {
                    agent_id: agent.id.clone(),
                    at_ms: started,
                    kind: ActivityKind::Lifecycle,
                    title: "started".into(),
                    summary: agent.task.clone(),
                    entry: None,
                });
            }
            if agent.status.is_finished() {
                activity.push(ActivityEvent {
                    agent_id: agent.id.clone(),
                    at_ms: agent.last_activity_ms.or(agent.started_ms).unwrap_or(now),
                    kind: if agent.status == AgentStatus::Failed {
                        ActivityKind::Error
                    } else {
                        ActivityKind::Lifecycle
                    },
                    title: agent.status.label().into(),
                    summary: agent.current.clone().unwrap_or_default(),
                    entry: None,
                });
            }
        }
        activity.extend(traffic_events);
        activity.sort_by_key(|event| event.at_ms);
        if activity.len() > ACTIVITY_LIMIT {
            let excess = activity.len() - ACTIVITY_LIMIT;
            activity.drain(..excess);
        }

        let totals = totals(&agents);
        HubSnapshot {
            agents,
            activity,
            totals,
        }
    }
}

fn entry_from_transcript(
    id: &str,
    summary: &TranscriptSummary,
    live: &HashSet<String>,
) -> AgentEntry {
    let header = &summary.header;
    let status_record = summary.status.as_ref();
    let field = |key: &str| {
        status_record
            .and_then(|status| status[key].as_str())
            .or_else(|| header[key].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let mut entry = AgentEntry {
        id: id.to_owned(),
        name: field("name"),
        agent: field("agent"),
        role: field("role"),
        model: field("model"),
        model_source: field("model_source"),
        model_explanation: status_record
            .and_then(|status| status["model_explanation"].as_str())
            .or_else(|| header["model_explanation"].as_str())
            .map(str::to_owned),
        task: task_line(&header["task"]),
        transcript: Some(summary.path.clone()),
        ..AgentEntry::default()
    };
    if entry.task.is_empty() {
        entry.task = task_line(&header["assignment"]);
    }
    entry.started_ms = header["started_ms"].as_u64();
    entry.parent = header["parent"].as_str().map(str::to_owned);
    entry.steps = status_record
        .and_then(|status| status["steps"].as_u64())
        .or_else(|| header["steps"].as_u64())
        .unwrap_or(0);
    entry.tool_calls = status_record
        .and_then(|status| status["tools"].as_u64())
        .unwrap_or(0);
    entry.requests = status_record
        .and_then(|status| status["requests"].as_u64())
        .unwrap_or(0);
    entry.tokens_in = status_record.and_then(|status| status["tokens_in"].as_u64());
    entry.tokens_out = status_record.and_then(|status| status["tokens_out"].as_u64());
    entry.cost_usd = status_record.and_then(|status| status["cost_usd"].as_f64());
    entry.elapsed_ms = status_record.and_then(|status| status["elapsed_ms"].as_u64());
    entry.last_activity_ms = status_record.and_then(|status| status["finished_ms"].as_u64());
    entry.artifacts = artifacts_of(status_record.unwrap_or(&Value::Null));
    if entry.artifacts.is_empty() {
        entry.artifacts = artifacts_of(header);
    }
    entry.isolation = status_record
        .and_then(|status| status["isolation"].as_str())
        .map(str::to_owned);
    entry.status = match status_record.and_then(|status| status["status"].as_str()) {
        Some(wire) => AgentStatus::from_wire(wire),
        // No final record: running if the owner still has it, otherwise interrupted.
        None if live.contains(id) => AgentStatus::Running,
        None => AgentStatus::Interrupted,
    };
    entry
}

fn apply_body(entry: &mut AgentEntry, data: &Value, phase: Phase, now: u64) {
    let set = |slot: &mut String, key: &str| {
        if let Some(value) = data[key].as_str().filter(|value| !value.is_empty()) {
            *slot = value.to_owned();
        }
    };
    set(&mut entry.name, "name");
    set(&mut entry.agent, "agent");
    set(&mut entry.role, "role");
    set(&mut entry.model, "model");
    set(&mut entry.model_source, "model_source");
    if let Some(explanation) = data["model_explanation"].as_str() {
        entry.model_explanation = Some(explanation.to_owned());
    }
    entry.config = WorkerConfig::from_data(&data["config"]).or_else(|| entry.config.take());
    if let Some(task) = data["task"].as_str() {
        entry.task = one_line_clip(task);
    }
    // A final transcript is an explicit cancellation receipt; Phase alone cannot distinguish it
    // from shutdown/recovery interruption. Only preserve it for this same interrupted incarnation.
    let cancelled = entry.status == AgentStatus::Cancelled
        || data["phase"].as_str() == Some("cancelled")
        || data["status"].as_str() == Some("cancelled");
    entry.status = if phase == Phase::Interrupted && cancelled {
        AgentStatus::Cancelled
    } else {
        AgentStatus::from_phase(phase)
    };
    if entry.status == AgentStatus::Running && data["phase"].as_str() == Some("waiting") {
        entry.status = AgentStatus::Waiting;
    }
    if let Some(steps) = data["steps"].as_u64() {
        entry.steps = steps;
    }
    if let Some(tools) = data["tools"].as_u64() {
        entry.tool_calls = tools;
    }
    if let Some(requests) = data["requests"].as_u64() {
        entry.requests = requests;
    }
    entry.tokens_in = data["tokens_in"].as_u64().or(entry.tokens_in);
    entry.tokens_out = data["tokens_out"].as_u64().or(entry.tokens_out);
    entry.cost_usd = data["cost_usd"].as_f64().or(entry.cost_usd);
    if let (Some(used), Some(window)) = (data["context"][0].as_u64(), data["context"][1].as_u64()) {
        entry.context = Some((used, window));
    }
    if let (Some(attempt), Some(max)) = (data["retry"][0].as_u64(), data["retry"][1].as_u64()) {
        entry.retry = Some((attempt as u32, max as u32));
    }
    if let Some(started) = data["started_ms"].as_u64() {
        entry.started_ms = Some(started);
    }
    if let Some(last) = data["last_activity_ms"].as_u64() {
        entry.last_activity_ms = Some(last);
    }
    if let Some(parent) = data["parent"].as_str() {
        entry.parent = Some(parent.to_owned());
    }
    if let Some(elapsed) = data["elapsed_ms"].as_u64() {
        entry.elapsed_ms = Some(elapsed);
    } else if let Some(started) = entry.started_ms
        && entry.status == AgentStatus::Running
    {
        entry.elapsed_ms = Some(now.saturating_sub(started));
    }
    if let Some(isolation) = data["isolation"].as_str() {
        entry.isolation = Some(isolation.to_owned());
    }
    let mut artifacts = artifacts_of(data);
    if !artifacts.is_empty() {
        entry.artifacts = std::mem::take(&mut artifacts);
    }
    if entry.status == AgentStatus::Running {
        entry.current = data["current"].as_str().map(str::to_owned).or_else(|| {
            data["current_tool"]
                .as_str()
                .map(|tool| format!("tool · {tool}"))
        });
    } else {
        entry.current = None;
        entry.retry = None;
    }
}

fn finish_projection(agent: &mut AgentEntry, live: &HashSet<String>, now: u64) {
    let running_live = live.contains(&agent.id);
    if agent.status == AgentStatus::Running && !running_live && agent.transcript.is_some() {
        // A background body still marked running but no longer owned by the live job.
        agent.status = AgentStatus::Interrupted;
    }
    let active =
        matches!(agent.status, AgentStatus::Running | AgentStatus::Waiting) && running_live;
    agent.steerable = active;
    agent.cancellable = active;
    agent.resumable = agent.status.is_finished() && agent.transcript.is_some();
    if agent.status == AgentStatus::Running
        && let (Some(started), None) = (agent.started_ms, agent.elapsed_ms)
    {
        agent.elapsed_ms = Some(now.saturating_sub(started));
    }
    if agent.last_activity_ms.is_none() {
        agent.last_activity_ms = agent.started_ms;
    }
}

/// Roster order: status rank, most recent activity first, then id.
pub fn sort_agents(agents: &mut [AgentEntry]) {
    agents.sort_by(|a, b| {
        a.status
            .rank()
            .cmp(&b.status.rank())
            .then_with(|| b.last_activity_ms.cmp(&a.last_activity_ms))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Roster row with tree placement: `depth` below a root, and whether it is the last sibling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub index: usize,
    pub depth: usize,
    pub last: bool,
}

/// Parent-first tree order. Workers whose parent is absent (or the main agent) are roots.
/// Input order is kept among siblings, and cycles are cut.
pub fn tree_rows(agents: &[AgentEntry]) -> Vec<TreeRow> {
    let index_of: HashMap<&str, usize> = agents
        .iter()
        .enumerate()
        .map(|(index, agent)| (agent.id.as_str(), index))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); agents.len()];
    let mut roots = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        match agent
            .parent
            .as_deref()
            .and_then(|parent| index_of.get(parent))
        {
            Some(&parent) if parent != index => children[parent].push(index),
            _ => roots.push(index),
        }
    }
    let mut rows = Vec::with_capacity(agents.len());
    let mut placed = vec![false; agents.len()];
    fn walk(
        index: usize,
        depth: usize,
        last: bool,
        children: &[Vec<usize>],
        placed: &mut [bool],
        rows: &mut Vec<TreeRow>,
    ) {
        if placed[index] {
            return;
        }
        placed[index] = true;
        rows.push(TreeRow { index, depth, last });
        let count = children[index].len();
        for (position, &child) in children[index].iter().enumerate() {
            walk(
                child,
                depth + 1,
                position + 1 == count,
                children,
                placed,
                rows,
            );
        }
    }
    let count = roots.len();
    for (position, &root) in roots.iter().enumerate() {
        walk(
            root,
            0,
            position + 1 == count,
            &children,
            &mut placed,
            &mut rows,
        );
    }
    // Members of a parent cycle have no root; list them flat so nothing disappears.
    for index in 0..agents.len() {
        if !placed[index] {
            walk(index, 0, true, &children, &mut placed, &mut rows);
        }
    }
    rows
}

/// Ids of `root` and every worker below it.
pub fn subtree(agents: &[AgentEntry], root: &str) -> HashSet<String> {
    let mut found = HashSet::from([root.to_owned()]);
    loop {
        let before = found.len();
        for agent in agents {
            if let Some(parent) = &agent.parent
                && found.contains(parent)
            {
                found.insert(agent.id.clone());
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// Case-insensitive, space-separated terms, each a subsequence of name, id, or task.
pub fn matches_filter(agent: &AgentEntry, query: &str) -> bool {
    let haystack = format!("{} {} {}", agent.name, agent.id, agent.task).to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|term| is_subsequence(&term, &haystack))
}

fn is_subsequence(term: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    term.chars().all(|wanted| chars.any(|c| c == wanted))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivityFilter {
    #[default]
    All,
    Errors,
    Responses,
    Tools,
    Messages,
}

impl ActivityFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Errors,
            Self::Errors => Self::Responses,
            Self::Responses => Self::Tools,
            Self::Tools => Self::Messages,
            Self::Messages => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Errors => "errors",
            Self::Responses => "responses",
            Self::Tools => "tools",
            Self::Messages => "messages",
        }
    }

    pub fn admits(self, kind: ActivityKind) -> bool {
        match self {
            Self::All => true,
            Self::Errors => kind == ActivityKind::Error,
            Self::Responses => kind == ActivityKind::Response,
            Self::Tools => kind == ActivityKind::Tool,
            Self::Messages => kind == ActivityKind::Message,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ActivityScope {
    #[default]
    All,
    /// Events of one worker.
    Selected(String),
    /// Events of a worker and every worker it spawned.
    Subtree(String),
}

impl ActivityScope {
    pub fn next(&self, selected: Option<&str>) -> Self {
        match (self, selected) {
            (Self::All, Some(id)) => Self::Selected(id.to_owned()),
            (Self::All, None) => Self::All,
            (Self::Selected(id), _) => Self::Subtree(id.clone()),
            (Self::Subtree(_), _) => Self::All,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Selected(_) => "selected",
            Self::Subtree(_) => "subtree",
        }
    }

    /// Whether `event` is inside the scope; `agents` resolves subtrees.
    pub fn admits(&self, event: &ActivityEvent, agents: &[AgentEntry]) -> bool {
        match self {
            Self::All => true,
            Self::Selected(id) => &event.agent_id == id,
            Self::Subtree(id) => subtree(agents, id).contains(&event.agent_id),
        }
    }
}

/// Operator actions on the worker currently selected in the hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Show this line in the hub.
    Banner(String),
    /// Resume a finished worker with this message. The app launches it through the `task`
    /// tool's resume path (`Agent::start_subagents`), with no model turn.
    Resume { id: String, message: String },
}

/// Steer a running background worker: the text is delivered before its next model request.
pub fn steer(live: Option<&LiveTaskControl>, id: &str, text: &str) -> Result<()> {
    let live = live.ok_or_else(|| anyhow!("no live subagents in this session"))?;
    live.send(id, text)
        .map(|_| ())
        .map_err(|error| anyhow!("cannot steer {id}: {error}"))
}

/// Cancel a running background worker.
pub fn cancel(live: Option<&LiveTaskControl>, id: &str) -> Result<()> {
    let live = live.ok_or_else(|| anyhow!("no live subagents in this session"))?;
    live.cancel(id)
        .map(|_| ())
        .map_err(|error| anyhow!("cannot cancel {id}: {error}"))
}

/// Handle a hub action string. `None` means the action is not a subagent action.
///
/// - `subagent-cancel:<id>`: cancel a running worker.
/// - `subagent-message:<id>\n<text>`: steer a running worker; a finished one is resumed.
/// - `subagent-resume:<id>\n<text>`: resume a finished worker.
pub fn act(action: &str, live: Option<&LiveTaskControl>) -> Option<Result<ActionOutcome>> {
    if let Some(body) = action.strip_prefix("subagent-config:") {
        return Some((|| {
            let (id, update) = body
                .split_once('\n')
                .ok_or_else(|| anyhow!("missing worker setting"))?;
            let (axis, value) = update
                .split_once('\n')
                .ok_or_else(|| anyhow!("missing setting value"))?;
            let live = live.ok_or_else(|| anyhow!("no live subagents in this session"))?;
            let result = match axis {
                "model" => live.configure(id, Some(value), None)?,
                "thinking" => live.configure(id, None, Some(value))?,
                _ => return Err(anyhow!("unknown worker setting")),
            };
            Ok(ActionOutcome::Banner(format!("{id}: {result}")))
        })());
    }
    if let Some(id) = action.strip_prefix("subagent-cancel:") {
        return Some(
            cancel(live, id).map(|()| ActionOutcome::Banner(format!("cancel sent to {id}"))),
        );
    }
    if let Some(body) = action.strip_prefix("subagent-message:") {
        let (id, text) = body.split_once('\n').unwrap_or((body, ""));
        let running = live.is_some_and(|control| {
            control
                .list()
                .into_iter()
                .any(|(job, running)| running && job == id)
        });
        if running {
            return Some(
                steer(live, id, text)
                    .map(|()| ActionOutcome::Banner(format!("message sent to {id}"))),
            );
        }
        return Some(Ok(ActionOutcome::Resume {
            id: id.to_owned(),
            message: text.to_owned(),
        }));
    }
    if let Some(body) = action.strip_prefix("subagent-resume:") {
        let (id, text) = body.split_once('\n').unwrap_or((body, ""));
        return Some(Ok(ActionOutcome::Resume {
            id: id.to_owned(),
            message: text.to_owned(),
        }));
    }
    None
}

fn totals(agents: &[AgentEntry]) -> Totals {
    let mut totals = Totals {
        total: agents.len(),
        ..Totals::default()
    };
    for agent in agents {
        totals.cost_usd += agent.cost_usd.unwrap_or(0.0);
        totals.tokens_in += agent.tokens_in.unwrap_or(0);
        totals.tokens_out += agent.tokens_out.unwrap_or(0);
        totals.requests += agent.requests;
        totals.tool_calls += agent.tool_calls;
        totals.agent_ms += agent.elapsed_ms.unwrap_or(0);
        if agent.status == AgentStatus::Running || agent.status == AgentStatus::Waiting {
            totals.running += 1;
        }
    }
    totals
}

impl HubCache {
    /// Advances the feed of one transcript by at most the round budget and returns its events,
    /// timed by the agent. A persistent cache spends [`PERSISTENT_FOLLOW_ROUNDS`] reads per
    /// refresh, so a long or still-growing transcript never stalls the view; the rest is followed
    /// on later refreshes from the same position. A failed read drops the feed (the next call
    /// rebuilds it) and is counted, never reported as an empty fresh transcript.
    fn feed_activity(
        &mut self,
        agent: &AgentEntry,
        summary: &TranscriptSummary,
    ) -> Vec<ActivityEvent> {
        let rounds = if self.unbounded {
            usize::MAX
        } else {
            PERSISTENT_FOLLOW_ROUNDS
        };
        let feed = self
            .feeds
            .entry(summary.path.clone())
            .or_insert_with(|| TranscriptFeed {
                follower: MessageFollower::new(summary.path.clone()),
                events: VecDeque::new(),
            });
        let TranscriptFeed { follower, events } = feed;
        let before = follower.bytes_read();
        let mut result = Ok(());
        for _ in 0..rounds {
            let step = follower.follow(&mut |change| match change {
                Change::Reset => events.clear(),
                Change::Message(index, message) => {
                    push_bounded(events, activity_of_message(&agent.id, index, message));
                }
            });
            match step {
                Ok(Follow::Unchanged) => break,
                Ok(_) => {}
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        self.bytes_read += follower.bytes_read() - before;
        if result.is_err() {
            self.read_errors += 1;
            self.feeds.remove(&summary.path);
            return Vec::new();
        }
        let base = agent
            .last_activity_ms
            .or(agent.started_ms)
            .unwrap_or_else(epoch_ms);
        self.feeds
            .get(&summary.path)
            .into_iter()
            .flat_map(|feed| feed.events.iter())
            .map(|event| ActivityEvent {
                at_ms: base,
                ..event.clone()
            })
            .collect()
    }
}

/// Keeps the newest [`ACTIVITY_LIMIT`] events of one transcript; older ones are dropped.
fn push_bounded(events: &mut VecDeque<ActivityEvent>, new: Vec<ActivityEvent>) {
    for event in new {
        if events.len() == ACTIVITY_LIMIT {
            events.pop_front();
        }
        events.push_back(event);
    }
}

/// Activity events of one archived message. `index` is its `#entry` deep-link index. Times are
/// placeholders; [`HubCache::feed_activity`] assigns the agent's time when it reads the feed.
fn activity_of_message(agent_id: &str, index: usize, message: &Value) -> Vec<ActivityEvent> {
    let text = message_text(message);
    let event = |kind: ActivityKind, title: String, summary: String| ActivityEvent {
        agent_id: agent_id.to_owned(),
        at_ms: 0,
        kind,
        title,
        summary,
        entry: Some(index),
    };
    let short = |body: &str| crate::ui::text::clip(&crate::ui::text::one_line(body), 160);
    match message["role"].as_str().unwrap_or_default() {
        "assistant" => {
            let tool_blocks: Vec<&Value> = message["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "tool_use")
                        .collect()
                })
                .unwrap_or_default();
            let mut events = Vec::new();
            if tool_blocks.is_empty() {
                events.push(event(
                    ActivityKind::Response,
                    "response".into(),
                    short(text.as_str()),
                ));
            } else {
                if !text.trim().is_empty() {
                    events.push(event(
                        ActivityKind::Response,
                        "response".into(),
                        short(text.as_str()),
                    ));
                }
                for block in tool_blocks {
                    events.push(event(
                        ActivityKind::Tool,
                        block["name"].as_str().unwrap_or("tool").to_owned(),
                        String::new(),
                    ));
                }
            }
            events
        }
        "tool" => {
            let failed = message["is_error"].as_bool().unwrap_or(false)
                || message["isError"].as_bool().unwrap_or(false);
            let kind = if failed {
                ActivityKind::Error
            } else {
                ActivityKind::Tool
            };
            vec![event(
                kind,
                message["name"].as_str().unwrap_or("tool result").to_owned(),
                short(text.as_str()),
            )]
        }
        "user" => vec![event(
            ActivityKind::Message,
            "prompt".into(),
            short(text.as_str()),
        )],
        _ => Vec::new(),
    }
}

fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn artifacts_of(data: &Value) -> Vec<(String, String)> {
    data["artifacts"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| Some((row[0].as_str()?.to_owned(), row[1].as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

fn task_line(assignment: &Value) -> String {
    let text = assignment
        .as_str()
        .or_else(|| assignment["task"].as_str())
        .or_else(|| assignment["prompt"].as_str())
        .unwrap_or_default();
    one_line_clip(text)
}

fn one_line_clip(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    crate::ui::text::clip(&crate::ui::text::one_line(first), 160)
}

pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests;
