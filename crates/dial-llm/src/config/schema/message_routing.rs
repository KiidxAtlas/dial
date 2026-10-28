//! Routing of messages typed while a turn is running.
use super::*;

const ROUTE: EnumValues = &[("steer", "Steer"), ("classify", "Classify")];

pub(super) static DECLARATIONS: &[SettingDef] = &[field!(
    "message_routing.route",
    message_routing.route,
    IN,
    "Input",
    "Busy message routing",
    "How a message typed while a turn runs routes when you do not pick an option. steer (default) steers the running task deterministically, with no model call. classify asks the routing judge whether to steer or start an independent worker; a failed or uncertain decision steers. Explicit Follow-up and Independent worker choices are always deterministic.",
    Kind::Enum(ROUTE),
)];
