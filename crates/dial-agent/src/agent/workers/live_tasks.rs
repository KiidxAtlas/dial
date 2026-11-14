//! Live jobs own their backends and conversations. Approval always returns to the controller.
use crate::agent::{
    Ui,
    safety::approval::{self, Denial, PermissionRule},
    workers::mailbox::{self, Team},
    workers::subagents::{self, Approver, Env, Outcome},
};
use crate::runtime::tool_output::ToolOutput;
use crate::{
    config::paths::Layout,
    config::types::{Approval, Config, Harness},
    llm::ToolCall,
    tools::{Workspace, integrations::ext::Registry},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
/// Permission inputs the pump authorizes live-task events against.
pub(in crate::agent) struct PumpPolicy<'a> {
    pub(in crate::agent) read_only: bool,
    pub(in crate::agent) rules: &'a mut Vec<PermissionRule>,
    pub(in crate::agent) root: &'a std::path::Path,
    pub(in crate::agent) extra_roots: &'a [std::path::PathBuf],
}
enum Event {
    Policy(ToolCall, approval::PolicyExplanation),
    CapturePolicy {
        cancel: Arc<AtomicBool>,
        interrupt: Arc<AtomicBool>,
        answer: mpsc::Sender<Result<bool, String>>,
    },
    Question {
        request: crate::agent::workflow::questions::QuestionRequest,
        cancel: Arc<AtomicBool>,
        answer: mpsc::Sender<crate::agent::workflow::questions::QuestionOutcome>,
    },
    Approve {
        call: ToolCall,
        summary: String,
        needs: bool,
        provenance: approval::CallProvenance,
        cancel: Arc<AtomicBool>,
        interrupt: Arc<AtomicBool>,
        answer: mpsc::Sender<Result<(), Denial>>,
    },
    NativeMutation {
        original: ToolCall,
        tool: String,
        paths: Vec<std::path::PathBuf>,
        unbounded: bool,
        scope: subagents::MutationScope,
        cancel: Arc<AtomicBool>,
        interrupt: Arc<AtomicBool>,
        answer: mpsc::Sender<Result<(), String>>,
    },
    Notice(String),
    Cost(f64),
    Done(String, Value),
}
struct JobState {
    body: crate::runtime::Body,
    control: Option<Arc<ControlJob>>,
    interrupt: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    active_tool: ActiveTool,
    delivered: bool,
}
impl JobState {
    fn status(&self) -> Value {
        self.control
            .as_ref()
            .map_or_else(|| body_status(&self.body), |control| job_status(control))
    }
    fn running(&self) -> bool {
        self.body.read(|n| {
            n.result
                .data
                .as_ref()
                .is_some_and(|data| data["status"] == "running")
        })
    }
}
/// Mailbox size used until a batch sets `task.inbox_limit`.
const DEFAULT_INBOX_LIMIT: usize = 256;

/// Result of a worker that unwound or panicked. Its effects are unknown, so it is never replayed.
const WORKER_LOST: &str = "subagent worker panicked";
/// Live jobs by id, for status, cancel and list. Messages travel on the team bus instead.
type LiveJobs = Arc<Mutex<BTreeMap<String, Arc<ControlJob>>>>;
/// Tool calls a job has in flight, keyed by call id. Weak: the registry owns the tools.
type ActiveTool =
    Arc<Mutex<BTreeMap<String, std::sync::Weak<dyn crate::tools::integrations::ext::Tool>>>>;
struct ControlJob {
    interrupt: Arc<AtomicBool>,
    active_tool: ActiveTool,
    body: crate::runtime::Body,
    finished: AtomicBool,
    /// Config the worker launched with; `configure` validates settings against it.
    base: Config,
    settings: Arc<Mutex<control::SettingsSlot>>,
    workspace: Workspace,
    acceptance: Mutex<Option<crate::agent::verify::worker_acceptance::Acceptance>>,
    /// Set when the owner stops the worker by going away (quit, rebuild), not by a cancel request:
    /// its transcript then closes as `interrupted`, which `--restore` resumes, not `cancelled`.
    owner_shutdown: AtomicBool,
}

fn body_status(body: &crate::runtime::Body) -> Value {
    body.read(|n| n.result.data.clone().unwrap_or(Value::Null))
}

/// Publishes settings state on the running worker body as typed data. The header reads `model`, which
/// changes only when `applied_model` is given, so a requested-but-unapplied model never shows as active.
/// `phase` stays the worker's own progress (`tool: X`, `generating`); the settings line lives in
/// `config`, so a settings change never hides what the worker is doing.
fn publish_settings(
    body: &crate::runtime::Body,
    slot: &control::SettingsSlot,
    applied_model: Option<String>,
) {
    let node = body.snapshot();
    let mut data = node
        .result
        .data
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    data["config"] = slot.metadata();
    data["config_status"] = json!(slot.status());
    if let Some(model) = applied_model {
        data["model"] = json!(model);
    }
    body.settle(node.result.text, Some(data), crate::runtime::Phase::Running);
}

fn job_status(job: &ControlJob) -> Value {
    let mut value = body_status(&job.body);
    let acceptance = job
        .acceptance
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(receipt) = acceptance.as_ref() {
        value["acceptance"] = json!(receipt);
    }
    let checks: Vec<String> = value["assignment"]["acceptance_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|check| check.as_str().map(str::to_owned))
        .collect();
    value["candidate"] = subagents::candidate_status(
        value["status"].as_str().unwrap_or("unknown"),
        acceptance
            .as_ref()
            .map(|receipt| receipt.state)
            .unwrap_or_default(),
        &checks,
    );
    value
}

/// Interrupts every tool call a job has in flight. A call that finished was removed on its own,
/// so only still-running calls are cancelled.
fn cancel_active(active: &ActiveTool) {
    let tools: Vec<_> = active
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter_map(std::sync::Weak::upgrade)
        .collect();
    for tool in tools {
        tool.cancel_operation();
    }
}

/// Publish the terminal result before exposing completion to control handles or releasing a lease.
/// Merge rather than replace so assignment, timing and route metadata survive failures too.
fn settle_result(body: &crate::runtime::Body, value: &Value) {
    let mut data = body.read(|node| node.result.data.clone().unwrap_or(json!({})));
    if let (Some(target), Some(fields)) = (data.as_object_mut(), value.as_object()) {
        target.extend(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    data["phase"] = value["status"].clone();
    body.settle(
        value["result"].as_str().unwrap_or("").into(),
        Some(data),
        crate::runtime::Phase::from_status(value["status"].as_str().unwrap_or("failed")),
    );
}
/// Persistence is opt-in for the whole worker lifetime, not just successful completion.
/// Runtime nodes remain available even when no durable conversation is captured.
fn create_transcript(
    persist: bool,
    layout: Option<&Layout>,
    session: &str,
    id: &str,
    header: Value,
) -> anyhow::Result<Option<Arc<super::child_transcript::ChildTranscript>>> {
    layout
        .filter(|_| persist)
        .map(|layout| {
            super::child_transcript::ChildTranscript::create(layout, session, id, header)
                .map(Arc::new)
        })
        .transpose()
}

#[derive(Clone)]
pub struct LiveTaskControl {
    jobs: LiveJobs,
    team: Team,
}
impl LiveTaskControl {
    /// Ids of live jobs and whether each is still running; safe to call while a turn runs.
    pub fn list(&self) -> Vec<(String, bool)> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .map(|(id, job)| (id.clone(), !job.finished.load(Ordering::SeqCst)))
            .collect()
    }
    /// Recent messages sent or received by `id` (`parent` or a live task id), oldest first.
    pub fn traffic(&self, id: &str) -> Vec<mailbox::Traffic> {
        self.team.traffic(id)
    }
    /// Queues model and thinking changes for a live worker. They apply at its next request-safe
    /// boundary, when its backend is swapped and the change is recorded in its transcript.
    pub fn configure(
        &self,
        id: &str,
        model: Option<&str>,
        thinking: Option<&str>,
    ) -> anyhow::Result<Value> {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        let Some(job) = job else {
            anyhow::bail!("unknown live worker {id}");
        };
        if job.finished.load(Ordering::SeqCst) {
            anyhow::bail!("worker {id} has finished; start or resume it with new settings");
        }
        let update = control::WorkerSettings {
            model: model.map(str::to_owned),
            thinking: thinking.map(str::to_owned),
        };
        if update.is_empty() {
            anyhow::bail!("configure needs a model or a thinking level");
        }
        let mut slot = job.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut merged = slot.accepted.clone();
        merged.merge(update);
        control::apply_settings(&job.base, &merged)?;
        slot.accepted = merged.clone();
        slot.queued = true;
        slot.revision += 1;
        slot.failure = None;
        publish_settings(&job.body, &slot, None);
        Ok(
            json!({"id": id, "status": "queued", "revision": slot.revision, "model": merged.model, "thinking": merged.thinking}),
        )
    }

    /// A resumed worker keeps its public id but owns a new runtime node. Historical incarnations
    /// remain in the journal and must not replace the current owner's state in UI projections.
    pub fn current_node(&self, id: &str) -> Option<String> {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()?;
        Some(job.body.read(|node| node.id.clone()))
    }
    pub fn any_running(&self) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|job| !job.finished.load(Ordering::SeqCst))
    }
    /// Status of live jobs in the same shape as [`LiveTasks::status`], readable while a turn runs (no
    /// pump needed: workers update their bodies directly).
    pub fn status(&self, id: Option<&str>) -> Result<Value, String> {
        // Clone cheap handles under the registry lock; projection never performs filesystem I/O.
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<_> = match id {
            Some(id) => vec![(
                id.to_owned(),
                jobs.get(id)
                    .cloned()
                    .ok_or_else(|| format!("unknown job {id}"))?,
            )],
            None => jobs
                .iter()
                .map(|(id, job)| (id.clone(), job.clone()))
                .collect(),
        };
        drop(jobs);
        entries.sort_by_key(|(id, _)| id.trim_start_matches("task-").parse::<u64>().unwrap_or(0));
        if id.is_some() {
            Ok(job_status(&entries[0].1))
        } else {
            Ok(json!(
                entries
                    .iter()
                    .map(|(_, job)| job_status(job))
                    .collect::<Vec<_>>()
            ))
        }
    }
    /// Sends `text` from the parent to `id`, or to `parent`/`all`/a label, through the team bus.
    /// Never waits for the recipient. This out-of-band path (user `/message`, RPC) reports a
    /// finished worker as an error, unlike the model's `message` tool, which gets a receipt.
    pub fn send(&self, id: &str, text: &str) -> anyhow::Result<Value> {
        let receipt = self
            .team
            .send(mailbox::PARENT, id, text)
            .map_err(|why| anyhow::anyhow!(why))?;
        if let ([], [(to, status), ..]) = (receipt.queued.as_slice(), receipt.finished.as_slice()) {
            anyhow::bail!("{to} has already finished ({status}); the message was not delivered");
        }
        Ok(receipt.json())
    }
    pub fn cancel(&self, id: &str) -> anyhow::Result<Value> {
        let job = self
            .jobs
            .lock()
            .map_err(|_| anyhow::anyhow!("live job registry poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("job is unknown or finished"))?;
        job.interrupt.store(true, Ordering::SeqCst);
        cancel_active(&job.active_tool);
        Ok(json!({"id":id,"cancellation_requested":true}))
    }
}
struct CancelUi<'a> {
    inner: &'a mut dyn Ui,
    cancel: &'a AtomicBool,
    interrupt: &'a AtomicBool,
}
impl Ui for CancelUi<'_> {
    fn policy_explanation(
        &mut self,
        call_id: &str,
        tool: &str,
        explanation: &approval::PolicyExplanation,
    ) {
        self.inner.policy_explanation(call_id, tool, explanation);
    }
    fn approve(&mut self, tool: &str, summary: &str) -> crate::agent::Decision {
        self.gate(|inner, cancel| inner.approve_with_cancel(tool, summary, cancel))
    }
    fn approve_call(
        &mut self,
        call_id: &str,
        tool: &str,
        summary: &str,
        _cancel: Option<&AtomicBool>,
    ) -> crate::agent::Decision {
        self.gate(|inner, cancel| inner.approve_call(call_id, tool, summary, Some(cancel)))
    }
    fn approval_reason(&mut self) -> Option<String> {
        self.inner.approval_reason()
    }
}
impl CancelUi<'_> {
    /// Runs one approval on the inner surface under this operation's cancellation flag. Interrupts
    /// cancel the flag first, so a stopped operation is never prompted.
    fn gate(
        &mut self,
        decide: impl FnOnce(&mut dyn Ui, &AtomicBool) -> crate::agent::Decision,
    ) -> crate::agent::Decision {
        if self.interrupt.load(Ordering::SeqCst) {
            self.cancel.store(true, Ordering::SeqCst);
        }
        if self.cancel.load(Ordering::SeqCst) {
            return crate::agent::Decision::Deny;
        }
        let decision = decide(&mut *self.inner, self.cancel);
        if self.interrupt.load(Ordering::SeqCst) {
            self.cancel.store(true, Ordering::SeqCst);
        }
        if self.cancel.load(Ordering::SeqCst) {
            crate::agent::Decision::Deny
        } else {
            decision
        }
    }
}
/// Reports a worker that unwinds outside the `catch_unwind` boundary as failed, so its job never reads as running forever.
struct WorkerExit {
    id: String,
    tx: mpsc::Sender<Event>,
    control: Arc<ControlJob>,
    reported: bool,
    team: Team,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if self.reported {
            return;
        }
        let value = json!({"id":self.id,"status":"failed","steps":0,"result":WORKER_LOST});
        settle_result(&self.control.body, &value);
        self.team.retire(&self.id, "failed");
        self.control.finished.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Event::Done(self.id.clone(), value));
        crate::platform::wake::signal();
    }
}
pub(in crate::agent) struct LiveTasks {
    tree: crate::runtime::Tree,
    jobs: BTreeMap<String, JobState>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    next: u64,
    registry: LiveJobs,
    team: Team,
    completed: Vec<Value>,
    /// Reusable worker pools by `pool-N` id; see `pool.rs`.
    pools: BTreeMap<String, pool::WorkerPool>,
    next_pool: u64,
    /// `task.stall_secs` of the batch that started the newest worker; 0 disables stall detection.
    stall_secs: u64,
    /// Running workers already reported as stalled, so each stall is reported once until it recovers.
    flagged: BTreeSet<String>,
    /// Stalls found by `pump` that no `wait` has reported to the parent yet.
    stall_alerts: Vec<String>,
}
impl Default for LiveTasks {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tree: Default::default(),
            jobs: BTreeMap::new(),
            tx,
            rx,
            next: 0,
            registry: Arc::default(),
            team: Team::open(DEFAULT_INBOX_LIMIT),
            completed: Vec::new(),
            pools: BTreeMap::new(),
            next_pool: 0,
            stall_secs: Config::default().task.stall_secs,
            flagged: BTreeSet::new(),
            stall_alerts: Vec::new(),
        }
    }
}
struct Bridge {
    signals: crate::tools::exec::signals::CallSignals,
    interruptible: Arc<AtomicBool>,
    body: crate::runtime::Body,
    tx: mpsc::Sender<Event>,
    interrupt: Arc<AtomicBool>,
    team: Team,
    id: String,
    active_tool: ActiveTool,
    transcript: Option<Arc<crate::agent::workers::child_transcript::ChildTranscript>>,
    settings: Arc<Mutex<control::SettingsSlot>>,
}
impl Approver for Bridge {
    fn tool_signals(
        &mut self,
        call: &ToolCall,
        registry: &Registry,
    ) -> Option<crate::tools::exec::signals::CallSignals> {
        self.interruptible.store(
            registry
                .get(&call.name)
                .is_some_and(|tool| tool.interruptible(&call.arguments)),
            Ordering::SeqCst,
        );
        Some(self.signals.clone())
    }
    fn model_route(&mut self, route: &Value) {
        subagents::update_model_route(&self.body, route);
    }
    fn transcript_sink(
        &self,
    ) -> Option<Arc<crate::agent::workers::child_transcript::ChildTranscript>> {
        self.transcript.clone()
    }
    fn take_worker_settings(&mut self) -> Option<control::WorkerSettings> {
        let mut slot = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        let accepted = slot.take();
        if accepted.is_some() {
            publish_settings(&self.body, &slot, None);
        }
        accepted
    }
    fn worker_settings_applied(
        &mut self,
        _settings: &control::WorkerSettings,
        resolved: &crate::config::types::Config,
    ) {
        let mut slot = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        slot.acknowledge();
        // The header model changes only here, after the durable record and backend swap succeeded.
        publish_settings(&self.body, &slot, Some(resolved.selection().display_name()));
    }
    fn worker_settings_failed(&mut self, error: &str) {
        let mut slot = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        slot.fail(error);
        publish_settings(&self.body, &slot, None);
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
            return Err("memory capture cancelled".into());
        }
        let (answer, rx) = mpsc::channel();
        self.tx
            .send(Event::CapturePolicy {
                cancel: cancel.clone(),
                interrupt: self.interrupt.clone(),
                answer,
            })
            .map_err(|_| "memory capture owner disconnected".to_owned())?;
        crate::platform::wake::signal();
        loop {
            if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
                return Err("memory capture cancelled".into());
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
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
        if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
            return QuestionOutcome::Cancelled;
        }
        let mut request = request.clone();
        request.owner_task_id = self.body.read(|node| {
            node.result
                .data
                .as_ref()
                .and_then(|data| data["id"].as_str())
                .map(str::to_owned)
        });
        let (answer, rx) = mpsc::channel();
        if self
            .tx
            .send(Event::Question {
                request,
                cancel: self.interrupt.clone(),
                answer,
            })
            .is_err()
        {
            return QuestionOutcome::Disconnected;
        }
        crate::platform::wake::signal();
        loop {
            if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
                return QuestionOutcome::Cancelled;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(outcome) => {
                    return if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst)
                    {
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
    fn progress(&mut self, steps: u32, phase: &str, text: &str) {
        self.body.progress(phase, steps);
        if !text.is_empty() {
            self.body.append(text);
        }
    }
    fn authorize(&mut self, call: &ToolCall, summary: &str, needs: bool) -> Result<(), Denial> {
        let cancel = self.interrupt.clone();
        self.authorize_with_cancel(call, summary, needs, &cancel)
    }
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        self.authorize_from(
            call,
            summary,
            needs,
            approval::CallProvenance::Structured,
            cancel,
        )
    }
    fn authorize_from(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs: bool,
        provenance: approval::CallProvenance,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        let interrupted = || {
            if self.interrupt.load(Ordering::SeqCst) {
                // The job's cancellation also ends its currently pending callback approval.
                cancel.store(true, Ordering::SeqCst);
            }
            cancel.load(Ordering::SeqCst)
        };
        if interrupted() {
            return Err(Denial::User);
        }
        let (answer, rx) = mpsc::channel();
        if self
            .tx
            .send(Event::Approve {
                call: call.clone(),
                summary: summary.to_owned(),
                needs,
                provenance,
                cancel: cancel.clone(),
                interrupt: self.interrupt.clone(),
                answer,
            })
            .is_err()
        {
            return Err(Denial::User);
        }
        crate::platform::wake::signal();
        loop {
            if interrupted() {
                return Err(Denial::User);
            }
            match rx.recv_timeout(Duration::from_millis(80)) {
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
        paths: &[std::path::PathBuf],
        unbounded: bool,
        cancel: &Arc<AtomicBool>,
        scope: subagents::MutationScope,
    ) -> Result<(), String> {
        let interrupted = || {
            if self.interrupt.load(Ordering::SeqCst) {
                cancel.store(true, Ordering::SeqCst);
            }
            cancel.load(Ordering::SeqCst)
        };
        if interrupted() {
            return Err("native mutation cancelled".into());
        }
        let (answer, rx) = mpsc::channel();
        if self
            .tx
            .send(Event::NativeMutation {
                original: original.clone(),
                tool: tool.to_owned(),
                paths: paths.to_vec(),
                unbounded,
                scope,
                cancel: cancel.clone(),
                interrupt: self.interrupt.clone(),
                answer,
            })
            .is_err()
        {
            return Err("native mutation controller unavailable".into());
        }
        crate::platform::wake::signal();
        loop {
            if interrupted() {
                return Err("native mutation cancelled".into());
            }
            match rx.recv_timeout(Duration::from_millis(80)) {
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
    fn policy_explanation(&mut self, call: &ToolCall, explanation: &approval::PolicyExplanation) {
        let _ = self
            .tx
            .send(Event::Policy(call.clone(), explanation.clone()));
        crate::platform::wake::signal();
    }
    fn notice(&mut self, message: &str) {
        let _ = self.tx.send(Event::Notice(message.to_owned()));
        crate::platform::wake::signal();
    }
    fn add_cost(&mut self, cost: f64) {
        let _ = self.tx.send(Event::Cost(cost));
        crate::platform::wake::signal();
    }
    fn take_messages(&mut self) -> Vec<String> {
        self.signals.clear();
        self.team
            .drain(&self.id)
            .into_iter()
            .map(|letter| format!("[Message from {}]\n{}", letter.from, letter.text))
            .collect()
    }
    fn live_coordination(&self) -> bool {
        true
    }
    fn tool_started(&mut self, call_id: &str, name: &str, registry: &Registry) {
        let steps = self.body.read(|n| {
            n.result
                .data
                .as_ref()
                .and_then(|d| d["steps"].as_u64())
                .unwrap_or(0) as u32
        });
        self.body.tool_started(name, steps);
        if let Some(tool) = registry.get(name) {
            self.active_tool
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(call_id.to_owned(), Arc::downgrade(tool));
        }
    }
    fn tool_finished(&mut self, call_id: &str) {
        self.active_tool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(call_id);
    }
    fn peer_message(&mut self, args: &Value) -> Result<Value, String> {
        route(&self.team, &self.id, args)
    }
}
/// What a new batch of live tasks is started from.
pub struct StartContext<'a> {
    pub config: &'a Config,
    pub workspace: &'a Workspace,
    pub ext: &'a Registry,
    pub layout: Option<&'a Layout>,
    pub session_id: &'a str,
    pub harness: &'a Harness,
    pub usage: Option<&'a crate::llm::usage::Scope>,
    pub context_cap: Option<u32>,
    pub memory: Option<&'a Arc<crate::storage::memory_backend::MemoryRuntime>>,
    pub plan_mode: bool,
    pub agents_messages: &'a [Value],
    pub handoff: subagents::Handoff,
}

/// Approval state lent to child tasks while the parent waits on them.
pub struct ApprovalGate<'a> {
    pub approval: Approval,
    pub read_only: bool,
    pub rules: &'a mut Vec<PermissionRule>,
    pub root: &'a std::path::Path,
    pub extra_roots: &'a [std::path::PathBuf],
    pub layout: Option<&'a Layout>,
    pub cost: &'a mut Option<f64>,
}

impl LiveTasks {
    /// Called at parent verification boundaries so configuration edits invalidate old contracts.
    pub(in crate::agent) fn revalidate_acceptance(
        &self,
        config: &crate::config::types::VerificationConfig,
    ) {
        for job in self.jobs.values() {
            if let Some(control) = &job.control
                && let Some(receipt) = control
                    .acceptance
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_mut()
            {
                receipt.revalidate(&control.workspace, config);
            }
        }
    }

    /// Typed controller receipt, never reconstructed from the worker's serialized report.
    pub(in crate::agent) fn acceptance_receipt(
        &self,
        id: &str,
    ) -> Option<crate::agent::verify::worker_acceptance::Acceptance> {
        self.jobs
            .get(id)?
            .control
            .as_ref()?
            .acceptance
            .lock()
            .ok()?
            .clone()
    }

    fn refresh_acceptance_report(&self, report: &mut Value) {
        if let Some(items) = report.as_array_mut() {
            for item in items {
                self.refresh_acceptance_report(item);
            }
            return;
        }
        if let Some(job) = report["id"].as_str().and_then(|id| self.jobs.get(id))
            && job.control.is_some()
        {
            report["acceptance"] = job.status()["acceptance"].clone();
        }
        if let Some(items) = report.get_mut("items").and_then(Value::as_array_mut) {
            for item in items {
                self.refresh_acceptance_report(item);
            }
        }
    }
    /// Finished worker results the model has not received yet. A result leaves this queue only when a
    /// request persists it into the session or a `status`/`wait` reply returns it, so it is delivered once.
    pub fn undelivered(&self) -> Vec<Value> {
        let mut reports = self.completed.clone();
        for report in &mut reports {
            self.refresh_acceptance_report(report);
        }
        reports
    }
    /// Records that one worker result reached the model; it is never queued again.
    pub fn deliver(&mut self, id: &str) {
        // A queued aggregate may contain only a suffix of a pool's items. Acknowledge exactly
        // those reports, never newer runs that have not appeared in the parent's request.
        let items = self
            .completed
            .iter()
            .find(|result| result["id"].as_str() == Some(id))
            .and_then(|result| result["items"].as_array())
            .cloned();
        if let Some(items) = items {
            self.acknowledge(&json!(items));
        }
        if let Some(job) = self.jobs.get_mut(id)
            && !job.running()
        {
            // A direct status read can deliver a result before its Done event is pumped.
            job.delivered = true;
        }
        self.completed
            .retain(|result| result["id"].as_str() != Some(id));
    }
    /// A model-facing `status` or `wait` reply carries finished jobs in full, so those results are delivered.
    pub fn acknowledge(&mut self, reply: &Value) {
        let jobs: Vec<&Value> = match reply.as_array() {
            Some(jobs) => jobs.iter().collect(),
            None => vec![reply],
        };
        for job in jobs {
            if let Some(items) = job.get("items").filter(|items| items.is_array()) {
                self.acknowledge(items);
                if let Some(id) = job["id"].as_str() {
                    self.deliver(id);
                }
            }
            if let (Some(id), Some(status)) = (job["id"].as_str(), job["status"].as_str())
                && status != "running"
            {
                self.deliver(id);
            }
        }
    }
    /// Polling replies keep typed reports intact but do not repeat already delivered narratives.
    /// Explicit targeted status uses `output` instead to recover the original preview.
    pub(in crate::agent) fn polling_output(&mut self, reply: Result<Value, String>) -> ToolOutput {
        let mut rendered = output(reply);
        if let Some(value) = &rendered.data {
            rendered.text = model_summary_view(value, &|record| {
                record["id"]
                    .as_str()
                    .and_then(|id| self.jobs.get(id))
                    .is_some_and(|job| {
                        let mut canonical = job.status();
                        // Item ordinals are pool presentation, not part of the worker report.
                        if let Some(item) = record.get("item") {
                            canonical["item"] = item.clone();
                        }
                        job.delivered && !job.running() && canonical == *record
                    })
            });
            self.acknowledge(value);
        }
        rendered
    }
    /// Ids of workers that have not settled yet; a turn that tries to finish waits for them.
    pub fn running_ids(&self) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.running() || job.worker.is_some())
            .map(|(id, _)| id.clone())
            .chain(self.pending_pool_ids())
            .collect()
    }
    /// Messages workers sent to the parent since the last call, in send order.
    pub fn take_parent_messages(&self) -> Vec<mailbox::Letter> {
        self.team.drain(mailbox::PARENT)
    }
    pub fn bind(&mut self, tree: crate::runtime::Tree) {
        if !self.tree.same(&tree) {
            self.cancel_all();
            self.tree.interrupt_open();
            self.completed.clear();
        }
        self.tree = tree;
    }
    /// The team bus, so tests can enlist a stand-in worker.
    #[cfg(test)]
    pub(in crate::agent) fn team(&self) -> &Team {
        &self.team
    }
    pub fn control(&self) -> LiveTaskControl {
        LiveTaskControl {
            jobs: self.registry.clone(),
            team: self.team.clone(),
        }
    }
    pub fn start(&mut self, ctx: StartContext<'_>, args: &Value) -> Result<Value, String> {
        self.start_inner(ctx, args, None)
    }
    pub(in crate::agent) fn start_with_promotion(
        &mut self,
        ctx: StartContext<'_>,
        args: &Value,
        promotion: Option<super::contracts::Promotion>,
    ) -> Result<Value, String> {
        self.start_inner_with_promotion(ctx, args, None, promotion)
    }
    /// Starts a worker, routing it onto a pool slot when `route` is set so the slot's backend and
    /// carried conversation are reused.
    fn start_inner(
        &mut self,
        ctx: StartContext<'_>,
        args: &Value,
        route: Option<SlotRoute>,
    ) -> Result<Value, String> {
        self.start_inner_with_promotion(ctx, args, route, None)
    }

    fn start_inner_with_promotion(
        &mut self,
        ctx: StartContext<'_>,
        args: &Value,
        mut route: Option<SlotRoute>,
        promotion: Option<super::contracts::Promotion>,
    ) -> Result<Value, String> {
        let StartContext {
            config,
            workspace,
            ext,
            layout,
            session_id,
            harness,
            usage,
            context_cap,
            memory,
            plan_mode,
            agents_messages,
            handoff,
        } = ctx;
        let session_id = session_id.to_owned();
        if !config.task.can_spawn_at(0) {
            return Err("task spawning is disabled by task.max_recursion_depth".into());
        }
        let config = subagents::with_file_agents(config);
        let mut resume = if args["action"] == "resume" {
            let id = args["id"].as_str().unwrap_or("");
            if let Some(job) = self.jobs.get(id)
                && (job.running() || job.worker.is_some())
            {
                return Err(
                    "worker is still settling; call task wait or status before resuming".into(),
                );
            }
            if !config.task.persist {
                return Err("worker continuation is disabled by task.persist".into());
            }
            let layout = layout.ok_or("resuming requires persistent session storage")?;
            Some(
                crate::agent::workers::worker_resume::load(
                    layout,
                    &session_id,
                    workspace.root(),
                    args["id"].as_str().unwrap_or(""),
                    args["message"].as_str().unwrap_or(""),
                )
                .map_err(|error| format!("{error:#}"))?,
            )
        } else {
            None
        };
        let effective = resume.as_ref().map_or(args, |resume| &resume.task).clone();
        let mut request = subagents::parse(&effective, &config)?;
        subagents::check_isolation_backends(&request.jobs, workspace, &config)?;
        subagents::seance::attach(
            &mut request.jobs,
            layout,
            Some(session_id.as_str()),
            &config,
            workspace.artifacts(),
        )?;
        if self
            .jobs
            .values()
            .filter(|j| j.running() || j.worker.is_some())
            .count()
            + request.jobs.len()
            > config
                .subagent_max_live_jobs
                .min(config.task.max_concurrency)
        {
            return Err(format!(
                "maximum {} live jobs; wait for completion first or increase subagent_max_live_jobs (up to 32)",
                config
                    .subagent_max_live_jobs
                    .min(config.task.max_concurrency)
            ));
        }
        // Resolve each job's role controls before pruning history or creating runtime nodes.
        // A rejected model profile must not leave a phantom running worker behind.
        let mut roles = Vec::with_capacity(request.jobs.len());
        for job in &request.jobs {
            let role = job.model_routes[0].config.clone();
            let job_harness = role
                .effective_harness(harness)
                .map_err(|e| format!("subagent model profile: {e:#}"))?;
            if let Some(saved) = &resume
                && (role.selection().display_name() != saved.model
                    || serde_json::to_value(config.agents.get(job.kind)).unwrap_or(Value::Null)
                        != saved.definition)
            {
                return Err("worker model or agent definition changed; restore its original settings or start a new worker".into());
            }
            let role = match resume.as_ref().and_then(|saved| saved.settings.as_ref()) {
                Some(settings) => {
                    control::apply_settings(&role, settings).map_err(|e| format!("{e:#}"))?
                }
                None => role,
            };
            roles.push((role, job_harness));
        }
        let workspace_identity =
            super::worker_resume::identity(workspace.root()).map_err(|error| error.to_string())?;
        // One directory scan per batch, not per worker. Resume keeps its original identity.
        if let Some(layout) = layout {
            for entry in crate::agent::workers::child_transcript::scan(layout, &session_id) {
                if let Some(number) = entry.header["id"]
                    .as_str()
                    .and_then(|id| id.strip_prefix("task-"))
                    .and_then(|id| id.parse::<u64>().ok())
                {
                    self.next = self.next.max(number);
                }
            }
        }
        while self.jobs.len() + request.jobs.len() > 256 {
            let oldest = self
                .jobs
                .iter()
                .filter(|(_, j)| !j.running() && j.worker.is_none())
                .min_by_key(|(id, _)| id.trim_start_matches("task-").parse::<u64>().unwrap_or(0))
                .map(|(id, _)| id.clone())
                .ok_or("live job history is full")?;
            self.jobs.remove(&oldest);
            self.registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&oldest);
        }
        let mut result = Vec::new();
        // All job contracts were validated above; runtime setup failures settle only that job.
        for (job, (role, harness)) in request.jobs.iter().zip(roles) {
            let promotion = promotion.clone();
            let mut harness = harness;
            let resume_messages = resume
                .as_ref()
                .map(|r| r.messages.clone())
                .unwrap_or_default();
            if let Some(saved) = &resume {
                harness.tools.retain(|tool| saved.tools.contains(tool));
            }
            self.next += 1;
            let id = if resume.is_some() {
                args["id"].as_str().unwrap_or("").to_owned()
            } else {
                format!("task-{}", self.next)
            };
            let resume_tools = resume.as_ref().map(|saved| saved.tools.clone());
            let resume_settings = resume.as_ref().and_then(|saved| saved.settings.clone());
            let original_task = effective["original_task"].as_str().unwrap_or(&job.prompt);
            let lease = match resume.take() {
                Some(saved) => Some(saved.lease),
                None => match layout
                    .filter(|_| config.task.persist)
                    .map(|layout| {
                        crate::agent::workers::worker_resume::lease(layout, &session_id, &id)
                    })
                    .transpose()
                {
                    Ok(lease) => lease,
                    Err(error) => {
                        result.push(json!({"id":id,"status":"failed","result":format!("worker lease acquisition failed: {error:#}"),"data":{"dial_effects":"none"}}));
                        continue;
                    }
                },
            };
            let body = self.tree.start("worker", &job.label, None);
            let mut assignment = json!({"task":job.prompt,"solution_space":job.solution_space,"agent":job.kind,"max_steps":job.max_steps,"name":job.label,"isolated":job.isolated});
            let mut task = json!({"task":job.prompt,"solution_space":job.solution_space,"agent":job.kind,"max_steps":job.max_steps,"name":job.label});
            if !job.acceptance_checks.is_empty() {
                assignment["acceptance_checks"] = json!(job.acceptance_checks);
                task["acceptance_checks"] = json!(job.acceptance_checks);
            }
            let original = effective
                .get("tasks")
                .and_then(Value::as_array)
                .and_then(|tasks| tasks.get(job.index))
                .unwrap_or(&effective);
            if let Some(skills) = original.get("skills") {
                task["skills"] = skills.clone();
            }
            if job.isolated {
                assignment["isolation"] = json!("copy");
                task["isolation"] = json!("copy");
            }
            if let Some(preference) = job.isolation_backend {
                assignment["isolation_backend"] = json!(preference);
                task["isolation_backend"] = json!(preference);
            }
            if let Some(schema) = &job.schema {
                task["output_schema"] = schema.clone();
            }
            if let Some(context) = &job.shared_context {
                assignment["context"] = json!(context);
                task["context"] = json!(context);
            }
            if let Some(profile) = &request.model_profile {
                assignment["model_profile"] = json!(profile);
                task["model_profile"] = json!(profile);
            }
            if let Some(packet) = &job.work_packet {
                let packet = json!({
                    "scope":packet.scope,
                    "interface":packet.interface,
                    "acceptance":packet.acceptance
                });
                assignment["work_packet"] = packet.clone();
                task["work_packet"] = packet;
            }
            if let Some(seance) = &job.seance {
                task["source_session"] = json!(seance.source);
                if let Some(model) = &seance.model {
                    task["model"] = json!(model);
                }
            }
            let resume_contract = json!({"version":1,"workspace":workspace_identity,"task":task,"original_task":original_task,"model":role.selection().display_name(),"definition":config.agents.get(job.kind)});
            let started_ms = epoch_ms();
            let status = json!({"id":id,"runtime_node":body.read(|node| node.id.clone()),"status":"running","name":job.label,"agent":job.kind,"model":role.selection().display_name(),"role":job.role,"model_source":job.model_routes[0].source,"model_explanation":job.model_routes[0].explanation,"phase":"starting","steps":0,"tools":0,"started_ms":started_ms,
                "assignment":assignment, "task":original_task, "instruction":job.prompt});
            body.settle(
                String::new(),
                Some(status.clone()),
                crate::runtime::Phase::Running,
            );
            let shared_body = body.clone();
            let config = config.clone();
            let usage = usage.cloned();
            let memory = memory.cloned();
            let agents_messages = agents_messages.to_vec();
            let handoff = handoff.clone();
            let workspace = workspace.clone();
            let ext = ext.clone();
            let layout = layout.cloned();
            let session_id = session_id.clone();
            let mut header = status.clone();
            header["started"] = json!(agent_timestamp());
            header["resume_contract"] = resume_contract.clone();
            if let Some(seance) = &job.seance {
                header["seance"] = json!({"source_session": seance.source});
            }
            let transcript = match create_transcript(
                config.task.persist,
                layout.as_ref(),
                &session_id,
                &id,
                header,
            ) {
                Ok(sink) => sink,
                Err(e) => {
                    let why = format!("{e:#}");
                    body.settle(
                        why.clone(),
                        Some(json!({"id":id,"status":"failed"})),
                        crate::runtime::Phase::Failed,
                    );
                    result.push(json!({"id":id,"status":"failed","result":why}));
                    continue;
                }
            };
            self.team.set_limit(config.task.inbox_limit);
            self.stall_secs = config.task.stall_secs;
            let interrupt = Arc::new(AtomicBool::new(false));
            let cancel = interrupt.clone();
            let tx = self.tx.clone();
            let done_id = id.clone();
            let active_tool: ActiveTool = Arc::default();
            let active = active_tool.clone();
            let signals = crate::tools::exec::signals::CallSignals::new();
            let interruptible = Arc::new(AtomicBool::new(false));
            let (slot_store, continuation) = match route.take() {
                Some(SlotRoute {
                    store,
                    continuation,
                }) => (Some(store), continuation),
                None => (None, None),
            };
            let settings = Arc::new(Mutex::new(control::SettingsSlot {
                accepted: resume_settings.unwrap_or_default(),
                ..Default::default()
            }));
            let control = Arc::new(ControlJob {
                interrupt: interrupt.clone(),
                active_tool: active_tool.clone(),
                body: body.clone(),
                finished: AtomicBool::new(false),
                base: role.clone(),
                settings: settings.clone(),
                workspace: workspace.clone(),
                acceptance: Mutex::new(None),
                owner_shutdown: AtomicBool::new(false),
            });
            let wake_signals = signals.clone();
            let wake_interruptible = interruptible.clone();
            let wake_active = active_tool.clone();
            let enlisted = self.team.enlist(
                &id,
                &job.label,
                Box::new(move || {
                    wake_signals.request_steering();
                    wake_signals.interrupt_waits();
                    if wake_interruptible.load(Ordering::SeqCst) {
                        cancel_active(&wake_active);
                    }
                }),
            );
            // Settle this job alone, as for a transcript failure: a batch error would leave a
            // phantom running worker and hide jobs this batch already started.
            if let Err(error) = enlisted {
                let why = error.to_string();
                body.settle(
                    why.clone(),
                    Some(json!({"id":id,"status":"failed"})),
                    crate::runtime::Phase::Failed,
                );
                result.push(json!({"id":id,"status":"failed","result":why}));
                continue;
            }
            self.registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.clone(), control.clone());
            let team = self.team.clone();
            let status_control = control.clone();
            let work: SlotWork = Box::new(move |slot: &mut SlotState| {
                let _lease = lease;
                let mut exit = WorkerExit {
                    id: done_id.clone(),
                    tx: tx.clone(),
                    control: control.clone(),
                    team: team.clone(),
                    reported: false,
                };
                let mut bridge = Bridge {
                    signals,
                    interruptible,
                    body: shared_body,
                    tx: tx.clone(),
                    interrupt: cancel.clone(),
                    team: team.clone(),
                    id: done_id.clone(),
                    active_tool: active,
                    transcript: transcript.clone(),
                    settings: settings.clone(),
                };
                let mut outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut request = match subagents::parse(&task, &config) {
                        Ok(r) => r,
                        Err(e) => return Outcome::failed(0, e),
                    };
                    // The carried conversation belongs to the slot's retained backend; an item whose
                    // config differs starts fresh instead of continuing another config's conversation.
                    let compatible = slot.compatible(&role);
                    let (messages, tools) = match (slot.carry.take(), continuation) {
                        (Some(carry), Some(assignment))
                            if resume_messages.is_empty() && compatible =>
                        {
                            let mut messages = carry.messages;
                            messages.push(json!({"role":"user","content":assignment}));
                            (messages, Some(carry.tools))
                        }
                        _ => (resume_messages, resume_tools),
                    };
                    request.jobs[0].resume_messages = messages;
                    request.jobs[0].resume_tools = tools;
                    request.jobs[0].promotion = promotion;
                    if let Err(why) = subagents::seance::attach(
                        &mut request.jobs,
                        layout.as_ref(),
                        Some(session_id.as_str()),
                        &config,
                        workspace.artifacts(),
                    ) {
                        return Outcome::failed(0, why);
                    }
                    handoff.apply(&mut request.jobs);
                    if let Err(why) = subagents::preload_skills(&mut request.jobs, layout.as_ref())
                    {
                        return Outcome::failed(0, why);
                    }
                    let env = Env {
                        config: &role,
                        workspace: &workspace,
                        ext: &ext,
                        layout: layout.as_ref(),
                        harness: &harness,
                        interrupt: &cancel,
                        usage: usage.as_ref(),
                        context_cap,
                        memory: memory.as_ref(),
                        plan_mode,
                        agents_messages: &agents_messages,
                    };
                    match slot.backend_for(&role) {
                        Ok(backend) => subagents::run_job(
                            env,
                            &request.jobs[0],
                            &mut bridge,
                            |env, approver| {
                                subagents::run_child(
                                    env,
                                    &request.jobs[0],
                                    &mut *backend,
                                    None,
                                    approver,
                                )
                            },
                        ),
                        Err(e) => Outcome::failed(0, format!("{e:#}")),
                    }
                }))
                .unwrap_or_else(|_| Outcome::failed(0, WORKER_LOST));
                let lost = outcome.status.text.as_str() == WORKER_LOST;
                slot.carry = outcome.conversation.take();
                let applied =
                    std::mem::take(&mut settings.lock().unwrap_or_else(|e| e.into_inner()).applied);
                if lost || applied {
                    slot.retire_backend();
                }
                let status = if outcome.status.phase == crate::runtime::Phase::Interrupted {
                    if control.owner_shutdown.load(Ordering::SeqCst) {
                        "interrupted"
                    } else {
                        "cancelled"
                    }
                } else {
                    outcome.status.wire()
                };
                let text = outcome.status.text;
                let mut value =
                    json!({"id":done_id,"status":status,"steps":outcome.steps,"result":text});
                value["inspections"] = json!(outcome.inspections);
                value["scheduling"] = json!(outcome.scheduling);
                value["acceptance"] = json!(outcome.acceptance);
                *control
                    .acceptance
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(outcome.acceptance);
                value["execution_id"] = json!(bridge.body.read(|node| node.id.clone()));
                value["assignment"] = bridge.body.read(|node| {
                    node.result
                        .data
                        .as_ref()
                        .map(|data| data["assignment"].clone())
                        .unwrap_or(Value::Null)
                });
                let route = bridge
                    .body
                    .read(|node| node.result.data.clone().unwrap_or(json!({})));
                for key in ["role", "model", "model_source", "model_explanation"] {
                    value[key] = route[key].clone();
                }
                if let Some(data) = outcome.data {
                    value["data"] = data;
                }
                if let Some(isolation) = outcome.isolation {
                    // A successful structured result must stay valid JSON.
                    if value.get("data").is_none() || status != "done" {
                        let text = value["result"].as_str().unwrap_or("");
                        value["result"] = json!(format!("{text}\n\n[{}]", isolation.describe()));
                    }
                    value["isolation_label"] = json!(isolation.describe());
                    value["isolation"] = json!(isolation);
                }
                if let Some(rules) = outcome.portable_rules {
                    value["portable_rules"] = rules;
                }
                let acceptance = value["assignment"]["work_packet"]["acceptance"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                value["projection"] = subagents::result_projection(
                    value["result"].as_str().unwrap_or(""),
                    value.get("data"),
                    &acceptance,
                    Some(&workspace),
                );
                if let Some(store) = workspace.artifacts() {
                    match subagents::retain_report(store, &value) {
                        Ok(uri) => value["artifact"] = json!(uri),
                        Err(e) => {
                            value["status"] = json!("failed");
                            value["artifact_error"] = json!(format!("{e:#}"));
                        }
                    }
                }
                let finished_ms = epoch_ms();
                value["finished_ms"] = json!(finished_ms);
                value["elapsed_ms"] = json!(finished_ms.saturating_sub(started_ms));
                if let Some(sink) = &transcript {
                    let artifacts = value["artifact"]
                        .as_str()
                        .map_or_else(|| json!([]), |uri| json!([["result", uri]]));
                    if let Err(error) = sink.finish(json!({"status":value["status"],"steps":outcome.steps,"finished":agent_timestamp(),"finished_ms":finished_ms,"elapsed_ms":value["elapsed_ms"],"artifacts":artifacts,"isolation":value.get("isolation_label").cloned(),"role":value["role"],"model":value["model"],"model_source":value["model_source"],"model_explanation":value["model_explanation"]})) {
                            value["status"] = json!("failed");
                            value["transcript_error"] = json!(format!("{error:#}"));
                        }
                }
                settle_result(&control.body, &value);
                drop(_lease);
                exit.reported = true;
                team.retire(&done_id, value["status"].as_str().unwrap_or("finished"));
                control.finished.store(true, Ordering::SeqCst);
                let _ = tx.send(Event::Done(done_id, value));
                crate::platform::wake::signal();
            });
            let store = slot_store.unwrap_or_default();
            let spawned = slot::run_on(store, work, format!("dial-{id}")).map_err(|e| {
                self.team.retire(&id, "failed");
                self.registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                let why = format!("starting live subagent: {e}");
                body.settle(
                    why.clone(),
                    Some(json!({"id":id,"status":"failed"})),
                    crate::runtime::Phase::Failed,
                );
                why
            });
            let worker = match spawned {
                Ok(worker) => worker,
                Err(why) => {
                    result.push(json!({"id":id,"status":"failed","result":why}));
                    continue;
                }
            };
            self.jobs.insert(
                id.clone(),
                JobState {
                    body,
                    control: Some(status_control),
                    interrupt,
                    worker: Some(worker),
                    active_tool,
                    delivered: false,
                },
            );
            result.push(status);
        }
        Ok(json!(result))
    }
    pub fn pump(
        &mut self,
        approval: Approval,
        policy: PumpPolicy<'_>,
        layout: Option<&Layout>,
        cost: &mut Option<f64>,
        ui: &mut dyn Ui,
    ) {
        let PumpPolicy {
            read_only,
            rules,
            root,
            extra_roots,
        } = policy;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::CapturePolicy {
                    cancel,
                    interrupt,
                    answer,
                } => {
                    let result =
                        if cancel.load(Ordering::SeqCst) || interrupt.load(Ordering::SeqCst) {
                            Err("memory capture cancelled".into())
                        } else {
                            Ok(!read_only && approval != Approval::Readonly)
                        };
                    let _ = answer.send(result);
                }
                Event::Question {
                    mut request,
                    cancel,
                    answer,
                } => {
                    // The live owner's current plan mode wins over the child's spawn-time settings.
                    if read_only {
                        request.timeout_seconds = 0;
                    }
                    let outcome = if cancel.load(Ordering::SeqCst) {
                        crate::agent::workflow::questions::QuestionOutcome::Cancelled
                    } else {
                        ui.ask_questions(&request, &cancel)
                    };
                    let _ = answer.send(outcome);
                }
                Event::Approve {
                    call,
                    summary,
                    needs,
                    provenance,
                    cancel,
                    interrupt,
                    answer,
                } => {
                    let result = approval::authorize_worker_call(
                        approval::WorkerPolicy {
                            approval,
                            rules,
                            root,
                            read_only,
                            cancel: Some(&cancel),
                            interrupt: Some(&interrupt),
                        },
                        &call,
                        &summary,
                        needs,
                        provenance,
                        &mut CancelUi {
                            inner: ui,
                            cancel: &cancel,
                            interrupt: &interrupt,
                        },
                    );
                    let _ = answer.send(result);
                }
                Event::NativeMutation {
                    original,
                    tool,
                    paths,
                    unbounded,
                    scope,
                    cancel,
                    interrupt,
                    answer,
                } => {
                    let mut result = if cancel.load(Ordering::SeqCst)
                        || interrupt.load(Ordering::SeqCst)
                    {
                        Err("native mutation cancelled".into())
                    } else {
                        match scope {
                            subagents::MutationScope::Workspace => {
                                approval::authorize_mutation_paths(
                                    approval::MutationPolicy {
                                        rules,
                                        root,
                                        extra_roots,
                                        read_only: read_only
                                            || matches!(approval, Approval::Readonly),
                                    },
                                    &original,
                                    &tool,
                                    &paths,
                                    unbounded,
                                )
                            }
                            subagents::MutationScope::SourceRead => {
                                approval::authorize_source_read_paths(
                                    rules,
                                    root,
                                    extra_roots,
                                    &original,
                                    &tool,
                                    &paths,
                                    unbounded,
                                )
                            }
                            subagents::MutationScope::ManagedMemory => {
                                if tool != original.name || unbounded {
                                    Err("managed memory authority does not match its declared operation".into())
                                } else {
                                    layout
                                        .ok_or_else(|| {
                                            "managed memory requires owning layout".into()
                                        })
                                        .and_then(|layout| {
                                            approval::authorize_managed_memory_paths(
                                                rules,
                                                layout,
                                                read_only || matches!(approval, Approval::Readonly),
                                                &original,
                                                &paths,
                                            )
                                        })
                                }
                            }
                        }
                    };
                    if cancel.load(Ordering::SeqCst) || interrupt.load(Ordering::SeqCst) {
                        cancel.store(true, Ordering::SeqCst);
                        result = Err("native mutation cancelled".into());
                    }
                    let _ = answer.send(result);
                }
                Event::Policy(call, explanation) => {
                    ui.policy_explanation(&call.id, &call.name, &explanation)
                }
                Event::Notice(text) => ui.background_notice(&text),
                Event::Cost(value) => *cost = Some(cost.unwrap_or(0.0) + value),
                Event::Done(id, value) => {
                    self.registry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    let pooled = self.pool_owns_job(&id);
                    if let Some(job) = self.jobs.get_mut(&id) {
                        // Workers normally publish directly; retain this fallback for interrupted
                        // owners and synthetic events without overwriting an already terminal body.
                        if job.running() {
                            settle_result(&job.body, &value);
                        }
                        if self.tree.owns(&job.body) {
                            ui.result_node(&job.body);
                            if !job.delivered && !pooled {
                                self.completed.push(job.status());
                            }
                        }
                        if let Some(worker) = job.worker.take() {
                            let _ = worker.join();
                        }
                    }
                }
            }
        }
        let now = epoch_ms();
        for id in self.scan_stalls(now) {
            ui.background_notice(&format!(
                "{id} has shown no progress for at least {}s; check its status, message it or cancel it.",
                self.stall_secs
            ));
        }
        self.advance_pools();
    }
    pub fn status(&self, id: Option<&str>) -> Result<Value, String> {
        match id {
            Some(id) if self.is_pool(id) => self.pool_report(id, false),
            Some(id) => self
                .jobs
                .get(id)
                .map(|j| self.watched(j))
                .ok_or_else(|| format!("unknown job {id}")),
            None => {
                let mut jobs: Vec<_> = self.jobs.iter().collect();
                jobs.sort_by_key(|(id, _)| {
                    id.trim_start_matches("task-").parse::<u64>().unwrap_or(0)
                });
                let mut entries: Vec<Value> =
                    jobs.into_iter().map(|(_, j)| self.watched(j)).collect();
                entries.extend(self.pool_entries());
                Ok(json!(entries))
            }
        }
        .map(|mut report| {
            self.refresh_acceptance_report(&mut report);
            report
        })
    }
    /// A job's status; a running one also reports `running_secs`, `idle_secs` and `health`
    /// (`active`, `slow` or `stalled`) so the parent can decide whether to keep waiting.
    fn watched(&self, job: &JobState) -> Value {
        let mut status = job.status();
        watch(&mut status, epoch_ms(), self.stall_secs);
        status
    }
    /// Records running workers that have gone quiet for `task.stall_secs`. Returns those newly
    /// stalled and queues them for the next `wait`. A worker that resumes or settles is forgotten,
    /// so a later stall is reported again.
    fn scan_stalls(&mut self, now_ms: u64) -> Vec<String> {
        let stall = self.stall_secs;
        let stalled: BTreeSet<String> = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.running()
                    && matches!(
                        health(&job.status(), now_ms, stall),
                        Some((_, _, "stalled"))
                    )
            })
            .map(|(id, _)| id.clone())
            .collect();
        let fresh: Vec<String> = stalled.difference(&self.flagged).cloned().collect();
        self.flagged = stalled;
        self.stall_alerts.retain(|id| self.flagged.contains(id));
        self.stall_alerts.extend(fresh.iter().cloned());
        fresh
    }
    /// Whether an unreported stall concerns `id` (any worker when `None`); reports it once.
    fn take_stall_alert(&mut self, id: Option<&str>) -> bool {
        let concerns = |alert: &String| id.is_none_or(|id| id == alert);
        let hit = self.stall_alerts.iter().any(concerns);
        self.stall_alerts.retain(|alert| !concerns(alert));
        hit
    }
    pub fn message(&self, id: &str, text: &str) -> Result<Value, String> {
        self.control().send(id, text).map_err(|e| e.to_string())
    }
    /// The parent's `message` tool: `args` carry `to` and `text`, as for a worker.
    pub fn message_call(&self, args: &Value) -> Result<Value, String> {
        route(&self.team, mailbox::PARENT, args)
    }
    pub fn cancel(&self, id: &str) -> Result<Value, String> {
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| format!("unknown job {id}"))?;
        job.interrupt.store(true, Ordering::SeqCst);
        cancel_active(&job.active_tool);
        Ok(json!({"id":id,"cancellation_requested":true}))
    }
    pub fn cancel_all(&mut self) {
        for id in self.jobs.keys() {
            let _ = self.cancel(id);
        }
        let pools: Vec<String> = self.pools.keys().cloned().collect();
        for id in pools {
            let _ = self.pool_close(&id);
        }
    }
    /// Waits for the selected workers. The second value says why the wait ended early, if it
    /// did: the user typed input, or a worker sent the parent a message. The workers keep
    /// running either way, and the message stays queued for the parent's next request.
    pub fn wait(
        &mut self,
        id: Option<&str>,
        seconds: u64,
        gate: ApprovalGate<'_>,
        interrupt: &AtomicBool,
        ui: &mut dyn Ui,
    ) -> Result<(Value, Option<WaitStop>), String> {
        let ApprovalGate {
            approval,
            read_only,
            rules,
            root,
            extra_roots,
            layout,
            cost,
        } = gate;
        self.status(id)?;
        let until = Instant::now() + Duration::from_secs(seconds.min(120));
        let waiter = crate::platform::wake::Waiter::new().map_err(|e| e.to_string())?;
        loop {
            self.pump(
                approval,
                PumpPolicy {
                    read_only,
                    rules,
                    root,
                    extra_roots,
                },
                layout,
                cost,
                ui,
            );
            let pending = self.pool_pending(id)
                || self.jobs.iter().any(|(name, j)| {
                    id.is_none_or(|id| id == name) && (j.running() || j.worker.is_some())
                });
            if interrupt.load(Ordering::SeqCst) {
                return Err("wait interrupted by the user".into());
            }
            if !pending || Instant::now() >= until {
                return self.status(id).map(|status| (status, None));
            }
            if ui.pending_input() {
                return self
                    .status(id)
                    .map(|status| (status, Some(WaitStop::Input)));
            }
            if self.team.has_mail(mailbox::PARENT) {
                return self
                    .status(id)
                    .map(|status| (status, Some(WaitStop::Message)));
            }
            if self.take_stall_alert(id) {
                return self
                    .status(id)
                    .map(|status| (status, Some(WaitStop::Stalled)));
            }
            ui.heartbeat();
            waiter
                .wait::<std::io::PipeReader>(
                    None,
                    None,
                    Some(
                        until
                            .saturating_duration_since(Instant::now())
                            .min(crate::agent::ui::chat::HEARTBEAT),
                    ),
                )
                .map_err(|e| e.to_string())?;
        }
    }
}
fn agent_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64)
}
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

/// Seconds a running worker has run and been silent, and its health: `stalled` after `stall_secs`
/// without progress, `slow` once it has run `SLOW_AFTER_STALLS` stall periods, else `active`.
/// `None` for a settled job, a job without timestamps, or when `stall_secs` is 0.
fn health(status: &Value, now_ms: u64, stall_secs: u64) -> Option<(u64, u64, &'static str)> {
    if status["status"] != "running" || stall_secs == 0 {
        return None;
    }
    let started = status["started_ms"].as_u64()?;
    let last = status["updated_ms"]
        .as_u64()
        .unwrap_or(started)
        .max(started);
    let running = now_ms.saturating_sub(started) / 1000;
    let idle = now_ms.saturating_sub(last) / 1000;
    let label = if idle >= stall_secs {
        "stalled"
    } else if running >= stall_secs.saturating_mul(SLOW_AFTER_STALLS) {
        "slow"
    } else {
        "active"
    };
    Some((running, idle, label))
}

/// Adds `running_secs`, `idle_secs` and `health` to a running job's status.
fn watch(status: &mut Value, now_ms: u64, stall_secs: u64) {
    if let Some((running, idle, label)) = health(status, now_ms, stall_secs) {
        status["running_secs"] = json!(running);
        status["idle_secs"] = json!(idle);
        status["health"] = json!(label);
    }
}

/// A progressing worker is reported `slow` after this many stall periods of run time.
const SLOW_AFTER_STALLS: u64 = 3;
impl Drop for LiveTasks {
    fn drop(&mut self) {
        // The owner is going away, not cancelling the work: workers stopped here stay restorable.
        for job in self.jobs.values() {
            if let Some(control) = &job.control {
                control.owner_shutdown.store(true, Ordering::SeqCst);
            }
        }
        self.cancel_all();
        for job in self.jobs.values_mut() {
            if let Some(worker) = job.worker.take() {
                let _ = worker.join();
            }
        }
    }
}
/// Why a live-task wait returned before its workers settled or its timeout passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum WaitStop {
    /// The user typed input.
    Input,
    /// A worker sent the parent a message.
    Message,
    /// A worker showed no progress for `task.stall_secs`.
    Stalled,
}
impl WaitStop {
    /// The note appended to the wait's result.
    pub(in crate::agent) fn note(self) -> &'static str {
        match self {
            Self::Input => STOPPED_FOR_INPUT,
            Self::Message => STOPPED_FOR_MESSAGE,
            Self::Stalled => STOPPED_FOR_STALL,
        }
    }
}
/// Appended to a pure wait that ended early because the user typed input.
pub(in crate::agent) const STOPPED_FOR_INPUT: &str = "\n[Stopped waiting early: the user sent a new message. Nothing was cancelled; the awaited work keeps running and its results arrive at request boundaries.]";
/// Appended to a live-task wait that ended early because a worker messaged the parent.
const STOPPED_FOR_MESSAGE: &str = "\n[Stopped waiting early: a worker sent you a message, delivered with your next request. Nothing was cancelled; the awaited work keeps running and its results arrive at request boundaries.]";
/// Appended to a live-task wait that ended early because a worker stopped making progress.
const STOPPED_FOR_STALL: &str = "\n[Stopped waiting early: a worker has shown no progress for the stall limit (see `health`, `idle_secs`, `phase` in its status). Nothing was cancelled. Keep waiting, message it, or cancel it and tell the user.]";

pub(in crate::agent) fn output(result: Result<Value, String>) -> ToolOutput {
    match result {
        Ok(value) => ToolOutput {
            execution: Default::default(),
            data: Some(value.clone()),
            text: model_summary(&value),
            is_error: false,
        },
        Err(text) => ToolOutput {
            execution: Default::default(),
            data: None,
            text,
            is_error: true,
        },
    }
}

/// Model-facing result projection. Observers retain the full typed state in `data`.
pub(in crate::agent) fn model_summary(value: &Value) -> String {
    model_summary_view(value, &|_| false)
}

fn model_summary_view(value: &Value, delivered: &impl Fn(&Value) -> bool) -> String {
    fn one(job: &Value, delivered: bool) -> String {
        let id = job["id"]
            .as_str()
            .or_else(|| job["name"].as_str())
            .unwrap_or("worker");
        let status = job["status"].as_str().unwrap_or("unknown");
        let steps = job["steps"].as_u64().unwrap_or(0);
        let mut text = format!("Worker {id}: {status} ({steps} requests)");
        if let Some(state) = job["acceptance"]["state"].as_str() {
            text.push_str(&format!(
                "\nController acceptance: {state}. {}",
                job["acceptance"]["reason"].as_str().unwrap_or("")
            ));
        }
        // Never elide a failed/unknown result or one without a retained full report.
        let compact = delivered
            && matches!(status, "done" | "completed")
            && job["artifact"].as_str().is_some()
            && job.get("artifact_error").is_none()
            && job.get("transcript_error").is_none();
        for (key, label) in [
            ("artifact_error", "Worker report retention failed"),
            ("transcript_error", "Worker transcript persistence failed"),
        ] {
            if let Some(error) = job[key].as_str() {
                let error = crate::agent::window::prompt::truncate_chars(error, 1000);
                text.push_str(&format!("\n[{label}: {error}]"));
            }
        }
        if matches!(status, "running" | "queued")
            && job
                .get("result")
                .is_none_or(|result| result.is_null() || result.as_str().is_some_and(str::is_empty))
            && job.get("projection").is_none()
        {
            if let Some(phase) = job["phase"].as_str().filter(|phase| *phase != status) {
                text.push_str(&format!("; {phase}"));
            }
            if let (Some(health), Some(idle)) = (job["health"].as_str(), job["idle_secs"].as_u64())
                && health != "active"
            {
                text.push_str(&format!("; {health}, idle {idle}s"));
            }
            return text;
        }
        let derived;
        let projection = if let Some(projection) = job.get("projection") {
            projection
        } else {
            let acceptance = job["assignment"]["work_packet"]["acceptance"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            derived = subagents::result_projection(
                job["result"].as_str().unwrap_or(""),
                job.get("data"),
                &acceptance,
                None,
            );
            &derived
        };
        let mut projection = projection.clone();
        if compact {
            if let Some(fields) = projection.as_object_mut() {
                for key in ["summary", "evidence_refs", "changed_paths", "provenance"] {
                    fields.remove(key);
                }
                fields.retain(|_, value| match value {
                    Value::Null => false,
                    Value::Array(items) => !items.is_empty(),
                    Value::Object(fields) => !fields.is_empty(),
                    _ => true,
                });
            }
            text.push_str(
                "\n[Report already delivered; task status with this id recovers the full preview.]",
            );
        }
        if !compact
            || projection
                .as_object()
                .is_none_or(|fields| !fields.is_empty())
        {
            text.push_str(&format!(
                "\nWorker result projection (claims, not verification):\n{projection}"
            ));
        }
        if let Some(result) = job.get("result").filter(|_| !compact) {
            let report = result
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| result.to_string());
            if !report.trim().is_empty() {
                text.push('\n');
                text.push_str(&crate::agent::window::prompt::truncate_chars(&report, 5000));
            }
        }
        if let Some(uri) = job["artifact"].as_str() {
            text.push_str(&format!("\n[Complete worker report: {uri}]"));
        }
        if let Some(isolation) = job.get("isolation") {
            text.push_str(&format!("\nIsolation result: {isolation}"));
        }
        text
    }
    if let Some(items) = value["items"].as_array() {
        let id = value["id"].as_str().unwrap_or("pool");
        let state = value["state"].as_str().unwrap_or("unknown");
        let mut text = format!("Worker pool {id}: {state}; counts: {}", value["counts"]);
        for item in items {
            text.push_str("\n\n");
            text.push_str(&one(item, delivered(item)));
        }
        return text;
    }
    if let Some(jobs) = value.as_array().or_else(|| value["jobs"].as_array()) {
        if jobs.is_empty() {
            return "No workers.".into();
        }
        return jobs
            .iter()
            .map(|job| one(job, delivered(job)))
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    if value.get("status").is_some() {
        return one(value, delivered(value));
    }
    value.to_string()
}
/// Parses a `message` tool call from `from` (`parent` or a worker id). Omitting `to` lists the
/// recipients the caller can reach; otherwise the reply holds one receipt per recipient (`queued`,
/// `refused` with a reason, or `finished` for a recently finished worker), and an unknown
/// recipient is an error listing the live ones. The parent and workers share this path.
pub(in crate::agent) fn route(team: &Team, from: &str, args: &Value) -> Result<Value, String> {
    match args.get("to") {
        None | Some(Value::Null) => Ok(json!({"recipients": team.roster(from)})),
        Some(Value::String(to)) if !to.trim().is_empty() => {
            let text = args["text"].as_str().ok_or("message requires text")?;
            team.send(from, to.trim(), text)
                .map(|receipt| receipt.json())
        }
        Some(_) => Err("to must be a non-empty recipient name".into()),
    }
}

/// Tool schema shared by workers and the parent: `message` sends text to a live peer. With `peers`
/// (the top-level session with project coordination) it also names other Dial sessions; only the
/// descriptions differ, so the contract identity is the same either way.
pub(in crate::agent) fn message_schema(peers: bool) -> Value {
    let (sessions, to) = if peers {
        (
            " A 32-hex peer id (see `peers`) reaches that Dial session in this checkout; `all` never does.",
            "Recipient: a live task id, a unique live label, `parent`, `all` (this session's workers), or a peer id of another Dial session. Omit to list recipients and live peers.",
        )
    } else {
        (
            "",
            "Recipient: a live task id, a unique live label, `parent`, or `all`. Omit to list recipients.",
        )
    };
    json!({"type":"function","function":{"name":"message","description":format!("Send text to a live task id/label, `parent`, or `all` (other workers); omit `to` to list recipients.{sessions} Receipts: queued, refused (reason), or finished (collect via task). Text only; grants no authority. Delivered next step, in order."),"parameters":{"type":"object","properties":{"to":{"type":"string","description":to},"text":{"type":"string","description":"Message body, at most 32768 bytes."}},"additionalProperties":false}}})
}

pub mod control;
mod pool;
mod slot;
use slot::{SlotRoute, SlotState, SlotWork};
#[cfg(test)]
mod tests;
