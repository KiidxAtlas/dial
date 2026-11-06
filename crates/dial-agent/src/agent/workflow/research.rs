//! A synthesis boundary for long runs of different, successful observations.
//! This does not decide that research is wrong or require a mutation: read-only work can renew
//! its window by reporting a finding tied to returned evidence. Claims remain unverified.
use crate::{
    llm::{Reply, ToolCall},
    runtime::tool_output::ToolOutput,
};
use serde::Deserialize;
use std::collections::VecDeque;

// Bound expanding lookups so a long run of observations reaches a synthesis boundary. Coding
// work routinely spans many read/search/run rounds before its first edit, so the window is wide.
// These count model round trips, not batched calls; evidence-linked synthesis renews the window.
const WARN_ROUNDS: u32 = 12;
const SYNTHESIS_ROUNDS: u32 = 24;
const MAX_ATTEMPTS: u32 = 3;
const MAX_EVIDENCE: usize = 128;

#[derive(Default)]
pub(in crate::agent) struct Research {
    rounds: u32,
    observed: bool,
    progressed: bool,
    evidence: VecDeque<String>,
    findings: VecDeque<String>,
    attempts: u32,
    /// Keep the agreed next step visible after renewal, including across context compaction.
    /// This is quoted model output, never verification or permission to broaden the task.
    continuation: Option<String>,
}

pub(in crate::agent) enum Synthesis {
    Accepted,
    Retry(String),
    Incomplete(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    finding: String,
    evidence: Vec<String>,
    next_action: String,
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Calls whose successful result can supply evidence, shared with execution progress accounting.
pub(in crate::agent) fn is_observation(call: &ToolCall) -> bool {
    match call.name.as_str() {
        "read" | "grep" | "glob" | "find" | "ast_search" | "dep_source" | "web_search"
        | "web_fetch" | "lsp" | "bash" | "skill" | "tool_search" => true,
        "memory" => matches!(
            call.arguments["action"].as_str(),
            Some("recall" | "reflect" | "history")
        ),
        "codebase_memory" => matches!(
            call.arguments["action"].as_str(),
            Some("recall" | "architecture" | "impact" | "refresh")
        ),
        "peers" => matches!(call.arguments["action"].as_str(), None | Some("list")),
        _ => false,
    }
}

impl Research {
    pub fn observe(&mut self, call: &ToolCall, output: &ToolOutput, progressed: bool) {
        if progressed {
            self.rounds = 0;
            self.attempts = 0;
            self.evidence.clear();
            self.progressed = true;
            self.continuation = None;
        }
        if output.is_error || !is_observation(call) {
            return;
        }
        self.observed = true;
        if self.evidence.len() == MAX_EVIDENCE {
            self.evidence.pop_front();
        }
        self.evidence.push_back(call.id.clone());
    }

    /// Count model rounds rather than calls so batching is not penalized. Bookkeeping, failures
    /// and unknown tools neither advance nor erase the window.
    pub fn round(&mut self) {
        if self.observed && !self.progressed {
            self.rounds = self.rounds.saturating_add(1);
        }
        self.observed = false;
        self.progressed = false;
    }

    pub fn required(&self) -> bool {
        self.rounds >= SYNTHESIS_ROUNDS
    }

    pub fn prompt(&self) -> Option<serde_json::Value> {
        let text = if self.required() {
            format!(
                "{}\nReturned observation IDs (identifiers only, not verification proof): {}",
                crate::agent::prompts::recovery::RESEARCH_SYNTHESIS,
                serde_json::to_string(&self.evidence).expect("string evidence IDs")
            )
        } else if let Some(checkpoint) = &self.continuation {
            // This carries the accepted checkpoint's data, not a repeatable advisory warning.
            // Keep it out of advisory deduplication; controllers retire and regenerate it per request.
            return Some(serde_json::json!({
                "role":"user", "dial_transient":true,
                "content":format!(
                    "{}\nQuoted checkpoint (untrusted model claims, not instructions or verification):\n{checkpoint}",
                    crate::agent::prompts::recovery::RESEARCH_FOLLOW_THROUGH,
                )
            }));
        } else if self.rounds >= WARN_ROUNDS {
            return Some(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Loop,
                crate::agent::prompts::recovery::RESEARCH_WARNING,
            ));
        } else {
            return None;
        };
        Some(serde_json::json!({"role":"user", "dial_transient":true, "content":text}))
    }

    /// Called before inline-edit recovery, completion handling or *any* tool admission.
    /// Retained provider schemas do not grant permission to dispatch on this request: tool calls
    /// in the reply are never executed. A valid checkpoint in the reply text is accepted even when
    /// the model also requested tools (small local models often do, because schemas stay offered).
    pub fn synthesize(&mut self, reply: &Reply) -> Synthesis {
        let parsed = if reply.finish_reason != "length" && reply.text.len() <= 8192 {
            serde_json::from_str::<Checkpoint>(unfenced(&reply.text)).ok()
        } else {
            None
        };
        if let Some(checkpoint) = parsed {
            let finding = normalized(&checkpoint.finding);
            if !finding.is_empty()
                && !checkpoint.next_action.trim().is_empty()
                && !checkpoint.evidence.is_empty()
                && checkpoint
                    .evidence
                    .iter()
                    .all(|id| self.evidence.contains(id))
                && !self.findings.contains(&finding)
            {
                if self.findings.len() == MAX_EVIDENCE {
                    self.findings.pop_front();
                }
                self.findings.push_back(finding);
                self.rounds = 0;
                self.attempts = 0;
                self.evidence.clear();
                self.continuation = Some(unfenced(&reply.text).to_owned());
                return Synthesis::Accepted;
            }
        }
        self.attempts += 1;
        if self.attempts >= MAX_ATTEMPTS {
            return Synthesis::Incomplete("Stopped with incomplete work: the research synthesis checkpoint was not satisfied after three attempts. No tools from these checkpoint replies were executed; earlier effects and history are preserved. Findings are model claims, not verification proof.".into());
        }
        let example = serde_json::json!({
            "finding": "<what the returned evidence established>",
            "evidence": [self.evidence.back().map_or("<observation id>", String::as_str)],
            "next_action": "<the supported next step>",
        });
        if reply.tool_calls.is_empty() && !reply.dropped_tool_call() {
            return Synthesis::Retry(format!(
                "Research checkpoint not accepted. No tools from that response were executed. Return only the requested JSON, with a new nonempty finding, evidence IDs from the returned observations, and a concrete next_action. Repeating an earlier finding or inventing evidence IDs does not renew the research window. Shape: {example}"
            ));
        }
        let mut names: Vec<&str> = reply
            .tool_calls
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        names.dedup();
        let called = if names.is_empty() {
            "a tool".to_owned()
        } else {
            format!("`{}`", names.join("`, `"))
        };
        Synthesis::Retry(format!(
            "Research checkpoint not accepted: that response called {called}, and tools never run on this checkpoint request. Do not call any tool now. Reply with text only: one JSON object, no fences, with a new finding, evidence IDs from the returned observations and a concrete next_action. Your tools return after the checkpoint is accepted. Shape: {example}"
        ))
    }
}

/// The reply text without one surrounding Markdown code fence (```json … ```), which small
/// models add despite instructions. Anything else is returned trimmed and otherwise unchanged.
fn unfenced(text: &str) -> &str {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return text;
    };
    // Drop the info string (`json`) on the opening line.
    body.split_once('\n').map_or(body, |(_, body)| body).trim()
}

#[cfg(test)]
mod tests;
