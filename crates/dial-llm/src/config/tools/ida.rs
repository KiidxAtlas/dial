//! `[ida]` IDA Pro runtime configuration.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IdaConfig {
    /// Python interpreter that can import `idapro` (idalib). Empty uses `python3` from PATH.
    pub python: String,
    /// IDA installation directory exported to the worker as `IDADIR`. Empty inherits the
    /// process environment.
    pub idadir: String,
    /// Default deadline in seconds for database edits and `exec` scripts (1..=3600).
    pub timeout_secs: u64,
}

impl Default for IdaConfig {
    fn default() -> Self {
        Self {
            python: String::new(),
            idadir: String::new(),
            timeout_secs: 120,
        }
    }
}
