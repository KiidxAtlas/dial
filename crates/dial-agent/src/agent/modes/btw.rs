//! `/btw`: side threads answer one question at a time from a snapshot of the session.
//! No tools are offered and nothing is appended to the transcript. A snapshot owns everything a
//! request needs, so it can outlive the agent's turn and a main turn can proceed while it streams.

use crate::agent::{Agent, make_backend};
use crate::config::types::Config;
use crate::llm::Chat;
use anyhow::{Result, ensure};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicBool};

/// The session as it stood when the snapshot was taken: its id, the branch leaf that side
/// threads anchor to, the projected history, and the config to answer with.
#[derive(Clone)]
pub struct SideSnapshot {
    session_id: String,
    anchor: Option<String>,
    pending_prompt: Option<String>,
    chat: Chat,
    config: Config,
}

/// A side request frozen at the moment it was built. It owns everything it needs to run.
pub struct SideRequest {
    chat: Chat,
    config: Config,
    usage_scope: Option<crate::llm::usage::Scope>,
}

impl Agent<'_> {
    /// Freezes the side context of the current session. Tools are cleared: side answers never act.
    /// `pending_prompt` is the main prompt about to run. It is not in the session yet, so it is
    /// appended as the last user message unless the projection already ends with it.
    pub fn side_snapshot(&self, pending_prompt: Option<&str>) -> Result<SideSnapshot> {
        let history = self.session.messages();
        let mut chat = self.projected_request(&history, None)?;
        chat.tools.clear();
        let mut captured_prompt = None;
        if let Some(prompt) = pending_prompt.filter(|prompt| !prompt.trim().is_empty()) {
            let already_last = chat.messages.last().is_some_and(|message| {
                message["role"] == "user" && message["content"].as_str() == Some(prompt)
            });
            if !already_last {
                chat.messages
                    .push(json!({"role": "user", "content": prompt}));
                captured_prompt = Some(prompt.to_owned());
            }
        }
        Ok(SideSnapshot {
            session_id: self.session.id().to_string(),
            anchor: self.session.branch_leaf().map(str::to_string),
            pending_prompt: captured_prompt,
            chat,
            config: self.effective_config().clone(),
        })
    }
}

impl SideSnapshot {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The branch leaf at snapshot time, where a side thread created from it is anchored.
    pub fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }

    pub fn pending_prompt(&self) -> Option<&str> {
        self.pending_prompt.as_deref()
    }

    /// Builds a side request from this snapshot. `prior` holds earlier `(question, answer)`
    /// exchanges of the same side thread, replayed before the new question.
    pub fn request(&self, prior: &[(String, String)], question: &str) -> Result<SideRequest> {
        let question = question.trim();
        ensure!(!question.is_empty(), "usage: /btw <question>");
        let mut chat = self.chat.clone();
        // The snapshot may be captured while the main agent is dispatching a tool batch.
        // Close only the copied transcript's pending calls so the side provider sees a valid
        // conversation, without inventing outcomes or modifying the live session.
        let mut pending = Vec::new();
        for message in &chat.messages {
            match message["role"].as_str() {
                Some("assistant") => {
                    pending.clear();
                    if let Some(calls) = message["tool_calls"].as_array() {
                        pending.extend(calls.iter().filter_map(|call| call["id"].as_str()));
                    }
                }
                Some("tool") => pending.retain(|id| Some(*id) != message["tool_call_id"].as_str()),
                _ => {}
            }
        }
        let pending: Vec<String> = pending.into_iter().map(str::to_owned).collect();
        for id in pending {
            chat.messages.push(crate::llm::tool_message(
                &id,
                "Tool result pending at snapshot time; no outcome is known. Do not treat this as execution or verification evidence.",
            ));
        }
        for (asked, answered) in prior {
            chat.messages
                .push(json!({"role": "user", "content": asked}));
            chat.messages
                .push(json!({"role": "assistant", "content": answered}));
        }
        chat.messages.push(json!({
            "role": "user",
            "content": format!(
                "Side question (answer only this; do not call tools or continue the task): {question}"
            ),
        }));
        Ok(SideRequest {
            chat,
            config: self.config.clone(),
            usage_scope: None,
        })
    }
}

impl SideRequest {
    /// Charge an agent-initiated side question to the parent's shared worker ledger.
    pub(in crate::agent) fn with_usage_scope(mut self, scope: crate::llm::usage::Scope) -> Self {
        self.usage_scope = Some(scope);
        self
    }

    /// Streams the answer through `on_text` until it completes or `cancel` is set.
    pub fn run(self, cancel: Arc<AtomicBool>, on_text: &mut dyn FnMut(&str)) -> Result<String> {
        let mut backend = make_backend(&self.config)?;
        backend.set_usage_scope(self.usage_scope);
        let timeout = self.config.worker_timeout_secs;
        let mut keep_going = |text: &str| {
            on_text(text);
            true
        };
        let keep: &mut dyn FnMut(&str) -> bool = &mut keep_going;
        let reply = backend.chat_with_control(
            None,
            &self.chat,
            timeout,
            Some(keep),
            &mut |_| {},
            cancel,
        )?;
        Ok(reply.text)
    }
}

#[cfg(test)]
mod tests;
