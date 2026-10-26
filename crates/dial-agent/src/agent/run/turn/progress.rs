//! One shared count of recovery attempts since the turn last made progress. Each recovery
//! mechanism (stream stalls, truncation, overflow, unusable replies, rejected previews and
//! completions, portable-guidance regenerations, automatic continues) keeps its own cap, but those
//! caps reset independently: a model that alternates failure classes, or interleaves them with
//! failing tool calls, never reaches any single one. The ledger bounds the combination with a
//! fixed ladder: retry within each mechanism's own cap, then one redirecting note, then a
//! graceful stop that keeps the candidate answer. Coaching and model failover stay with their
//! existing owners (automatic escalation and the fallback chain).
//!
//! Progress is a tool result that is new and not an error: productive work is never capped.
use super::*;

#[cfg(test)]
mod tests;

/// Recovery attempts since the last progress before the model is told to change approach.
pub(super) const REDIRECT_AT: u32 = 6;
/// Recovery attempts since the last progress before the turn stops. Above every single
/// mechanism's own cap, so the ledger only ends loops that cross mechanisms.
const STOP_AT: u32 = 16;

/// A recovery attempt: the harness retries or corrects instead of moving the task forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Setback {
    StreamStall,
    Truncation,
    Overflow,
    UnusableReply,
    NoAnswer,
    RejectedPreview,
    GuidanceRegeneration,
    RejectedCompletion,
    AutomaticContinue,
}

impl Setback {
    fn label(self) -> &'static str {
        match self {
            Self::StreamStall => "stalled stream",
            Self::Truncation => "truncated output",
            Self::Overflow => "context overflow",
            Self::UnusableReply => "unusable tool call",
            Self::NoAnswer => "reply without an answer or action",
            Self::RejectedPreview => "rejected edit preview",
            Self::GuidanceRegeneration => "portable-guidance regeneration",
            Self::RejectedCompletion => "rejected completion",
            Self::AutomaticContinue => "automatic continue",
        }
    }
}

/// The ladder's answer to one more setback.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Rung {
    /// The mechanism's own recovery proceeds.
    Retry,
    /// Recovery proceeds, and this note asks for a different approach.
    Redirect(String),
    /// Recovery is not converging; end the turn with this explanation.
    Stop(String),
}

#[derive(Default)]
pub(super) struct ProgressLedger {
    since_progress: std::collections::BTreeMap<Setback, u32>,
    redirected: bool,
}

impl ProgressLedger {
    /// A new, successful tool result: every recovery so far did its job.
    pub(super) fn progressed(&mut self) {
        *self = Self::default();
    }

    pub(super) fn setback(&mut self, kind: Setback) -> Rung {
        *self.since_progress.entry(kind).or_default() += 1;
        let total: u32 = self.since_progress.values().sum();
        if total >= STOP_AT {
            return Rung::Stop(format!(
                "Stopped with incomplete work: {total} recovery attempts in a row made no progress ({}). No partial tool calls were executed; completed effects and the conversation are kept; no completion was recorded. Send a direction or say \"continue\" to resume.",
                self.summary()
            ));
        }
        if total >= REDIRECT_AT && !std::mem::replace(&mut self.redirected, true) {
            return Rung::Redirect(format!(
                "[Dial] {total} recovery attempts in a row made no progress ({}). Retrying the same way is not working. Change approach: make one small tool call known to work, answer directly from the evidence you have, or report the concrete blocker.",
                self.summary()
            ));
        }
        Rung::Retry
    }

    fn summary(&self) -> String {
        self.since_progress
            .iter()
            .map(|(kind, count)| format!("{} ×{count}", kind.label()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Agent<'_> {
    /// Record a setback before its recovery has any effect. `Some` is the report of a turn the
    /// ladder ended; it carries the candidate answer when a completion was rejected.
    pub(super) fn setback(
        &mut self,
        turn: &mut TurnState<'_>,
        kind: Setback,
        step: u32,
        ui: &mut dyn Ui,
    ) -> Result<Option<TurnReport>> {
        match turn.progress.setback(kind) {
            Rung::Retry => Ok(None),
            Rung::Redirect(note) => {
                turn.pending_nudges.push(crate::agent::run::guidance::note(
                    crate::agent::run::guidance::Priority::Loop,
                    note,
                ));
                Ok(None)
            }
            Rung::Stop(mut message) => {
                self.record(step, "recovery_ladder", json!({"stopped":message}))?;
                if let Some(candidate) = turn.completion_candidate.take() {
                    message.push_str("\n\n");
                    message.push_str(&candidate);
                }
                self.stop_turn(ui, message, step + 1).map(Some)
            }
        }
    }
}
