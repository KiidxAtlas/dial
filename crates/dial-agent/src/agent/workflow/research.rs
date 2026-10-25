//! A synthesis boundary for long runs of different, successful observations.
//! This does not decide that research is wrong or require a mutation: read-only work can renew
//! its window by reporting a finding tied to returned evidence. Claims remain unverified.
use crate::{
    llm::{Reply, ToolCall},
    runtime::tool_output::ToolOutput,
};
use serde::Deserialize;
use std::collections::VecDeque;

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

impl Research {
    pub fn observe(&mut self, call: &ToolCall, output: &ToolOutput, progressed: bool) {
        if progressed {
            self.rounds = 0;
            self.attempts = 0;
            self.evidence.clear();
            self.progressed = true;
        }
        if output.is_error
            || !matches!(
                call.name.as_str(),
                "read"
                    | "grep"
                    | "glob"
                    | "find"
                    | "ast_search"
                    | "dep_source"
                    | "artifact_read"
                    | "web_search"
                    | "web_fetch"
                    | "lsp"
                    | "bash"
            )
        {
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
    /// Retained provider schemas do not grant permission to dispatch on this request.
    pub fn synthesize(&mut self, reply: &Reply) -> Synthesis {
        let parsed = if reply.tool_calls.is_empty()
            && !reply.dropped_tool_call()
            && reply.finish_reason == "stop"
            && reply.text.len() <= 8192
        {
            serde_json::from_str::<Checkpoint>(&reply.text).ok()
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
                return Synthesis::Accepted;
            }
        }
        self.attempts += 1;
        if self.attempts >= MAX_ATTEMPTS {
            Synthesis::Incomplete("Stopped with incomplete work: the research synthesis checkpoint was not satisfied after three attempts. No tools from these checkpoint replies were executed; earlier effects and history are preserved. Findings are model claims, not verification proof.".into())
        } else {
            Synthesis::Retry("Research checkpoint not accepted. No tools from that response were executed. Return only the requested JSON, with a new nonempty finding, evidence IDs from the returned observations, and a concrete next_action. Repeating an earlier finding or inventing evidence IDs does not renew the research window.".into())
        }
    }
}

#[cfg(test)]
mod tests;
