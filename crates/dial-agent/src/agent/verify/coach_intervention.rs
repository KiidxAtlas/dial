//! Unified intervention envelope for coach, advisor, quality-review, and failure-escalation
//! continuations.
//!
//! Ensures every review or coaching surface shares one content-free filter, one optional clipping
//! boundary, and one canonical `NoticeBus` message materialization.
use crate::agent::run::guidance::{self, Priority};
use crate::agent::safety::guard;
use serde_json::{Value, json};

#[cfg(test)]
mod tests;

/// Origin of a bounded coaching or review intervention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum InterventionKind {
    /// Live per-round coach review (prioritized by coach severity).
    RoundReview,
    /// Unresolved advisor blocker re-raised once before completion.
    BlockerReraise,
    /// Opt-in self-review before completion (`verification.quality_review`).
    QualityReview,
    /// Bounded completion-review gap repair note.
    CompletionRepair,
    /// Automatic failure-escalation advice after consecutive failed tool rounds.
    FailureEscalation,
}

/// Canonical intervention emitted by any coach/review surface before entering `NoticeBus`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::agent) struct CoachIntervention {
    pub(in crate::agent) kind: InterventionKind,
    pub(in crate::agent) priority: Option<Priority>,
    pub(in crate::agent) body: String,
}

impl CoachIntervention {
    /// Build a prioritized advisory intervention, dropping empty or content-free text.
    pub(in crate::agent) fn advisory(
        kind: InterventionKind,
        priority: Priority,
        raw_content: &str,
        body: impl Into<String>,
    ) -> Option<Self> {
        if guidance::is_content_free(raw_content) {
            return None;
        }
        Some(Self {
            kind,
            priority: Some(priority),
            body: body.into(),
        })
    }

    /// Build a transient protocol-level intervention (completion repair or failure escalation),
    /// rejecting empty or content-free payloads and clipping raw advice when bounded.
    pub(in crate::agent) fn transient(
        kind: InterventionKind,
        raw_content: &str,
        max_chars: Option<usize>,
        wrap: impl FnOnce(&str) -> String,
    ) -> Option<Self> {
        let trimmed = raw_content.trim();
        if guidance::is_content_free(trimmed) {
            return None;
        }
        let body = match max_chars {
            Some(limit) => wrap(&guard::clip_middle(trimmed, limit)),
            None => wrap(trimmed),
        };
        Some(Self {
            kind,
            priority: None,
            body,
        })
    }

    /// Materialize the canonical request-tail message for `NoticeBus`.
    pub(in crate::agent) fn into_message(self) -> Value {
        match self.priority {
            Some(priority) => guidance::note(priority, self.body),
            None => json!({
                "role": "user",
                "dial_transient": true,
                "content": self.body,
            }),
        }
    }
}
