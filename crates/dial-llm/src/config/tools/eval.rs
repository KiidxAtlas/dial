//! `[eval]` settings for the evaluation tool.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvalConfig {
    /// `node` prefers the release-bundled runtime; explicit overrides must be operator-owned.
    pub node: String,
    /// Whole-operation cap; zero disables the cap, not cancellation or containment.
    pub timeout_secs: u64,
    /// Portable data/binary helpers; disable in a fresh session for an eval API ablation.
    pub portable_helpers: bool,
    pub auto_offer: bool,
    pub parallel_batches: bool,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            node: "node".into(),
            timeout_secs: 0,
            portable_helpers: true,
            auto_offer: true,
            parallel_batches: true,
        }
    }
}
