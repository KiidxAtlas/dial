//! Same-checkout session coordination settings.
use super::*;

pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "coordination.enabled",
        coordination.enabled,
        TO,
        "Session coordination",
        "Project peers",
        "Advertise this session and exchange advisory path claims and messages with other Dial sessions in the same checkout. Disabled in hermetic runs. Takes effect in a new session.",
        Kind::Bool,
        true,
    ),
    field!(
        "coordination.auto_wake",
        coordination.auto_wake,
        TO,
        "Session coordination",
        "Peer wake-ups",
        "Let fresh peer messages wake an idle open terminal session, whatever its last turn did (completed, blocked, failed or interrupted), and start a session with no task yet. At most eight wake-ups per user turn, at least 5 seconds apart; a message typed in a /watch pane refills the budget. May use paid model requests; normal approvals still apply. Takes effect in a new session.",
        Kind::Bool,
        true,
    ),
];
