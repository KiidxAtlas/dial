//! `[isolation]` settings: the requested backend preference.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preference {
    #[default]
    Auto,
    Clone,
    Overlay,
    Worktree,
    Copy,
    ProjFs,
}

/// `[isolation]` settings. Deserialized from the user's config file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IsolationConfig {
    pub backend: Preference,
}
