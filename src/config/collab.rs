//! `[collab]` relay and guest settings.

use serde::{Deserialize, Serialize};

/// `[collab]`: relay and hosted-viewer endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub relay_url: String,
    pub web_url: String,
    pub allow_guest_steering: bool,
}
