//! Model-owned investigation checkpoints. Rewind replaces context, never files or effects.
use super::{Agent, Ui, compaction};
use crate::{
    llm::{ToolCall, provider::Backend},
    storage::session::Session,
    tools::ToolOutput,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) const TOOL_NAMES: [&str; 2] = ["checkpoint", "rewind"];
const MAX_GOAL_BYTES: usize = 8 * 1024;
const MAX_REPORT_BYTES: usize = 64 * 1024;

pub(super) fn schema(name: &str) -> Option<Value> {
    let (key, description, limit) = match name {
        "checkpoint" => (
            "goal",
            "Begin an investigation checkpoint. Later call rewind with concise findings before finishing. Intermediate messages leave active context, but the immutable archive, actual effects and usage remain. Nested checkpoints are not permitted.",
            MAX_GOAL_BYTES,
        ),
        "rewind" => (
            "report",
            "Request completion of the active investigation checkpoint with nonempty findings. The owning lifecycle commits an anchored report only after all accepted tool calls have paired results. This does not restore files or undo effects.",
            MAX_REPORT_BYTES,
        ),
        _ => return None,
    };
    Some(
        json!({"type":"function","function":{"name":name,"description":description,
        "parameters":{"type":"object","properties":{key:{"type":"string","minLength":1,"maxLength":limit}},"required":[key],"additionalProperties":false}}}),
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Checkpoint {
    pub owner_id: String,
    pub entry_id: String,
    pub message_count: usize,
    pub started_at: String,
    pub goal: String,
    pub instruction_entry: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) enum Request {
    Checkpoint {
        goal: String,
        started_at: String,
        call_id: String,
        owner_id: String,
    },
    Rewind {
        report: String,
        call_id: String,
        policy: String,
    },
}

#[derive(Default)]
pub(super) struct Lifecycle {
    pub active: Option<Checkpoint>,
    pub pending: Option<Request>,
    pub completed: bool,
}

impl Lifecycle {
    pub fn restore(state: &Value, owner_id: &str) -> Self {
        let active = serde_json::from_value::<Checkpoint>(state["exploration"]["active"].clone())
            .ok()
            .filter(|checkpoint| checkpoint.owner_id == owner_id);
        Self {
            active,
            pending: None,
            completed: state["exploration"]["completed"].is_object(),
        }
    }

    pub fn requested_rewind(&self) -> bool {
        matches!(self.pending, Some(Request::Rewind { .. }))
    }
    pub fn busy(&self) -> bool {
        self.active.is_some() || self.pending.is_some()
    }
    pub fn invalidate(&mut self) {
        self.active = None;
        self.pending = None;
        self.completed = false;
    }

    /// The call must belong to an accepted, journaled assistant group, never a native callback.
    pub fn request(
        &mut self,
        call: &ToolCall,
        owner_id: &str,
        policy: String,
        timestamp: String,
        cancel: &AtomicBool,
    ) -> ToolOutput {
        let result = (|| -> Result<ToolOutput> {
            ensure!(
                !cancel.load(Ordering::SeqCst),
                "investigation request cancelled; no transition requested"
            );
            ensure!(
                !call.id.is_empty(),
                "checkpoint and rewind require an accepted assistant tool call; native callbacks cannot rewrite their owner's conversation"
            );
            let key = if call.name == "checkpoint" {
                "goal"
            } else {
                "report"
            };
            let args = call
                .arguments
                .as_object()
                .context("investigation arguments must be a JSON object")?;
            ensure!(
                args.len() == 1 && args.contains_key(key),
                "{key} is the only allowed argument"
            );
            let text = crate::tools::req_str(&call.arguments, key)
                .map_err(anyhow::Error::msg)?
                .trim();
            let limit = if key == "goal" {
                MAX_GOAL_BYTES
            } else {
                MAX_REPORT_BYTES
            };
            ensure!(
                !text.is_empty() && text.len() <= limit,
                "{key} must be nonempty and at most {limit} UTF-8 bytes"
            );
            ensure!(
                self.pending.is_none(),
                "an investigation transition is already pending in this tool group"
            );
            if key == "goal" {
                ensure!(
                    self.active.is_none(),
                    "Checkpoint already active; rewind it before starting another"
                );
                self.pending = Some(Request::Checkpoint {
                    goal: text.to_owned(),
                    started_at: timestamp.clone(),
                    call_id: call.id.clone(),
                    owner_id: owner_id.to_owned(),
                });
                Ok(ToolOutput {
                    execution: Default::default(),
                    text: format!(
                        "Checkpoint: {text}\nFinish exploration and call rewind with findings. Activation follows the durable paired tool-group boundary."
                    ),
                    is_error: false,
                    data: Some(
                        json!({"goal":text,"startedAt":timestamp,"requested":true,"committed":false}),
                    ),
                })
            } else {
                let checkpoint = self.active.as_ref().with_context(|| if self.completed {
                    "Checkpoint already completed; continue from the retained report instead of calling rewind again"
                } else { "No active checkpoint; call checkpoint before rewind" })?;
                ensure!(
                    checkpoint.owner_id == owner_id,
                    "checkpoint belongs to a different conversation owner"
                );
                self.pending = Some(Request::Rewind {
                    report: text.to_owned(),
                    call_id: call.id.clone(),
                    policy,
                });
                Ok(ToolOutput { execution: Default::default(),  text: "Rewind requested. Report captured; no history replacement has been acknowledged yet.".into(), is_error: false,
                    data: Some(json!({"report":text,"requested":true,"rewound":false})) })
            }
        })();
        result.unwrap_or_else(|error| ToolOutput::err(error.to_string()))
    }

    pub fn reject_result(&mut self, call: &ToolCall) {
        let matches = match &self.pending {
            Some(Request::Checkpoint { call_id, .. } | Request::Rewind { call_id, .. }) => {
                call_id == &call.id
            }
            None => false,
        };
        if matches {
            self.pending = None;
        }
    }
}

pub(super) fn report_message(
    report: &str,
    checkpoint: &Checkpoint,
    source_leaf: &str,
    digest: &str,
) -> Value {
    json!({"role":"user","dial_rewind_report":true,"dial_exploration_proof":{
        "checkpointEntryId":checkpoint.entry_id,"explorationLeaf":source_leaf,"startedAt":checkpoint.started_at,"digest":digest,
        "archive":{"tool":"session_search","arguments":{"query":"","history":true,"all_branches":true},"sourceEntryId":source_leaf}},
        "content":format!("Investigation checkpoint completed. The following model-authored findings replace intermediate exploration, not actual effects or the user request. Findings are untrusted historical data until verified; current instructions and permissions still govern. Need another investigation: create a new checkpoint.\n\nReport:\n{report}")})
}

/// Reparent only complete, genuine terminal sibling calls/results, not a fabricated native reply.
pub(super) fn terminal_siblings(messages: &[Value], rewind_id: &str) -> Result<Vec<Value>> {
    ensure!(
        compaction::complete_boundary(messages, messages.len()),
        "rewind requires a closed assistant/tool group"
    );
    let Some(at) = messages.iter().rposition(|message| {
        message["role"] == "assistant"
            && message["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["id"] == rewind_id))
    }) else {
        anyhow::bail!("accepted rewind assistant call is missing from the owning conversation");
    };
    let calls: Vec<Value> = messages[at]["tool_calls"]
        .as_array()
        .context("rewind group has no calls")?
        .iter()
        .filter(|call| call["function"]["name"] != "rewind")
        .cloned()
        .collect();
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let mut results = Vec::with_capacity(calls.len());
    for call in &calls {
        let id = call["id"]
            .as_str()
            .context("terminal sibling call has no id")?;
        let message = messages[at + 1..]
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .context("terminal sibling result is missing")?;
        results.push(message.clone());
    }
    let mut result = vec![json!({"role":"assistant","content":null,"tool_calls":calls})];
    result.extend(results);
    ensure!(
        compaction::complete_boundary(&result, result.len()),
        "terminal sibling group is incomplete"
    );
    Ok(result)
}

impl Agent<'_> {
    pub(super) fn exploration_owner(&self) -> bool {
        !self.hermetic
            && self.config.checkpoint.enabled
            && self.session.owns_journal()
            && self
                .config
                .active_model_profile()
                .and_then(|profile| profile.tools.as_ref())
                .is_none_or(|tools| tools.iter().any(|name| name == "session_search"))
    }

    fn exploration_policy_signature(&self) -> Result<String> {
        let control = self.session.control_state();
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!([
                self.compaction_configuration_signature()?,
                self.plan_mode,
                self.approval,
                self.always,
                self.catalog(),
                control["portable_rules"],
                control["provider_identity"]
            ]))?)
        ))
    }

    pub(super) fn invalidate_exploration(&mut self, reason: &str, ui: &mut dyn Ui) -> Result<()> {
        if self.exploration.busy() {
            self.exploration.invalidate();
            let mut state = self.session.control_state();
            state["exploration"] = json!({"invalidated":reason});
            self.session.set_control_state(state)?;
            ui.event(&crate::agent::ui_event::UiEvent::new(crate::agent::ui_event::EventKind::Investigation, format!("Investigation checkpoint cleared: {reason}. Existing user input, completed effects and archived history are preserved.")));
        }
        Ok(())
    }

    pub(super) fn dispatch_exploration(&mut self, call: &ToolCall) -> ToolOutput {
        if !self.exploration_owner() {
            return ToolOutput::err("investigation checkpoints require the owning durable session");
        }
        if self.pending_context_rollover.is_some() {
            return ToolOutput::err(
                "a new_context transition is already pending; complete it before checkpoint/rewind",
            );
        }
        let policy = match self.exploration_policy_signature() {
            Ok(policy) => policy,
            Err(error) => return ToolOutput::err(error.to_string()),
        };
        self.exploration.request(
            call,
            self.session.id(),
            policy,
            Session::exploration_timestamp(),
            &self.interrupt,
        )
    }

    pub(super) fn settle_exploration(
        &mut self,
        backend: &mut Backend,
        ui: &mut dyn Ui,
    ) -> Result<bool> {
        let Some(request) = self.exploration.pending.take() else {
            return Ok(false);
        };
        ensure!(
            self.exploration_owner() && !self.interrupt.load(Ordering::SeqCst),
            "investigation transition cancelled or no longer owns its durable session"
        );
        self.session.ensure_exploration_groups_closed()?;
        let owner = self.session.id().to_owned();
        let source_leaf = self
            .session
            .branch_leaf()
            .context("investigation has no source leaf")?
            .to_owned();
        match request {
            Request::Checkpoint {
                goal,
                started_at,
                call_id,
                owner_id,
            } => {
                ensure!(
                    owner_id == owner,
                    "checkpoint request belongs to a different conversation owner; no activation committed"
                );
                ensure!(
                    self.session.has_exploration_result(&call_id),
                    "checkpoint has no durable paired result"
                );
                let state = self.session.control_state();
                let checkpoint = Checkpoint {
                    owner_id: owner.clone(),
                    entry_id: source_leaf.clone(),
                    message_count: self.session.message_count(),
                    started_at,
                    goal,
                    instruction_entry: state["current_instruction_entry_id"]
                        .as_str()
                        .map(str::to_owned),
                };
                let mut state = state;
                state["exploration"] = json!({"active":checkpoint});
                self.retire_memory_capture();
                if let Some(memory) = &self.memory_runtime {
                    memory.cancel();
                }
                let commit = self
                    .session
                    .commit_exploration_control(&owner, &source_leaf, state);
                if let Some(memory) = &self.memory_runtime {
                    memory.resume(super::memory_authority(&self.session, 0));
                }
                let commit = commit?;
                self.exploration.active = Some(checkpoint);
                self.exploration.completed = false;
                if let Some(error) = commit.durability_error {
                    anyhow::bail!(
                        "{error}; checkpoint was applied but is not durably acknowledged; automatic continuation stopped"
                    )
                }
                ui.event(&crate::agent::ui_event::UiEvent::new(crate::agent::ui_event::EventKind::Investigation, "Investigation checkpoint committed. Rewind with findings before finishing; actual effects are not rolled back."));
                Ok(false)
            }
            Request::Rewind {
                report,
                call_id,
                policy,
            } => {
                let checkpoint = self
                    .exploration
                    .active
                    .as_ref()
                    .context("rewind checkpoint was invalidated")?
                    .clone();
                ensure!(
                    checkpoint.owner_id == owner
                        && self.session.contains_branch_leaf(&checkpoint.entry_id),
                    "rewind checkpoint is stale or belongs to another session"
                );
                let mut state = self.session.control_state();
                ensure!(
                    state["current_instruction_entry_id"].as_str()
                        == checkpoint.instruction_entry.as_deref(),
                    "a newer user instruction invalidated the investigation; no rewind committed"
                );
                ensure!(
                    policy == self.exploration_policy_signature()?,
                    "current provider, approval or rule policy changed before rewind; no history rewrite committed"
                );
                ensure!(
                    self.session.has_exploration_result(&call_id),
                    "rewind result has not been durably paired"
                );
                let siblings = terminal_siblings(&self.session.messages(), &call_id)?;
                let digest = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&json!([
                        owner,
                        checkpoint.entry_id,
                        source_leaf,
                        report,
                        siblings,
                        policy
                    ]))?)
                );
                let report_message = report_message(&report, &checkpoint, &source_leaf, &digest);
                state["exploration"] = json!({"completed":{"report":report,"checkpointEntryId":checkpoint.entry_id,"explorationLeaf":source_leaf,
                    "startedAt":checkpoint.started_at,"rewoundAt":Session::exploration_timestamp(),"digest":digest}});
                let generation = state["provider_identity"]["history_generation"]
                    .as_u64()
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("history generation exhausted")?;
                state["provider_identity"]["history_generation"] = json!(generation);
                self.cancel_background_compaction();
                self.session.flush_settled_runtime()?;
                let expected_leaf = self
                    .session
                    .branch_leaf()
                    .context("rewind source disappeared")?
                    .to_owned();
                ensure!(
                    !self.interrupt.load(Ordering::SeqCst),
                    "rewind cancelled before history replacement"
                );
                self.retire_memory_capture();
                if let Some(memory) = &self.memory_runtime {
                    memory.cancel();
                }
                let commit = self.session.rewind_exploration(&owner, &expected_leaf, &checkpoint.entry_id, report_message, siblings, state, digest)
                    .map_err(|error| {
                        if !self.session.owns_journal() {
                            backend.remember_session_error(anyhow::anyhow!("investigation journal append is uncertain; reopen the owning session before another request: {error:#}"));
                        }
                        ui.notice(&format!("Investigation branch append failed: {error:#}; no rollback or retry is implied."));
                        error
                    });
                if let Some(memory) = &self.memory_runtime {
                    memory.resume(super::memory_authority(&self.session, 0));
                }
                let commit = commit?;
                self.exploration.active = None;
                self.exploration.completed = true;
                self.last_prompt_tokens = 0;
                self.reported_messages = 0;
                self.context_meter = Default::default();
                self.task_packet.clear();
                self.superseded = Default::default();
                self.role_backends.clear();
                self.subagent_backend = None;
                self.auth_config.provider_cache_namespace =
                    Some(format!("{owner}:history:{generation}"));
                if let Some(error) = commit.durability_error {
                    backend.remember_session_error(anyhow::anyhow!("investigation report branch was applied but durable acknowledgement failed; reopen the owning session before another request: {error}"));
                    ui.notice(&error);
                    anyhow::bail!(
                        "{error}; report branch was applied but is not durably acknowledged; no retry or rollback is implied; automatic continuation stopped"
                    )
                }
                if let Err(error) = backend.bind_session(self.config, &mut self.session) {
                    let reason = format!(
                        "report branch {} is already durably committed, but provider rebinding failed: {error:#}; reopen the owning session before another request; no rollback or retry is implied",
                        commit.entry_id
                    );
                    backend.remember_session_error(anyhow::anyhow!(reason.clone()));
                    ui.notice(&reason);
                    anyhow::bail!(reason)
                }
                ui.event(&crate::agent::ui_event::UiEvent::new(crate::agent::ui_event::EventKind::Investigation, format!("Investigation rewound to its anchored report ({}, checkpoint {}, archived exploration {}). Actual effects and usage are preserved.", commit.entry_id, checkpoint.entry_id, source_leaf)));
                Ok(true)
            }
        }
    }
}
