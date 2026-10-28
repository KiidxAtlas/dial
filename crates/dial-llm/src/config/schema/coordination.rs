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
        "Let fresh peer messages resume completed or incomplete turns in the current task or conversation in an open terminal session, at most three times per user turn and at least 30 seconds apart. Never resumes blocked, failed, cancelled or human-input-waiting tasks. May use paid model requests; normal approvals still apply. Takes effect in a new session.",
        Kind::Bool,
        true,
    ),
];
