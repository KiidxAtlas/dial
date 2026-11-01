//! One shared count of recovery attempts since the turn last made progress. Each recovery
//! mechanism (stream stalls, truncation, overflow, unusable replies, rejected previews,
//! portable-guidance regenerations, automatic continues) keeps its own cap, but those
//! caps reset independently: a model that alternates failure classes, or interleaves them with
//! failing tool calls, never reaches any single one. The ledger bounds the combination with a
//! graceful stop. It never injects a redirect into the model context. Coaching and model failover
//! stay with their existing configured owners (automatic escalation and the fallback chain).
//!
//! Only novel observations and committed mutations repay recovery debt. Tool rounds are charged
//! once, not once per failed call; successful bookkeeping neither advances nor forgives recovery.
use super::*;

#[cfg(test)]
mod tests;

/// Total recovery debt before the turn stops, including failed tool rounds. This bounds both
/// repeated tool failures and recovery loops that alternate between mechanisms.
pub(in crate::agent) const STOP_AT: u32 = 16;

/// A recovery attempt: the harness retries or corrects instead of moving the task forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::agent) enum Setback {
    StreamStall,
    Truncation,
    Overflow,
    UnusableReply,
    NoAnswer,
    RejectedPreview,
    GuidanceRegeneration,
    AutomaticContinue,
    UnproductiveTools,
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
            Self::AutomaticContinue => "automatic continue",
            Self::UnproductiveTools => "tool round without productive progress",
        }
    }
}

/// Whether another setback remains within the shared recovery allowance.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::agent) enum Rung {
    /// The mechanism's own recovery proceeds.
    Retry,
    /// Recovery is not converging; end the turn with this explanation.
    Stop(String),
}

#[derive(Default)]
pub(in crate::agent) struct ProgressLedger {
    since_progress: std::collections::BTreeMap<Setback, u32>,
}

impl ProgressLedger {
    /// Productive execution repays the debt; bookkeeping alone never calls this.
    pub(in crate::agent) fn progressed(&mut self) {
        *self = Self::default();
    }

    pub(in crate::agent) fn tool_round(
        &mut self,
        progress: crate::agent::run::engine::RoundProgress,
    ) -> Option<Setback> {
        use crate::agent::run::engine::RoundProgress;
        match progress {
            RoundProgress::Productive => {
                self.progressed();
                None
            }
            RoundProgress::Stalled => Some(Setback::UnproductiveTools),
            RoundProgress::Neutral => None,
        }
    }

    pub(in crate::agent) fn setback(&mut self, kind: Setback) -> Rung {
        let count = self.since_progress.entry(kind).or_default();
        *count = count.saturating_add(1);
        self.stop_reason().map_or(Rung::Retry, Rung::Stop)
    }

    pub(in crate::agent) fn stop_reason(&self) -> Option<String> {
        let total = self
            .since_progress
            .values()
            .copied()
            .fold(0_u32, u32::saturating_add);
        (total >= STOP_AT).then(|| format!(
            "Blocked: {total} recovery attempts in a row made no progress ({}). Completed effects and the conversation are kept; no completion was recorded. Send a direction or say \"continue\" to resume.",
            self.summary()
        ))
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
        engine: &mut crate::agent::run::engine::Engine,
        kind: Setback,
        step: u32,
        ui: &mut dyn Ui,
    ) -> Result<Option<TurnReport>> {
        self.efficiency_metrics.recovery(kind.label());
        self.recovery_report(turn, engine.setback(kind), step, ui)
    }

    /// Parent settlement adjusts raw tool progress for admission failures before charging it.
    pub(super) fn tool_progress(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        progress: crate::agent::run::engine::RoundProgress,
        step: u32,
        ui: &mut dyn Ui,
    ) -> Result<Option<TurnReport>> {
        if progress == crate::agent::run::engine::RoundProgress::Stalled {
            self.efficiency_metrics
                .recovery(Setback::UnproductiveTools.label());
        }
        self.recovery_report(turn, engine.record_progress(progress), step, ui)
    }

    fn recovery_report(
        &mut self,
        turn: &mut TurnState<'_>,
        rung: Rung,
        step: u32,
        ui: &mut dyn Ui,
    ) -> Result<Option<TurnReport>> {
        match rung {
            Rung::Retry => Ok(None),
            Rung::Stop(mut message) => {
                self.record(step, "recovery_ladder", json!({"stopped":message}))?;
                if let Some(candidate) = turn.completion_candidate.take() {
                    message.push_str("\n\n");
                    message.push_str(&candidate);
                }
                self.stop_turn(ui, message, step + 1).map(|mut report| {
                    report.outcome = TurnOutcome::Blocked;
                    Some(report)
                })
            }
        }
    }
}
