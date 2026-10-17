//! `[checkpoint]` settings for the checkpoint and rewind tools.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointConfig {
    pub enabled: bool,
}
