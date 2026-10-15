//! Security scanning: plans, stored scan results, SARIF and Codex Security bundle import/export,
//! finding comparison, and Codex Security cloud configurations.
//!
//! Enabled by listing `security_scan` in `integrations`, which gates the tool and the
//! `/security` command's operations that need it.

use std::sync::Arc;

use crate::config::types::Config;
use crate::tools::ext::Tool;

pub mod auth;
pub mod cloud;
pub mod codex;
pub mod command;
pub mod comparison;
pub mod contracts;
pub mod coordinator;
pub mod ops;
pub mod preflight;
pub mod review;
pub mod sarif;
pub mod scan;
pub mod store;
pub mod tool;

pub use review::Reviewer;

/// Integration name that enables the `security_scan` tool.
pub const INTEGRATION: &str = "security_scan";

/// The `security_scan` tool when enabled by `integrations`; otherwise no tools.
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    if config.integrations.iter().any(|name| name == INTEGRATION) {
        vec![tool::tool()]
    } else {
        Vec::new()
    }
}
