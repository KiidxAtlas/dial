//! `[ask]` settings for human questions.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AskConfig {
    pub enabled: bool,
    /// Seconds after actual presentation; zero disables automatic selection.
    pub timeout_seconds: u64,
    pub notify: bool,
}

impl Default for AskConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_seconds: 0,
            notify: true,
        }
    }
}
