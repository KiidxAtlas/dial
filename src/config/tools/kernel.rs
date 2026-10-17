//! `[kernel]` settings for the persistent kernel tool.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig {
    pub python: String,
    pub timeout_secs: u64,
    /// Project environment: `"auto"`, `"off"`, or an explicit virtualenv/conda prefix.
    pub venv: String,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            python: DEFAULT_PYTHON.into(),
            timeout_secs: 0,
            venv: "auto".into(),
        }
    }
}

pub(crate) const DEFAULT_PYTHON: &str = "python3";
