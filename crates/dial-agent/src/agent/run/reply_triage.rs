//! Reply triage shared by the foreground turn loop (`run/turn/step.rs`) and worker loops
//! (`workers/child.rs`). This module owns the recovery *decision* and the model-visible
//! correction text, so a rule is written once and both loops decide identically. Each loop keeps
//! its own UI, journaling, progress ledger, message placement and dispatch.
use crate::agent::run::engine::Engine;
use crate::agent::safety::guard::LengthAction;
use crate::agent::workflow::reasoning;
use crate::llm::Reply;

/// Why an empty truncated reply triggered the reasoning gate.
pub(in crate::agent) const REASONING_REASON: &str =
    "the output token limit was spent without any answer or tool call";

/// What a loop knows about the request that produced a truncated reply.
pub(in crate::agent) struct LengthFacts {
    /// `[reasoning_guard] enabled`.
    pub reasoning_guard: bool,
    /// The retry must first record a todo plan (foreground `require_plan` with no open plan).
    pub plan_gate: bool,
    pub requested_tokens: u32,
    pub window: u32,
    pub prompt_tokens: u64,
}

/// How to recover from a reply cut off at the output limit. Where each message goes (history,
/// request-local note, UI) stays with the loop that applies the decision.
pub(in crate::agent) enum Truncation {
    /// The budget went to hidden reasoning. Discard the reply and retry after `message`.
    Reasoning { message: String, planned: bool },
    /// Retry the same request with a larger output limit.
    Raise(u32),
    /// Keep the partial answer and ask for the rest with `message`.
    Continue {
        partial: Option<String>,
        message: String,
    },
    /// Length recovery is exhausted; stop with this reason. Completed work is kept.
    Stop(String),
    /// The reasoning gate hit again after its one recovery; stop with this reason. Workers
    /// report this as a failure rather than incomplete work.
    ReasoningExhausted(String),
}

/// Decide how to recover from `reply`, or `None` when it was not cut off at the output limit.
pub(in crate::agent) fn truncation(
    engine: &mut Engine,
    reasoning: &mut reasoning::Recovery,
    reply: &Reply,
    facts: &LengthFacts,
) -> Option<Truncation> {
    if reply.finish_reason != "length" {
        return None;
    }
    if facts.reasoning_guard && reply.text.trim().is_empty() && reply.tool_calls.is_empty() {
        let recovered = if facts.plan_gate {
            reasoning.intercept_planned(REASONING_REASON)
        } else {
            reasoning.intercept(REASONING_REASON)
        };
        return Some(match recovered {
            Ok(message) => Truncation::Reasoning {
                message,
                planned: facts.plan_gate,
            },
            Err(reason) => Truncation::ReasoningExhausted(reason),
        });
    }
    Some(
        match engine.length(
            facts.requested_tokens,
            facts.window,
            facts.prompt_tokens,
            reply,
        ) {
            LengthAction::Retry { max_tokens } => Truncation::Raise(max_tokens),
            LengthAction::Nudge { partial, message } => Truncation::Continue { partial, message },
            LengthAction::Incomplete(reason) => Truncation::Stop(reason),
        },
    )
}

/// A reply whose tool call could not be used as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum FormFailure {
    /// The transport dropped a call whose arguments were not valid JSON.
    DroppedCall,
    /// The model wrote a tool call as plain text instead of using the tool-call channel.
    TextCall,
}

impl FormFailure {
    /// Classify `reply` against the offered tool names.
    pub fn of(reply: &Reply, names: &[String]) -> Option<Self> {
        if reply.dropped_tool_call() {
            Some(Self::DroppedCall)
        } else if reply.tool_calls.is_empty()
            && crate::agent::base::helpers::looks_like_text_call(&reply.text, names)
        {
            Some(Self::TextCall)
        } else {
            None
        }
    }

    /// The journal and training-recovery kind.
    pub fn kind(self) -> &'static str {
        match self {
            Self::DroppedCall => "dropped_call",
            Self::TextCall => "text_call",
        }
    }

    /// The correction sent before the next attempt. Both start with prefixes
    /// `learn::harvest` recognizes as corrections.
    pub fn correction(self) -> &'static str {
        match self {
            Self::DroppedCall => crate::agent::prompts::recovery::DROPPED_TOOL_CALL,
            Self::TextCall => crate::agent::prompts::recovery::TEXT_TOOL_CALL,
        }
    }
}

#[cfg(test)]
mod tests;
