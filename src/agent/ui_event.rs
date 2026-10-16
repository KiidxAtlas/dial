//! Typed controller events for presentation. Each event keeps the exact sentence the plain
//! `notice` channel always carried, so text surfaces (print, RPC, tests) are unchanged, while a
//! rich surface can place it in its own lane: a one-line context fact, a history divider, or an
//! alert, instead of a generic notice panel.
use serde::Serialize;

/// What the controller is reporting. The kind decides the lane and icon; the text stays the
/// authoritative wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Live steering reached the model, or the turn replanned around a new instruction.
    Steering,
    /// A skill's instructions were loaded into context.
    Skill,
    /// Tools were enabled for the next model round.
    Tools,
    /// Plan-mode and todo-plan transitions.
    Plan,
    /// Coach / advisor reviews and escalation advice (unverified by definition).
    Coach,
    /// Output from user-configured hooks.
    Hook,
    /// Configured behavioral checks.
    Verification,
    /// Investigation checkpoints and rewinds.
    Investigation,
    /// Model routing: fallbacks, prewalk switches, lean tool sets.
    Model,
    /// The loop corrected itself: stalls, repeated calls, output-limit retries.
    Recovery,
    /// Saved recipes and learned project knowledge.
    Learning,
    /// Delegated subagent progress and verdicts.
    Subagent,
    /// Automatic continuation of an unfinished turn.
    Continue,
    /// History was summarised into a shorter context.
    Compaction,
    /// A new notes-backed context window replaced the old one.
    ContextReset,
}

/// Where a surface should place an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// One quiet line beside the work it describes.
    Context,
    /// A boundary across the transcript: what came before is summarised.
    Divider,
    /// Needs the reader's attention.
    Alert,
}

impl EventKind {
    pub fn lane(self) -> Lane {
        match self {
            Self::Compaction | Self::ContextReset => Lane::Divider,
            Self::Recovery => Lane::Alert,
            _ => Lane::Context,
        }
    }

    /// Short lowercase label a surface may print beside the icon. At most seven characters, so
    /// it fits the transcript's tool-verb column.
    pub fn label(self) -> &'static str {
        match self {
            Self::Steering => "steer",
            Self::Skill => "skill",
            Self::Tools => "tools",
            Self::Plan => "plan",
            Self::Coach => "coach",
            Self::Hook => "hook",
            Self::Verification => "check",
            Self::Investigation => "explore",
            Self::Model => "model",
            Self::Recovery => "recover",
            Self::Learning => "learned",
            Self::Subagent => "agent",
            Self::Continue => "resume",
            Self::Compaction => "compacted",
            Self::ContextReset => "new context",
        }
    }
}

/// One controller event.
#[derive(Clone, Debug, Serialize)]
pub struct UiEvent {
    pub kind: EventKind,
    /// The full sentence, identical to what the plain notice channel shows.
    pub text: String,
    /// Context tokens before and after, for dividers that shrink the window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<(u64, u64)>,
}

impl UiEvent {
    pub fn new(kind: EventKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            tokens: None,
        }
    }

    pub fn with_tokens(mut self, before: u64, after: u64) -> Self {
        self.tokens = Some((before, after));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_serialize_as_snake_case_names() {
        assert_eq!(serde_json::to_value(Lane::Context).unwrap(), "context");
        assert_eq!(serde_json::to_value(Lane::Divider).unwrap(), "divider");
        assert_eq!(serde_json::to_value(Lane::Alert).unwrap(), "alert");
        assert_eq!(
            serde_json::to_value(EventKind::ContextReset.lane()).unwrap(),
            "divider"
        );
        assert_eq!(
            serde_json::to_value(EventKind::Recovery.lane()).unwrap(),
            "alert"
        );
    }
}
