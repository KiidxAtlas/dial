//! Automatic capture is a real, private native-tool turn owned by the stopped session.
use super::{
    Agent, TurnOutcome, TurnReport, Ui, compaction, compaction_runtime, make_backend,
    memory_authority,
};
use crate::{
    config::types::{Approval, Config},
    llm::{self, Chat, usage::Scope},
    storage::memory_backend::MemoryAuthority,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct Capture {
    authority: MemoryAuthority,
    config: Config,
    scope: Scope,
    messages: Vec<Value>,
    body: crate::runtime::Body,
    pub(super) cancel: Arc<AtomicBool>,
}
impl Capture {
    fn current(&self, agent: &Agent<'_>) -> bool {
        !self.cancel.load(Ordering::SeqCst)
            && !agent.interrupt.load(Ordering::SeqCst)
            && !agent.plan_mode
            && agent.approval != Approval::Readonly
            && super::memory_authority_matches(&agent.session, &self.authority, 0)
            && agent.auth_config.same_model_identity(&self.config)
            && agent.auth_config.model_roles == self.config.model_roles
            && agent.auth_config.provider_cache_namespace == self.config.provider_cache_namespace
    }
    pub(super) fn needs_compute(&self) -> bool {
        self.config.uses_local_compute()
    }
    pub(super) fn retire(self) {
        self.cancel.store(true, Ordering::SeqCst);
        let (total, turn) = self.scope.snapshot();
        self.body.settle(
            "Automatic memory capture superseded".into(),
            Some(json!({
                "authority":self.authority,"usage_total":total,"usage_turn":turn
            })),
            crate::runtime::Phase::Interrupted,
        );
    }
}
impl Agent<'_> {
    pub(super) fn retire_memory_capture(&mut self) {
        if let Some(capture) = self.memory_controller.capture.take() {
            capture.retire();
        }
        if let Some(cancel) = &self.memory_controller.active_capture {
            cancel.store(true, Ordering::SeqCst);
        }
        self.memory_controller.compaction_advice = None;
    }
    pub(super) fn schedule_memory_capture(
        &mut self,
        report: &TurnReport,
        predecessor: Option<&str>,
        ui: &mut dyn Ui,
    ) {
        if !self.config.memory.autolearn
            || !self.config.memory.autolearn_auto_continue
            || self.memory_runtime.is_none()
            || self.hermetic
            || self.plan_mode
            || self.approval == Approval::Readonly
            || report.interrupted
            || report.outcome != TurnOutcome::Completed
            || report.text.trim().is_empty()
            || self.exploration.busy()
            || self.interrupt.load(Ordering::SeqCst)
            || self.session.paired_tool_calls_since(predecessor)
                < self.config.memory.autolearn_min_tool_calls
            || !self.catalog().iter().any(|name| name == "learn")
        {
            return;
        }
        let mut messages = self.system_messages();
        messages.push(json!({"role":"system","content":crate::storage::memory_backend::autolearn_capture_prompt()}));
        messages.extend(self.session.messages());
        let body = self.session.runtime().start(
            "memory-capture",
            "Automatic reusable-memory capture",
            self.session.branch_leaf().map(str::to_owned),
        );
        body.progress("queued after genuine stopped turn", 0);
        let capture = Capture {
            authority: memory_authority(&self.session, 0),
            config: self.auth_config.clone(),
            scope: self
                .usage_scope
                .worker()
                .for_purpose(llm::usage::Purpose::Memory),
            messages,
            body,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if let Some(old) = self.memory_controller.capture.replace(capture) {
            old.retire();
        }
        if let Some(memory) = &self.memory_runtime {
            memory.stop_maintenance();
        }
        self.run_pending_memory_capture(ui);
    }
    /// Only the stopped turn calls this after its own control/journal bookkeeping, before yielding.
    pub(super) fn seal_pending_memory_capture(&mut self, completed: bool) {
        let Some(mut capture) = self.memory_controller.capture.take() else {
            return;
        };
        let authority = memory_authority(&self.session, 0);
        if !completed
            || capture.cancel.load(Ordering::SeqCst)
            || capture.authority.session_id != authority.session_id
            || capture.authority.branch_id != authority.branch_id
            || !self.auth_config.same_model_identity(&capture.config)
        {
            capture.retire();
            return;
        }
        capture.authority = authority;
        self.memory_controller.capture = Some(capture);
    }
    pub(super) fn run_pending_memory_capture(&mut self, ui: &mut dyn Ui) {
        if self.memory_controller.has_worker()
            || self.maintenance_has_live_work()
            || !ui.maintenance_ready()
        {
            return;
        }
        let Some(mut capture) = self.memory_controller.capture.take() else {
            return;
        };
        if !capture.current(self) {
            capture.retire();
            return;
        }
        self.memory_controller.active_capture = Some(capture.cancel.clone());
        let original_scope = std::mem::replace(&mut self.usage_scope, capture.scope.clone());
        let original_preview = std::mem::replace(&mut self.edit_preview_intercepted, false);
        let base = capture.messages.len();
        let mut accepted = 0usize;
        let result = (|| -> Result<()> {
            let schema = crate::storage::memory_backend::schema(&capture.config.memory, "learn")
                .context("selected backend does not offer native learn")?;
            let mut config = capture.config.clone();
            config.provider_cache_namespace = Some(compaction_runtime::side_request_namespace(
                self.session.id(),
            )?);
            let key = "memory-native-capture".to_owned();
            if !self.role_backends.contains_key(&key) {
                self.role_backends
                    .insert(key.clone(), make_backend(&config)?);
            }
            if let Some(main) = &mut self.backend {
                main.release();
            }
            let mut backend = self
                .role_backends
                .remove(&key)
                .context("capture backend missing")?;
            backend.set_usage_scope(Some(capture.scope.clone()));
            let execution = {
                let output = backend.output_budget(self.version.harness.max_tokens).min(
                    backend
                        .model_profile()
                        .and_then(|profile| profile.max_tokens)
                        .filter(|limit| *limit > 0)
                        .unwrap_or(u32::MAX),
                );
                let window = self.budget_window().min(config.context_window).min(
                    backend
                        .model_profile()
                        .and_then(|profile| profile.context_window)
                        .unwrap_or(u32::MAX),
                );
                let mut request = Chat {
                    messages: std::mem::take(&mut capture.messages),
                    tools: vec![schema],
                    temperature: 0.0,
                    max_tokens: output,
                    seed: None,
                };
                let adapter = self.version.adapter.clone();
                let turns = (|| -> Result<()> {
                    // One learn batch and its completion; an unlimited public harness is not a zero-call capture.
                    for _ in 0..self.version.harness.capped_steps(2) {
                        ensure!(
                            capture.current(self) && ui.maintenance_ready(),
                            "memory capture yielded to foreground work"
                        );
                        ensure!(
                            crate::llm::input_bytes(&request)? <= config.memory.request_max_bytes,
                            "native capture request exceeds configured byte capacity"
                        );
                        let selection = config.selection();
                        let tokens = compaction::request_tokens_with_policy(
                            &request,
                            &config.compaction,
                            crate::config::compaction::CompactionIdentity {
                                provider: &selection.provider,
                                model: &selection.model,
                                agent: Some("memory-capture"),
                            },
                        );
                        let remaining = u64::from(window).saturating_sub(tokens);
                        ensure!(
                            remaining > 0,
                            "complete native capture input exceeds actual model window"
                        );
                        request.max_tokens =
                            output.min(u32::try_from(remaining).unwrap_or(u32::MAX));
                        capture
                            .body
                            .progress("private native capture", accepted as u32);
                        let rewritten = self.provider_request_with(&request, ui)?;
                        let started = std::time::Instant::now();
                        let reply = super::chat_with_ui(
                            &mut backend,
                            adapter.as_deref(),
                            rewritten.as_ref().unwrap_or(&request),
                            super::ChatOptions {
                                timeout: config.worker_timeout_secs,
                                render_text: false,
                                context_tokens: None,
                                policy: None,
                                recovery: Some(&config.request_recovery),
                            },
                            capture.cancel.clone(),
                            ui,
                            &mut |ui| {
                                self.pump_subagents(ui);
                                if !capture.current(self) || !ui.maintenance_ready() {
                                    capture.cancel.store(true, Ordering::SeqCst);
                                }
                            },
                        );
                        self.record_aux_request(&backend, &reply, started);
                        let reply = reply?;
                        let valid = (|| -> Result<()> {
                            ensure!(
                                capture.current(self) && ui.maintenance_ready(),
                                "native capture reply lost its owner"
                            );
                            let mut ids =
                                std::collections::HashSet::with_capacity(reply.tool_calls.len());
                            ensure!(
                                reply.finish_reason != "length",
                                "native capture reply was truncated"
                            );
                            for call in &reply.tool_calls {
                                ensure!(
                                    call.name == "learn" && !call.malformed_arguments,
                                    "native capture requested an unoffered or malformed tool"
                                );
                                super::calls::validate(
                                    &request.tools[0]["function"]["parameters"],
                                    &call.arguments,
                                )
                                .map_err(anyhow::Error::msg)?;
                                ensure!(
                                    !call.id.is_empty() && ids.insert(call.id.as_str()),
                                    "native capture returned missing or duplicate tool-call identities"
                                );
                            }
                            Ok(())
                        })();
                        if let Err(error) = valid {
                            backend.record_discarded_response()?;
                            return Err(error);
                        }
                        request.messages.push(crate::llm::assistant_message(&reply));
                        if reply.tool_calls.is_empty() {
                            return Ok(());
                        }
                        for call in &reply.tool_calls {
                            ensure!(
                                capture.current(self) && ui.maintenance_ready(),
                                "native capture tool lost its owner"
                            );
                            let result = self.dispatch(&mut backend, call, ui);
                            if !result.is_error {
                                accepted += 1;
                            }
                            request.messages.push(json!({"role":"tool","tool_call_id":call.id,"content":result.text,"dial_capture_result":true,"is_error":result.is_error}));
                        }
                    }
                    anyhow::bail!("native memory capture reached the configured request limit")
                })();
                capture.messages = request.messages;
                turns
            };
            backend.release();
            backend.set_usage_scope(None);
            self.role_backends.insert(key, backend);
            execution
        })();
        self.usage_scope = original_scope;
        self.edit_preview_intercepted = original_preview;
        self.memory_controller.active_capture = None;
        let (total, turn) = capture.scope.snapshot();
        let phase = if result.is_ok() {
            crate::runtime::Phase::Completed
        } else if !capture.current(self) {
            crate::runtime::Phase::Interrupted
        } else {
            crate::runtime::Phase::Failed
        };
        let private_messages = if capture.messages.len() >= base {
            capture.messages.split_off(base)
        } else {
            Vec::new()
        };
        let error = result.err().map(|error| format!("{error:#}"));
        if let Some(error) = &error {
            ui.diagnostic(&format!("Automatic memory capture did not finish: {error}"));
        }
        capture.body.settle("Private automatic memory capture settled".into(),Some(json!({"authority":capture.authority,
            "accepted_learn_calls":accepted,"messages":private_messages,"error":error,"usage_total":total,"usage_turn":turn})),phase);
    }
}
