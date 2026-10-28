//! Host-served resource schemes (`set_host_uri_schemes`).
//!
//! The host declares scheme names. A `read` of `<scheme>://…` for a declared scheme emits
//! `host_uri_request` and blocks until the host answers with `host_uri_result`, the read times
//! out, or the turn is aborted. Results are bounded: a larger answer is replaced by an error.
//! Declarations are dropped on every session switch, like host tools.

use super::Sender;
use super::host_tools::{HostBridge, run_call};
use crate::tools::integrations::host_uri::{HostUriResolver, scheme_of};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub(super) const MAX_SCHEMES: usize = 16;
pub(super) const MAX_RESULT_BYTES: usize = 256 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Schemes that Dial resolves itself; a host declaration cannot shadow them.
const RESERVED: &[&str] = &[
    "file", "ssh", "pr", "issue", "conflict", "artifact", "http", "https",
];

/// Declared schemes plus the pending-call table for their reads.
#[derive(Default)]
pub(super) struct HostUris {
    schemes: Mutex<Vec<String>>,
    bridge: HostBridge,
}

impl HostUris {
    fn declared(&self) -> MutexGuard<'_, Vec<String>> {
        self.schemes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the declared schemes. An empty list stops serving every host scheme.
    pub(super) fn declare(&self, schemes: Vec<String>) {
        *self.declared() = schemes;
    }

    /// Drops every declaration and aborts pending reads (session switch).
    pub(super) fn reset(&self) {
        self.declared().clear();
        self.bridge.reset();
    }

    /// Delivers the host's answer to a pending read. Oversized answers become errors.
    pub(super) fn complete(&self, call_id: &str, output: String, is_error: bool) -> Result<()> {
        let (output, is_error) = if output.len() > MAX_RESULT_BYTES {
            (
                format!("host URI result exceeds {MAX_RESULT_BYTES} bytes"),
                true,
            )
        } else {
            (output, is_error)
        };
        self.bridge.complete(call_id, output, is_error)
    }

    fn serves(&self, scheme: &str) -> bool {
        self.declared().iter().any(|declared| declared == scheme)
    }
}

/// The resolver installed on the workspace. It routes reads through the host.
pub(super) struct Resolver {
    uris: Arc<HostUris>,
    out: Sender,
}

impl Resolver {
    pub(super) fn new(uris: Arc<HostUris>, out: Sender) -> Self {
        Self { uris, out }
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver").finish_non_exhaustive()
    }
}

impl HostUriResolver for Resolver {
    fn serves(&self, scheme: &str) -> bool {
        self.uris.serves(scheme)
    }

    fn read(&self, target: &str, cancel: &AtomicBool) -> std::result::Result<String, String> {
        let scheme = scheme_of(target).ok_or_else(|| "not a scheme target".to_owned())?;
        let output = run_call(
            &self.uris.bridge,
            cancel,
            &format!("host URI {scheme}"),
            READ_TIMEOUT,
            |call_id| {
                self.out.emit(json!({
                    "event":"host_uri_request",
                    "call_id": call_id,
                    "scheme": scheme,
                    "operation": "read",
                    "url": target,
                }))
            },
            || self.out.unavailable(),
        );
        if output.is_error {
            Err(output.text)
        } else {
            Ok(output.text)
        }
    }
}

fn valid_scheme_name(name: &str) -> bool {
    name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Parses the `schemes` array of `set_host_uri_schemes`.
pub(super) fn parse_schemes(value: &Value) -> Result<Vec<String>> {
    let items = value.as_array().context("schemes must be an array")?;
    ensure!(
        items.len() <= MAX_SCHEMES,
        "at most {MAX_SCHEMES} host URI schemes can be declared"
    );
    let mut schemes: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let raw = item.as_str().context("each scheme must be a string")?;
        ensure!(
            valid_scheme_name(raw),
            "scheme {raw:?} must start with a letter and use only letters, digits, '+', '-' or '.' (at most 32 characters)"
        );
        let scheme = raw.to_ascii_lowercase();
        ensure!(
            !RESERVED.contains(&scheme.as_str()),
            "scheme {scheme:?} is built in and cannot be declared by a host"
        );
        ensure!(
            !schemes.contains(&scheme),
            "scheme {scheme:?} is declared twice"
        );
        schemes.push(scheme);
    }
    Ok(schemes)
}

#[cfg(test)]
mod tests;
