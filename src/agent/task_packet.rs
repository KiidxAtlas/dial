//! Cache-stable, bounded task packets and lossless-access context projection.
//!
//! Packets do not replace user messages, constraints or the session log. Only older successful
//! observation bodies with artifact retrieval references are elided; calls/results stay paired.
//! A task transition freezes both the packet and its projection, avoiding prefix churn each step.
use super::todo::{Status, TodoList};
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

#[derive(Default)]
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
            "tasks":todo.tasks(), "working":control["working_state"], "goal":control["original_goal"],
            "session":session_id, "history":history_ref, "objective":verification_objective, "budget":max_bytes
        }).to_string().as_bytes()));
        if self.key != key {
            self.key = key;
            self.packet = None;
            // Previously projected bodies keep their exact placeholders across task changes too.
            let (index, task) = active.expect("checked active task");
            let mut packet = json!({
                "active_task":{"index":index,"content":task.content,"investigation":task.investigation},
                "verification_objective":clipped(verification_objective, 512),
                "constraints":[], "findings":[], "evidence":[],
                "history":{"session_id":session_id,"session_file":history_ref,"retrieve":"session_search (session files are outside the read tool workspace)"},
                "authority":"User messages and full history remain authoritative. Findings below are task-recorded claims with references, not new verification. Retrieve omitted evidence with artifact_read; summaries do not weaken constraints."
            });
            if let Some(goal) = control["original_goal"].as_str() {
                packet["original_goal"] = json!(clipped(goal, 512));
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
mod tests {
    use super::*;

    fn tasks() -> TodoList {
        let mut todo = TodoList::default();
        todo.restore(&json!([{"content":"repair duplicate handling","status":"in_progress"}]))
            .unwrap();
        todo
    }
    fn history() -> Vec<Value> {
        let mut messages = vec![
            json!({"role":"user","content":"Never modify public APIs. Preserve Unicode."}),
            json!({"role":"assistant","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"old","content":"source".repeat(600),"dial_evidence":{"artifact":"artifact://old","observation":"src/lib.rs"}}),
        ];
        for i in 0..12 {
            messages.push(json!({"role":"assistant","content":format!("recent {i}")}));
        }
        messages
    }

    #[test]
    fn focuses_old_bodies_without_losing_constraints_pairing_or_history() {
        let original = history();
        let mut messages = original.clone();
        let mut cache = PacketCache::default();
        let packet = cache
            .project(
                &mut messages,
                &tasks(),
                &json!({"original_goal":"fix the regression"}),
                "session",
                Some("/state/session.jsonl"),
                "duplicate-key test",
                2048,
            )
            .unwrap();
        assert!(packet.len() <= 2048);
        assert!(packet.contains("/state/session.jsonl"));
        assert_eq!(messages[0], original[0]);
        assert_eq!(messages[1], original[1]);
        assert_eq!(messages[2]["tool_call_id"], "old");
        assert!(
            messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("artifact://old")
        );
        assert!(
            messages[2]["content"].as_str().unwrap().len()
                < original[2]["content"].as_str().unwrap().len()
        );
        assert_eq!(&messages[3..], &original[3..]);
        assert_eq!(original[2]["content"], "source".repeat(600));
    }

    #[test]
    fn packet_and_placeholders_are_stable_until_task_state_changes() {
        let mut cache = PacketCache::default();
        let mut first = history();
        let packet = cache
            .project(&mut first, &tasks(), &json!({}), "s", None, "check", 2048)
            .unwrap();
        let mut second = history();
        second.push(json!({"role":"tool","tool_call_id":"new","content":"new evidence","dial_evidence":{"artifact":"artifact://new"}}));
        assert_eq!(
            cache.project(&mut second, &tasks(), &json!({}), "s", None, "check", 2048),
            Some(packet)
        );
        assert_eq!(first[2], second[2]);
    }

    #[test]
    fn does_not_hide_mutations_errors_or_nonretrievable_evidence() {
        for mode in ["mutation", "error", "no_artifact"] {
            let mut messages = history();
            match mode {
                "mutation" => messages[1]["tool_calls"][0]["function"]["name"] = json!("edit"),
                "error" => {
                    messages[2]["dial_evidence"]
                        .as_object_mut()
                        .unwrap()
                        .remove("observation");
                }
                _ => {
                    messages[2]["dial_evidence"]
                        .as_object_mut()
                        .unwrap()
                        .remove("artifact");
                }
            }
            let original = messages[2].clone();
            PacketCache::default().project(
                &mut messages,
                &tasks(),
                &json!({}),
                "s",
                None,
                "check",
                2048,
            );
            assert_eq!(messages[2], original);
        }
    }

    #[test]
    fn automatic_index_is_small_unique_and_preserves_explicit_evidence() {
        let mut messages = history();
        for i in 0..20 {
            messages.push(json!({"role":"tool","tool_call_id":format!("call_{i}"),
                "content":"source excerpt ".repeat(100),
                "dial_evidence":{"artifact":format!("artifact://{i:064}"),"observation":"src/lib.rs"}}));
        }
        // Duplicate the newest artifact: it must occupy only one slot.
        messages.push(messages.last().unwrap().clone());
        let original = messages.clone();
        let control = json!({"working_state":{"constraints":["preserve APIs"],
            "evidence":["artifact://explicit"]},"original_goal":"fix the regression"});
        let packet = PacketCache::default()
            .project(
                &mut messages,
                &tasks(),
                &control,
                "session",
                None,
                "check",
                4096,
            )
            .unwrap();
        let parsed: Value = serde_json::from_str(&packet).unwrap();
        let evidence = parsed["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), RECENT_EVIDENCE + 1);
        assert_eq!(evidence[0]["reference"], "artifact://explicit");
        assert_eq!(evidence[1]["reference"], format!("artifact://{:064}", 19));
        assert!(evidence.iter().all(|item| item.get("excerpt").is_none()));
        assert!(
            parsed["constraints"]
                .as_array()
                .unwrap()
                .contains(&json!("preserve APIs"))
        );
        assert_eq!(parsed["original_goal"], "fix the regression");
        // The newest results and original user message are untouched.
        assert_eq!(messages[0], original[0]);
        assert_eq!(
            &messages[messages.len() - RECENT_MESSAGES..],
            &original[original.len() - RECENT_MESSAGES..]
        );
        assert!(packet.len() < 1600, "packet bytes: {}", packet.len());
        // Reconstruct the previous fill-to-budget index using this exact fixture for comparison.
        let mut previous = parsed.clone();
        previous["evidence"] = json!([{"reference":"artifact://explicit"}]);
        for message in original.iter().rev() {
            let Some(reference) = message["dial_evidence"]["artifact"].as_str() else {
                continue;
            };
            let item = json!({"tool_call_id":message["tool_call_id"],"reference":reference,
                "excerpt":clipped(message["content"].as_str().unwrap_or(""), 160)});
            if !add_bounded(&mut previous, "evidence", item, 4096) {
                break;
            }
        }
        let old_bytes = previous.to_string().len();
        eprintln!("task packet fixture: {old_bytes} -> {} bytes", packet.len());
        assert!(packet.len() * 2 < old_bytes);
    }

    #[test]
    fn bounds_utf8_and_small_budgets_without_touching_history() {
        let mut messages = history();
        let original = messages.clone();
        assert!(
            PacketCache::default()
                .project(&mut messages, &tasks(), &json!({}), "s", None, "check", 20)
                .is_none()
        );
        assert_eq!(messages, original);
        let packet = PacketCache::default()
            .project(
                &mut messages,
                &tasks(),
                &json!({"working_state":{"constraints":["é".repeat(10000)]}}),
                "s",
                None,
                "check",
                1024,
            )
            .unwrap();
        assert!(packet.len() <= 1024);
        assert!(serde_json::from_str::<Value>(&packet).is_ok());
    }
}
