//! Selected-memory work remains owned by the actual session. Workers cannot authorize themselves.
use super::{Agent, Ui, approval, compaction, compaction_runtime, make_backend, memory_authority};
use crate::{
    llm::{
        self, Chat, ToolCall,
        usage::{ExternalUsageReceipt, Scope},
    },
    storage::memory_backend::{
        MemoryModelLimits, MemoryModelRequest, MemoryModelRunner, Operation,
    },
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    time::{Duration, Instant},
};

/// `/memory` with no memory backend selected.
const MEMORY_OFF: &str = "Memory is off: nothing is remembered or recalled. Choose a backend in /settings → Memory or with /setup memory.";
const MEMORY_OFF_STATUS: &str = "Memory is off: nothing is remembered or recalled.";

/// `/memory help` for the provenance backend.
const PROVENANCE_HELP: &str =
    "Provenance memory: facts with history and expiry, found again by keyword.
/memory                    status and the newest remembered facts
/memory search <words>     find remembered facts (any other words also search)
/memory clear [project|global]   forget this project's facts, or the facts shared by every project
/memory help               this help
The agent saves and corrects facts with its retain, recall, forget and memory_update tools.
Choose another backend in /settings → Memory or with /setup memory.";

#[derive(Default)]
pub(super) struct Receipts {
    next: u64,
    pending: BTreeMap<u64, ExternalUsageReceipt>,
}
struct Runner<'a, 'cfg> {
    agent: &'a mut Agent<'cfg>,
    ui: &'a mut dyn Ui,
    scope: Scope,
    receipts: &'a mut Receipts,
    origin: Option<&'a ToolCall>,
    frozen: Option<&'a crate::config::types::Config>,
}
impl Runner<'_, '_> {
    fn check(&self, op: &Operation) -> Result<()> {
        op.check()?;
        ensure!(
            !self
                .agent
                .interrupt
                .load(std::sync::atomic::Ordering::SeqCst),
            "memory owner interrupted"
        );
        ensure!(
            self.agent
                .memory_controller
                .active_capture
                .as_ref()
                .is_none_or(|cancel| !cancel.load(std::sync::atomic::Ordering::SeqCst)),
            "private memory capture cancelled"
        );
        ensure!(
            super::memory_authority_matches(&self.agent.session, &op.authority, 0),
            "memory callback lost its live session authority"
        );
        if let Some(config) = self.frozen {
            ensure!(
                self.agent.auth_config.same_model_identity(config)
                    && self.agent.auth_config.model_roles == config.model_roles
                    && self.agent.auth_config.provider_cache_namespace
                        == config.provider_cache_namespace,
                "memory callback belongs to superseded model/account configuration"
            );
        }
        ensure!(
            self.agent
                .config
                .budget
                .max_session_cost_usd
                .is_none_or(|cap| cap <= 0.0 || self.agent.session_cost().unwrap_or(0.0) < cap),
            "session cost boundary reached"
        );
        ensure!(
            self.agent.config.budget.max_turn_cost_usd.is_none_or(
                |cap| cap <= 0.0 || self.scope.observed_turn_cost().unwrap_or(0.0) < cap
            ),
            "original turn cost boundary reached"
        );
        Ok(())
    }
    fn permit(&self, op: &Operation, fallback: &str, writes: bool) -> Result<()> {
        self.check(op)?;
        let automatic = ToolCall {
            id: String::new(),
            name: fallback.into(),
            arguments: Value::Null,
            malformed_arguments: false,
        };
        let call = self.origin.unwrap_or(&automatic);
        ensure!(
            !approval::denied_by_rule(&self.agent.always, call, self.agent.workspace.root()),
            "selected memory operation denied by its actual tool policy"
        );
        if writes {
            ensure!(
                !self.agent.plan_mode
                    && self.agent.approval != crate::config::types::Approval::Readonly,
                "memory mutation denied by read-only execution policy"
            );
        }
        Ok(())
    }
    fn backend(
        &mut self,
        role: &str,
        model: Option<&str>,
    ) -> Result<(crate::config::types::Config, String)> {
        use sha2::{Digest, Sha256};
        let mut config = self
            .frozen
            .unwrap_or(&self.agent.auth_config)
            .memory_role_config(role, model)?;
        self.ui.role(role, &config.selection().display_name());
        let key = format!(
            "memory:{role}:{:x}",
            Sha256::digest(serde_json::to_vec(&config.selection())?)
        );
        if !self.agent.role_backends.contains_key(&key) {
            config.provider_cache_namespace = Some(compaction_runtime::side_request_namespace(
                self.agent.session.id(),
            )?);
            self.agent
                .role_backends
                .insert(key.clone(), make_backend(&config)?);
        }
        if let Some(id) = self.agent.role_backends[&key].account_id()
            && let Some(endpoint) = &mut config.endpoint
        {
            endpoint.account_id = Some(id.to_owned());
        }
        Ok((config, key))
    }
}
impl MemoryModelRunner for Runner<'_, '_> {
    fn heartbeat(&mut self, op: &Operation) -> Result<()> {
        self.ui.heartbeat();
        let capture = self.agent.memory_controller.active_capture.as_ref();
        let yield_capture = capture.is_some() && !self.ui.maintenance_ready();
        let yield_maintenance = self.frozen.is_some()
            && (!self.ui.maintenance_ready() || self.agent.maintenance_has_live_work());
        if self
            .agent
            .interrupt
            .load(std::sync::atomic::Ordering::SeqCst)
            || yield_capture
            || yield_maintenance
        {
            op.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(cancel) = capture {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.check(op)
    }
    fn limits(
        &mut self,
        role: &str,
        model: Option<&str>,
        system: &str,
        output: u32,
        op: &Operation,
    ) -> Result<MemoryModelLimits> {
        self.check(op)?;
        let (config, key) = self.backend(role, model)?;
        let backend = &self.agent.role_backends[&key];
        let window = config.context_window.min(
            backend
                .model_profile()
                .and_then(|p| p.context_window)
                .unwrap_or(u32::MAX),
        );
        let output = backend.output_budget(output).min(
            backend
                .model_profile()
                .and_then(|p| p.max_tokens)
                .filter(|n| *n > 0)
                .unwrap_or(u32::MAX),
        );
        let mut chat = Chat {
            messages: vec![json!({"role":"system","content":system})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: output,
            seed: None,
        };
        let selection = config.selection();
        let tokens = compaction::request_tokens_with_policy(
            &chat,
            &config.compaction,
            crate::config::compaction::CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: Some(role),
            },
        );
        ensure!(
            tokens.saturating_add(u64::from(output)) < u64::from(window),
            "memory system and output reserve exceed selected model window"
        );
        self.check(op)?;
        chat.messages.push(json!({"role":"user","content":""}));
        let skeleton = crate::llm::input_bytes(&chat)?;
        let capacity = self
            .agent
            .config
            .memory
            .request_max_bytes
            .checked_sub(skeleton)
            .context("memory system exceeds configured byte capacity")?;
        Ok(MemoryModelLimits {
            context_window: window as usize,
            max_output_tokens: output,
            max_input_bytes: capacity,
        })
    }
    fn complete(&mut self, request: MemoryModelRequest) -> Result<String> {
        self.check(&request.operation)?;
        let writes = self
            .origin
            .is_none_or(|call| matches!(call.name.as_str(), "learn" | "retain" | "forget"));
        self.permit(&request.operation, "learn", writes)?;
        let (config, key) = self.backend(&request.role, request.model_override.as_deref())?;
        let backend = &self.agent.role_backends[&key];
        let window = config.context_window.min(
            backend
                .model_profile()
                .and_then(|p| p.context_window)
                .unwrap_or(u32::MAX),
        );
        let output = backend.output_budget(request.max_output_tokens).min(
            backend
                .model_profile()
                .and_then(|p| p.max_tokens)
                .filter(|n| *n > 0)
                .unwrap_or(u32::MAX),
        );
        let chat = Chat {
            messages: vec![
                json!({"role":"system","content":request.system}),
                json!({"role":"user","content":request.input}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: output,
            seed: None,
        };
        ensure!(
            crate::llm::input_bytes(&chat)? <= self.agent.config.memory.request_max_bytes,
            "complete memory request exceeds configured byte capacity"
        );
        let selection = config.selection();
        let tokens = compaction::request_tokens_with_policy(
            &chat,
            &config.compaction,
            crate::config::compaction::CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: Some(&request.role),
            },
        );
        ensure!(
            tokens.saturating_add(u64::from(output)) <= u64::from(window),
            "complete memory request and actual output reserve exceed selected model window"
        );
        self.check(&request.operation)?;
        let rewritten = self.agent.provider_request_with(&chat, self.ui)?;
        if let Some(main) = &mut self.agent.backend {
            main.release();
        }
        let body = self.agent.session.runtime().start(
            "memory-model",
            &request.role,
            self.agent.session.branch_leaf().map(str::to_owned),
        );
        let op = request.operation;
        let mut backend = self
            .agent
            .role_backends
            .remove(&key)
            .expect("memory backend was prepared");
        backend.set_usage_scope(Some(self.scope.clone()));
        let maintenance = self.frozen.is_some();
        let capture_cancel = self.agent.memory_controller.active_capture.clone();
        let agent = &*self.agent;
        let started = std::time::Instant::now();
        let result = super::chat_with_ui(
            &mut backend,
            None,
            rewritten.as_ref().unwrap_or(&chat),
            super::ChatOptions {
                timeout: op.timeout_secs(request.timeout_secs),
                render_text: false,
                context_tokens: None,
                policy: None,
                recovery: Some(&config.request_recovery),
            },
            op.cancel.clone(),
            self.ui,
            &mut |ui| {
                if op.check().is_err()
                    || agent.interrupt.load(std::sync::atomic::Ordering::SeqCst)
                    || ((maintenance || capture_cancel.is_some()) && !ui.maintenance_ready())
                    || (maintenance && agent.maintenance_has_live_work())
                {
                    op.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                    if let Some(cancel) = &capture_cancel {
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            },
        );
        agent.record_aux_request(&backend, &result, started);
        backend.release();
        let received = result.is_ok();
        let result = result.and_then(|reply| {
            self.check(&op)?;
            ensure!(
                reply.finish_reason != "length" && reply.tool_calls.is_empty(),
                "memory model returned truncated output or tool calls"
            );
            Ok(reply.text)
        });
        let result = if received && result.is_err() {
            backend.record_discarded_response().and(result)
        } else {
            result
        };
        backend.set_usage_scope(None);
        self.agent.role_backends.insert(key, backend);
        let (total, turn) = self.scope.snapshot();
        body.settle(
            if result.is_ok() {
                "Memory model completed"
            } else {
                "Memory model result rejected"
            }
            .into(),
            Some(json!({"usage_total":total,"usage_turn":turn,"authority":op.authority})),
            if result.is_ok() {
                crate::runtime::Phase::Completed
            } else {
                crate::runtime::Phase::Failed
            },
        );
        result
    }
    fn authorize_external(&mut self, op: &Operation, purpose: &str) -> Result<u64> {
        let writes = !matches!(
            purpose,
            "recall"
                | "reflect"
                | "mental-model list"
                | "mental-model show"
                | "mental-model history"
        );
        self.permit(op, if writes { "retain" } else { "recall" }, writes)?;
        let receipt = self.scope.reserve_external(format!(
            "memory:{}",
            self.agent.config.memory.backend.as_str()
        ))?;
        self.receipts.next = self
            .receipts
            .next
            .checked_add(1)
            .context("memory receipt identity overflow")?;
        let id = self.receipts.next;
        self.receipts.pending.insert(id, receipt);
        Ok(id)
    }
    fn external_started(&mut self, op: &Operation, id: u64) -> Result<()> {
        self.check(op)?;
        self.receipts
            .pending
            .get_mut(&id)
            .context("unknown memory receipt")?
            .mark_started()
    }
    fn account_external(
        &mut self,
        op: &Operation,
        id: u64,
        usage: &Value,
        accepted: bool,
    ) -> Result<()> {
        let current = self.check(op);
        self.receipts
            .pending
            .remove(&id)
            .context("unknown or already settled memory receipt")?
            .account(usage, accepted, current.is_err())?;
        current
    }
    fn abandon_external(&mut self, _op: &Operation, id: u64) -> Result<()> {
        drop(
            self.receipts
                .pending
                .remove(&id)
                .context("unknown or already settled memory receipt")?,
        );
        Ok(())
    }
    fn authorize_managed_write(&mut self, op: &Operation, targets: &[PathBuf]) -> Result<()> {
        self.permit(op, "learn", true)?;
        let layout = self
            .agent
            .layout
            .as_ref()
            .context("managed memory requires owning layout")?;
        let automatic = ToolCall {
            id: String::new(),
            name: "learn".into(),
            arguments: Value::Null,
            malformed_arguments: false,
        };
        let call = self.origin.unwrap_or(&automatic);
        approval::authorize_managed_memory_paths(&self.agent.always, layout, false, call, targets)
            .map_err(anyhow::Error::msg)?;
        self.check(op)
    }
}

enum Event {
    Heartbeat(Operation, SyncSender<Result<()>>),
    Limits(
        String,
        Option<String>,
        String,
        u32,
        Operation,
        SyncSender<Result<MemoryModelLimits>>,
    ),
    Complete(MemoryModelRequest, SyncSender<Result<String>>),
    Authorize(Operation, String, SyncSender<Result<u64>>),
    Started(Operation, u64, SyncSender<Result<()>>),
    Account(Operation, u64, Value, bool, SyncSender<Result<()>>),
    Abandon(Operation, u64, SyncSender<Result<()>>),
    Write(Operation, Vec<PathBuf>, SyncSender<Result<()>>),
    Done(Result<()>),
}
struct Proxy {
    events: Sender<Event>,
}
fn response<T>(rx: Receiver<Result<T>>, op: &Operation) -> Result<T> {
    loop {
        op.check()?;
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => anyhow::bail!("memory owner disconnected"),
        }
    }
}
impl MemoryModelRunner for Proxy {
    fn heartbeat(&mut self, op: &Operation) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events.send(Event::Heartbeat(op.clone(), tx))?;
        response(rx, op)
    }
    fn limits(
        &mut self,
        role: &str,
        model: Option<&str>,
        system: &str,
        output: u32,
        op: &Operation,
    ) -> Result<MemoryModelLimits> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events.send(Event::Limits(
            role.into(),
            model.map(str::to_owned),
            system.into(),
            output,
            op.clone(),
            tx,
        ))?;
        response(rx, op)
    }
    fn complete(&mut self, request: MemoryModelRequest) -> Result<String> {
        let op = request.operation.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.events.send(Event::Complete(request, tx))?;
        response(rx, &op)
    }
    fn authorize_external(&mut self, op: &Operation, purpose: &str) -> Result<u64> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events
            .send(Event::Authorize(op.clone(), purpose.into(), tx))?;
        response(rx, op)
    }
    fn external_started(&mut self, op: &Operation, id: u64) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events.send(Event::Started(op.clone(), id, tx))?;
        response(rx, op)
    }
    fn account_external(
        &mut self,
        op: &Operation,
        id: u64,
        usage: &Value,
        accepted: bool,
    ) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events
            .send(Event::Account(op.clone(), id, usage.clone(), accepted, tx))?;
        response(rx, op)
    }
    fn abandon_external(&mut self, op: &Operation, id: u64) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events.send(Event::Abandon(op.clone(), id, tx))?;
        response(rx, op)
    }
    fn authorize_managed_write(&mut self, op: &Operation, targets: &[PathBuf]) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.events
            .send(Event::Write(op.clone(), targets.to_vec(), tx))?;
        response(rx, op)
    }
}
pub(super) struct Worker {
    events: Receiver<Event>,
    scope: Scope,
    receipts: Receipts,
    thread: std::thread::JoinHandle<()>,
    memory: std::sync::Arc<crate::storage::memory_backend::MemoryRuntime>,
    frozen: crate::config::types::Config,
    body: Option<crate::runtime::Body>,
}
impl Worker {
    fn settle(&mut self, phase: crate::runtime::Phase) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Account(_, id, usage, accepted, tx) => {
                    let result = self
                        .receipts
                        .pending
                        .remove(&id)
                        .context("unknown retired memory receipt")
                        .and_then(|receipt| receipt.account(&usage, accepted, true));
                    let _ = tx.send(result);
                }
                Event::Abandon(_, id, tx) => {
                    self.receipts.pending.remove(&id);
                    let _ = tx.send(Ok(()));
                }
                _ => {}
            }
        }
        self.receipts.pending.clear();
        let (total, turn) = self.scope.snapshot();
        if let Some(body) = self.body.take() {
            body.settle(
                "Selected memory maintenance settled".into(),
                Some(json!({"usage_total":total,"usage_turn":turn})),
                phase,
            );
        }
    }
}
#[derive(Default)]
pub(super) struct Controller {
    worker: Option<Worker>,
    last_start: Option<Instant>,
    pub(super) capture: Option<super::memory_capture::Capture>,
    pub(super) active_capture: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(super) compaction_advice: Option<(crate::storage::memory_backend::MemoryAuthority, String)>,
}
impl Controller {
    pub(super) fn has_worker(&self) -> bool {
        self.worker.is_some()
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.retire();
        }
        if let Some(cancel) = &self.active_capture {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(worker) = &mut self.worker {
            worker.memory.cancel();
            worker.settle(crate::runtime::Phase::Interrupted);
        }
    }
}
impl Agent<'_> {
    pub(super) fn memory_foreground_pending(&self, busy: bool) -> Result<bool> {
        if busy
            || self.hermetic
            || self.plan_mode
            || self.approval == crate::config::types::Approval::Readonly
            || self.interrupt.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(false);
        }
        if let Some(worker) = &self.memory_controller.worker {
            return Ok(worker.frozen.memory_uses_local_compute());
        }
        if self.maintenance_has_live_work() {
            return Ok(false);
        }
        if self
            .memory_controller
            .capture
            .as_ref()
            .is_some_and(|capture| capture.needs_compute())
        {
            return Ok(true);
        }
        Ok(self.memory_runtime.is_some()
            && self
                .memory_controller
                .last_start
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(30))
            && self.auth_config.memory_uses_local_compute())
    }
    fn retire_capture_for_clear(&mut self) {
        if let Some(capture) = self.memory_controller.capture.take() {
            capture.retire();
        }
        if let Some(cancel) = &self.memory_controller.active_capture {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.memory_controller.compaction_advice = None;
    }
    pub fn selected_memory_command(&mut self, input: &str, ui: &mut dyn Ui) -> Result<Value> {
        if let Some(memory) = self.memory.clone() {
            if input.trim() == "help" {
                return Ok(json!({"backend":"provenance","help":PROVENANCE_HELP}));
            }
            if matches!(input.trim(), "" | "status") {
                let recent = memory.recall("", None, 20, 24000)?;
                let items: Vec<Value> = recent
                    .entries
                    .iter()
                    .take(5)
                    .map(|entry| {
                        json!({"content":entry.content,"scope":entry.scope,"updated_at":entry.updated_at})
                    })
                    .collect();
                return Ok(
                    json!({"backend":"provenance","active":true,"count":recent.entries.len(),"truncated":recent.truncated,"recent":items,"message":"Facts are recorded as sessions run."}),
                );
            }
            if input == "clear" || input.starts_with("clear ") {
                use crate::storage::provenance::Scope;
                let scope = match input {
                    "clear" | "clear project" => Scope::Project,
                    "clear global" => Scope::Global,
                    _ => anyhow::bail!("usage: /memory clear [project|global]"),
                };
                let call = ToolCall {
                    id: String::new(),
                    name: "forget".into(),
                    arguments: json!({"scope":if scope == Scope::Global {"global"}else{"project"}}),
                    malformed_arguments: false,
                };
                ensure!(
                    !self.plan_mode && self.approval != crate::config::types::Approval::Readonly,
                    "memory clear denied by read-only policy"
                );
                ensure!(
                    !approval::denied_by_rule(&self.always, &call, self.workspace.root()),
                    "memory clear denied by forget policy"
                );
                if self.approval != crate::config::types::Approval::Auto {
                    ensure!(
                        matches!(
                            ui.approve_with_cancel("memory", input, self.interrupt.as_ref()),
                            super::Decision::Allow
                                | super::Decision::AllowAlways
                                | super::Decision::AllowProject
                        ),
                        "memory clear declined"
                    );
                }
                self.retire_capture_for_clear();
                return Ok(
                    json!({"backend":"provenance","cleared":memory.clear_scope(scope, self.session.id())?}),
                );
            }
            let query = input.strip_prefix("search ").unwrap_or(input);
            let call = ToolCall {
                id: String::new(),
                name: "recall".into(),
                arguments: json!({"query":query}),
                malformed_arguments: false,
            };
            ensure!(
                !approval::denied_by_rule(&self.always, &call, self.workspace.root()),
                "memory search denied by recall policy"
            );
            return Ok(serde_json::to_value(
                memory.recall(query, None, 20, 24000)?,
            )?);
        }
        let Some(memory) = self.memory_runtime.clone() else {
            use crate::config::memory_backend::MemoryBackendId;
            let off =
                !self.config.memory.enabled || self.config.memory.backend == MemoryBackendId::Off;
            anyhow::ensure!(off, "selected memory backend is not active");
            return match input.trim() {
                "" | "status" => {
                    Ok(json!({"backend":"off","active":false,"message":MEMORY_OFF_STATUS}))
                }
                "help" => Ok(json!({"backend":"off","help":MEMORY_OFF})),
                _ => anyhow::bail!("{MEMORY_OFF}"),
            };
        };
        memory.stop_maintenance();
        let authority = memory_authority(&self.session, 0);
        memory.bind(authority.clone());
        let mut receipts = Receipts::default();
        let scope = self
            .usage_scope
            .worker()
            .for_purpose(llm::usage::Purpose::Memory);
        // Semantic forget/update/rebuild change stored facts (C2), so they take the same approval
        // and read-only checks as clear and save.
        let mutating = matches!(
            input.split_whitespace().next(),
            Some("clear" | "save" | "enqueue" | "forget" | "update" | "rebuild")
        ) || input.starts_with("mm seed")
            || input.starts_with("mm delete")
            || input.starts_with("mm refresh");
        let origin = ToolCall {
            id: String::new(),
            name: if mutating { "learn" } else { "recall" }.into(),
            arguments: json!({"command":input}),
            malformed_arguments: false,
        };
        let mut runner = Runner {
            agent: self,
            ui,
            scope,
            receipts: &mut receipts,
            origin: Some(&origin),
            frozen: None,
        };
        ensure!(
            !approval::denied_by_rule(&runner.agent.always, &origin, runner.agent.workspace.root()),
            "memory command denied by selected tool policy"
        );
        ensure!(
            !mutating
                || (!runner.agent.plan_mode
                    && runner.agent.approval != crate::config::types::Approval::Readonly),
            "memory command denied by read-only execution policy"
        );
        if mutating && runner.agent.approval != crate::config::types::Approval::Auto {
            ensure!(
                matches!(
                    runner
                        .ui
                        .approve_with_cancel("memory", input, runner.agent.interrupt.as_ref()),
                    super::Decision::Allow
                        | super::Decision::AllowAlways
                        | super::Decision::AllowProject
                ),
                "memory command declined"
            );
        }
        if input.trim() == "clear" {
            runner.agent.retire_capture_for_clear();
        }
        memory.command(&authority, input, &mut runner)
    }
    pub(super) fn selected_memory_tool(
        &mut self,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> Result<Value> {
        ensure!(
            matches!(call.name.as_str(), "recall" | "reflect")
                || (!self.plan_mode && self.approval != crate::config::types::Approval::Readonly),
            "memory mutation denied by read-only execution policy"
        );
        let memory = self
            .memory_runtime
            .clone()
            .context("selected memory backend is not active")?;
        let authority = memory_authority(&self.session, 0);
        memory.bind(authority.clone());
        let mut receipts = Receipts::default();
        let scope = self
            .usage_scope
            .worker()
            .for_purpose(llm::usage::Purpose::Memory);
        memory.execute_tool(
            &authority,
            &call.name,
            &call.arguments,
            &mut Runner {
                agent: self,
                ui,
                scope,
                receipts: &mut receipts,
                origin: Some(call),
                frozen: None,
            },
        )
    }
    pub(super) fn prepare_compaction_memory(
        &mut self,
        ui: &mut dyn Ui,
        live_backend: Option<&mut crate::llm::provider::Backend>,
    ) {
        self.memory_controller.compaction_advice = None;
        if let Some(capture) = self.memory_controller.capture.take() {
            capture.retire();
        }
        // Every selected runtime is consulted; backends without compaction recall return None.
        if self.hermetic {
            return;
        }
        let Some(memory) = self.memory_runtime.clone() else {
            return;
        };
        memory.stop_maintenance();
        // Only Hindsight's compaction recall ever released the local model; semantic recall is a
        // local index search that needs no chat model, so the loaded model stays resident.
        if matches!(self.config.memory.backend.as_str(), "hindsight")
            && self.auth_config.memory_uses_local_compute()
        {
            if let Some(backend) = live_backend {
                backend.release();
            }
            if let Some(backend) = &mut self.backend {
                backend.release();
            }
        }
        let authority = memory_authority(&self.session, 0);
        memory.bind(authority.clone());
        let messages = self.session.archived_user_assistant_messages();
        let origin = ToolCall {
            id: String::new(),
            name: "recall".into(),
            arguments: json!({"purpose":"compaction context"}),
            malformed_arguments: false,
        };
        let mut receipts = Receipts::default();
        let scope = self
            .usage_scope
            .worker()
            .for_purpose(llm::usage::Purpose::Memory);
        let result = memory.compaction_context(
            &authority,
            &messages,
            &mut Runner {
                agent: self,
                ui,
                scope,
                receipts: &mut receipts,
                origin: Some(&origin),
                frozen: None,
            },
        );
        match result {
            Ok(Some(text)) if super::memory_authority_matches(&self.session, &authority, 0) => {
                self.memory_controller.compaction_advice = Some((authority, text))
            }
            Ok(_) => {}
            Err(error) => {
                ui.diagnostic(&format!("Memory compaction context unavailable: {error:#}"))
            }
        }
    }
    pub(super) fn prepare_selected_memory(
        &mut self,
        user: &str,
        ui: &mut dyn Ui,
    ) -> Result<Option<crate::storage::memory_backend::PreparedRecall>> {
        let Some(memory) = self.memory_runtime.clone() else {
            return Ok(None);
        };
        let authority = memory_authority(&self.session, 0);
        memory.bind(authority.clone());
        memory.observe_owned(&authority, self.session.archived_user_assistant_messages())?;
        let mut receipts = Receipts::default();
        let scope = self
            .usage_scope
            .worker()
            .for_purpose(llm::usage::Purpose::Memory);
        let origin = ToolCall {
            id: String::new(),
            name: "recall".into(),
            arguments: json!({"query":user}),
            malformed_arguments: false,
        };
        memory.prepare_prompt(
            &authority,
            user,
            &mut Runner {
                agent: self,
                ui,
                scope,
                receipts: &mut receipts,
                origin: Some(&origin),
                frozen: None,
            },
        )
    }
    pub(super) fn memory_maintenance_tick(&mut self, ui: &mut dyn Ui, busy: bool) {
        let Some(memory) = self.memory_runtime.clone() else {
            return;
        };
        if busy || self.memory_controller.capture.is_some() || !ui.maintenance_ready() {
            memory.stop_maintenance();
        }
        let mut controller = std::mem::take(&mut self.memory_controller);
        if busy && let Some(capture) = controller.capture.take() {
            capture.retire();
        }
        if let Some(mut worker) = controller.worker.take() {
            let mut done = false;
            loop {
                let event = match worker.events.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        worker.settle(crate::runtime::Phase::Failed);
                        done = true;
                        break;
                    }
                };
                let mut runner = Runner {
                    agent: self,
                    ui,
                    scope: worker.scope.clone(),
                    receipts: &mut worker.receipts,
                    origin: None,
                    frozen: Some(&worker.frozen),
                };
                match event {
                    Event::Heartbeat(op, tx) => {
                        let _ = tx.send(runner.heartbeat(&op));
                    }
                    Event::Limits(role, model, system, output, op, tx) => {
                        let _ =
                            tx.send(runner.limits(&role, model.as_deref(), &system, output, &op));
                    }
                    Event::Complete(request, tx) => {
                        let _ = tx.send(runner.complete(request));
                    }
                    Event::Authorize(op, purpose, tx) => {
                        let _ = tx.send(runner.authorize_external(&op, &purpose));
                    }
                    Event::Started(op, id, tx) => {
                        let _ = tx.send(runner.external_started(&op, id));
                    }
                    Event::Account(op, id, usage, accepted, tx) => {
                        let _ = tx.send(runner.account_external(&op, id, &usage, accepted));
                    }
                    Event::Abandon(op, id, tx) => {
                        let _ = tx.send(runner.abandon_external(&op, id));
                    }
                    Event::Write(op, targets, tx) => {
                        let _ = tx.send(runner.authorize_managed_write(&op, &targets));
                    }
                    Event::Done(result) => {
                        let phase = if result.is_ok() {
                            crate::runtime::Phase::Completed
                        } else {
                            crate::runtime::Phase::Interrupted
                        };
                        if let Err(error) = result {
                            ui.diagnostic(&format!(
                                "Selected memory maintenance did not finish: {error:#}"
                            ));
                        }
                        worker.settle(phase);
                        done = true;
                        break;
                    }
                }
            }
            if done {
                let _ = worker.thread.join();
            } else {
                controller.worker = Some(worker);
            }
        }
        if controller.worker.is_none() && !busy && controller.capture.is_some() {
            self.memory_controller = controller;
            self.run_pending_memory_capture(ui);
            controller = std::mem::take(&mut self.memory_controller);
        }
        if controller.worker.is_none()
            && controller.capture.is_none()
            && !busy
            && !self.plan_mode
            && !self.interrupt.load(std::sync::atomic::Ordering::SeqCst)
            && self.approval != crate::config::types::Approval::Readonly
            && ui.maintenance_ready()
            && !self.maintenance_has_live_work()
            && controller
                .last_start
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(30))
        {
            let authority = memory_authority(&self.session, 0);
            memory.bind(authority.clone());
            let scope = self
                .usage_scope
                .worker()
                .for_purpose(llm::usage::Purpose::Memory);
            let body = self.session.runtime().start(
                "memory-maintenance",
                "Selected memory maintenance",
                self.session.branch_leaf().map(str::to_owned),
            );
            let (tx, rx) = mpsc::channel();
            let frozen = self.auth_config.clone();
            let owned_memory = memory.clone();
            match std::thread::Builder::new()
                .name("dial-memory".into())
                .spawn(move || {
                    let result = memory.maintain(&authority, &mut Proxy { events: tx.clone() });
                    let _ = tx.send(Event::Done(result));
                }) {
                Ok(thread) => {
                    controller.last_start = Some(Instant::now());
                    controller.worker = Some(Worker {
                        events: rx,
                        scope,
                        receipts: Receipts::default(),
                        thread,
                        memory: owned_memory,
                        frozen,
                        body: Some(body),
                    });
                }
                Err(error) => ui.diagnostic(&format!("Cannot start memory maintenance: {error}")),
            }
        }
        self.memory_controller = controller;
    }

    /// One-shot runs (`dial -p`) have no idle loop, so before exit run one selected-memory
    /// maintenance pass, bounded by `budget`, to retain the finished turn. Nothing is printed on
    /// success; a pass still running at the deadline is cancelled. A pending automatic capture turn
    /// is not started here (it is a full model turn) and is retired with the agent.
    pub fn finish_memory_before_exit(&mut self, ui: &mut dyn Ui, budget: Duration) {
        if self.memory_runtime.is_none() {
            return;
        }
        if let Some(capture) = self.memory_controller.capture.take() {
            capture.retire();
        }
        let deadline = Instant::now() + budget;
        self.memory_controller.last_start = None;
        self.memory_maintenance_tick(ui, false);
        while self.memory_controller.worker.is_some() {
            if Instant::now() >= deadline {
                if let Some(mut worker) = self.memory_controller.worker.take() {
                    worker.memory.cancel();
                    worker.settle(crate::runtime::Phase::Interrupted);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
            // Drains the worker's model and authorization requests; a finished worker is joined.
            let started = self.memory_controller.last_start;
            self.memory_maintenance_tick(ui, false);
            // Never start a second pass: one bounded pass is the whole budget.
            if self.memory_controller.last_start != started {
                if let Some(mut worker) = self.memory_controller.worker.take() {
                    worker.memory.cancel();
                    worker.settle(crate::runtime::Phase::Interrupted);
                }
                return;
            }
        }
    }
}
