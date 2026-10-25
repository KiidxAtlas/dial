//! Same-checkout session coordination settings.
use super::*;

pub(super) static DECLARATIONS: &[SettingDef] = &[field!(
    "coordination.enabled",
    coordination.enabled,
    TO,
    "Session coordination",
    "Project peers",
    "Advertise this session and exchange advisory path claims and messages with other Dial sessions in the same checkout. Disabled in hermetic runs. Takes effect in a new session.",
    Kind::Bool,
    true,
)];
