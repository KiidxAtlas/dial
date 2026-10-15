//! End-to-end encrypted collaboration transport and local relay.
use serde::{Deserialize, Serialize};

pub mod crypto;
pub mod guest;
pub mod host;
pub mod protocol;
pub mod registry;
pub mod room;
pub mod server;
pub mod socket;

/// `[collab]`: relay and hosted-viewer endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub relay_url: String,
    pub web_url: String,
    pub allow_guest_steering: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            relay_url: String::new(),
            web_url: String::new(),
            allow_guest_steering: false,
        }
    }
}
