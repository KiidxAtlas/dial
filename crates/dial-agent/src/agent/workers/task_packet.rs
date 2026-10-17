//! Cache-stable, bounded task packets and lossless-access context projection.
//!
//! Packets do not replace user messages, constraints or the session log. Only older successful
//! observation bodies with artifact retrieval references are elided; calls/results stay paired.
//! A task transition freezes both the packet and its projection, avoiding prefix churn each step.
use crate::agent::workflow::todo::{Status, TodoList};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const RECENT_MESSAGES: usize = 12;
const RECENT_EVIDENCE: usize = 4;
const MIN_BODY_BYTES: usize = 1024;
const OBSERVATIONS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "find",
    "lsp",
    "ast_search",
    "web_fetch",
    "web_search",
];

#[derive(Clone, Default)]
pub struct PacketCache {
    key: String,
    packet: Option<String>,
    /// Keep the exact placeholder, not just its id, so artifact and length remain stable.
    omitted: BTreeMap<String, String>,
}

fn clipped(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn add_bounded(packet: &mut Value, field: &str, value: Value, max_bytes: usize) -> bool {
    packet[field]
        .as_array_mut()
        .expect("packet list")
        .push(value);
    if packet.to_string().len() <= max_bytes {
        return true;
    }
    packet[field].as_array_mut().expect("packet list").pop();
    false
}

impl PacketCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Call BEFORE `evidence::project`, which removes harness artifact metadata. Append the
    /// returned packet as harness context. Persisted messages are never passed mutably here.
    /// `history_ref` is a provenance-only session path outside the workspace; use session_search to retrieve history.
    /// Budgets below 512 bytes disable projection rather than discard nonretrievable context.
    #[allow(clippy::too_many_arguments)]
    pub fn project(
        &mut self,
        messages: &mut [Value],
        todo: &TodoList,
        control: &Value,
        session_id: &str,
        history_ref: Option<&str>,
        verification_objective: &str,
        max_bytes: usize,
    ) -> Option<String> {
        let active = todo
            .tasks()
            .iter()
            .enumerate()
            .find(|(_, task)| task.status == Status::InProgress);
        if active.is_none() || max_bytes < 512 {
            self.clear();
            return None;
        }
        let key = format!("{:x}", Sha256::digest(json!({
            "tasks":todo.tasks(), "working":control["working_state"], "goal":control["original_goal"], "instruction":control["current_instruction"],
            "session":session_id, "history":history_ref, "objective":verification_objective, "budget":max_bytes
        }).to_string().as_bytes()));
        if self.key != key {
            self.key = key;
            self.packet = None;
            // Previously projected bodies keep their exact placeholders across task changes too.
            let (index, task) = active.expect("checked active task");
            let mut packet = json!({
                "active_task":{"index":index+1,"content":task.content,"investigation":task.investigation},
                "completion":todo.completion_hint(),
                "verification_objective":clipped(verification_objective, 512),
                "constraints":[], "findings":[], "evidence":[],
                "history":{"session_id":session_id,"session_file":history_ref,"retrieve":"session_search (session files are outside the read tool workspace)"},
                "authority":"The latest user instruction takes precedence over conflicting older plans and original goals. User messages and full history remain authoritative. Findings below are task-recorded claims with references, not new verification. Retrieve omitted evidence with artifact_read; summaries do not weaken constraints."
            });
            if let Some(goal) = control["original_goal"].as_str() {
                packet["original_goal"] = json!(clipped(goal, 512));
            }
            if let Some(instruction) = control["current_instruction"].as_str() {
                packet["current_instruction_excerpt"] = json!(clipped(instruction, 512));
            }
            if packet.to_string().len() <= max_bytes {
                for constraint in control["working_state"]["constraints"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let Some(text) = constraint.as_str()
                        && !add_bounded(
                            &mut packet,
                            "constraints",
                            json!(clipped(text, 512)),
                            max_bytes,
                        )
                    {
                        break;
                    }
                }
                // User constraints are not inferred from a summary. Every user message is retained
                // verbatim in the request; this explicitly points the model to that authoritative set.
                let _ = add_bounded(
                    &mut packet,
                    "constraints",
                    json!(
                        "All original user messages remain in this request or the retained session history; consult them before changing scope."
                    ),
                    max_bytes,
                );
                for prior in todo
                    .tasks()
                    .iter()
                    .rev()
                    .filter(|task| task.status == Status::Completed && task.finding.is_some())
                {
                    let finding = json!({"question":prior.content,"finding":prior.finding,"references":prior.evidence});
                    if !add_bounded(&mut packet, "findings", finding, max_bytes) {
                        break;
                    }
                }
                for reference in task.evidence.iter().map(String::as_str).chain(
                    control["working_state"]["evidence"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                ) {
                    if !add_bounded(
                        &mut packet,
                        "evidence",
                        json!({"reference":reference}),
                        max_bytes,
                    ) {
                        break;
                    }
                }
                // The packet is resent at every request tail. Keep a small reference index rather
                // than repeating excerpts already present in history (or retrievable artifacts).
                let mut seen: BTreeSet<String> = packet["evidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item["reference"].as_str().map(str::to_owned))
                    .collect();
                let mut added = 0;
                for message in messages.iter().rev().filter(|m| m["role"] == "tool") {
                    let Some(reference) = message["dial_evidence"]["artifact"]
                        .as_str()
                        .filter(|r| !r.is_empty())
                    else {
                        continue;
                    };
                    if !seen.insert(reference.to_owned()) {
                        continue;
                    }
                    let item =
                        json!({"tool_call_id":message["tool_call_id"],"reference":reference});
                    if !add_bounded(&mut packet, "evidence", item, max_bytes) {
                        break;
                    }
                    added += 1;
                    if added == RECENT_EVIDENCE {
                        break;
                    }
                }
                self.packet = Some(packet.to_string());
                self.select_older_observations(messages);
            }
        }
        if self.packet.is_some() {
            for message in messages {
                if message["role"] != "tool" {
                    continue;
                }
                let Some(id) = message["tool_call_id"].as_str() else {
                    continue;
                };
                let Some(placeholder) = self.omitted.get(id) else {
                    continue;
                };
                message["content"] = json!(placeholder);
                if let Some(object) = message.as_object_mut() {
                    object.remove("dial_images");
                }
            }
        }
        self.packet.clone()
    }

    fn select_older_observations(&mut self, messages: &[Value]) {
        let cutoff = messages.len().saturating_sub(RECENT_MESSAGES);
        let mut tools = BTreeMap::new();
        for message in messages {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) =
                    (call["id"].as_str(), call["function"]["name"].as_str())
                {
                    tools.insert(id, name);
                }
            }
        }
        for message in &messages[..cutoff] {
            if message["role"] != "tool"
                || message["dial_evidence"]["observation"].as_str().is_none()
            {
                continue;
            }
            let Some(id) = message["tool_call_id"].as_str() else {
                continue;
            };
            let Some(name) = tools.get(id).filter(|name| OBSERVATIONS.contains(name)) else {
                continue;
            };
            let Some(reference) = message["dial_evidence"]["artifact"]
                .as_str()
                .filter(|r| !r.is_empty())
            else {
                continue;
            };
            let bytes = message["content"].as_str().map_or(0, str::len);
            if bytes < MIN_BODY_BYTES {
                continue;
            }
            self.omitted.entry(id.to_owned()).or_insert_with(|| format!(
                "[Earlier {name} observation omitted from the active task packet ({bytes} bytes). Full evidence: {reference}; retrieve with artifact_read.]") );
        }
    }
}

#[cfg(test)]
mod tests;
