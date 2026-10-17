//! `[eval]` settings for the evaluation tool.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvalConfig {
    /// Operator-installed Node executable, never a workspace-owned program.
    pub node: String,
    /// Whole-operation cap; zero disables the cap, not cancellation or containment.
    pub timeout_secs: u64,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            node: "node".into(),
            timeout_secs: 0,
        }
    }
}
