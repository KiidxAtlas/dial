//! `[message_routing]`: where a message typed while a turn is running goes.

use serde::{Deserialize, Serialize};

/// How a busy-time message that is not an explicit choice is routed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// Steer the running task. Deterministic: no model call and no provider or local slot.
    #[default]
    Steer,
    /// Ask the routing judge whether to steer or start an independent worker. A failed or
    /// uncertain decision steers.
    Classify,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub route: RoutingMode,
}
