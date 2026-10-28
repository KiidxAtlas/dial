//! Host-provided tools for RPC sessions.
//!
//! The host registers tools with `set_host_tools`. When the model calls one, the agent emits a
//! `host_tool_call` event and blocks until the host answers with `host_tool_result`, the call
//! times out, or the turn is aborted. Calls go through the normal approval path before they reach
//! the host.

use super::Sender;
use crate::runtime::tool_output::ToolOutput;
use crate::tools::integrations::ext::{Tool, ToolContext};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

pub(super) const MAX_HOST_TOOLS: usize = 32;
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const POLL: Duration = Duration::from_millis(50);

/// The host's answer to one forwarded call.
pub(super) struct HostResult {
    pub(super) text: String,
    pub(super) is_error: bool,
}

type Pending = BTreeMap<String, mpsc::SyncSender<HostResult>>;

/// Pending forwarded calls, keyed by `call_id`. The controller completes them while the actor's
/// turn is blocked on the call.
#[derive(Default)]
pub(super) struct HostBridge {
    pending: Mutex<Pending>,
    next: AtomicU64,
    registered: AtomicBool,
    declared: Mutex<Vec<String>>,
}

impl HostBridge {
    fn lock(&self) -> Result<MutexGuard<'_, Pending>> {
        self.pending
            .lock()
            .map_err(|_| anyhow::anyhow!("host tool state unavailable"))
    }

    /// Completes a pending call. Unknown or already-finished call IDs are refused, so a late or
    /// repeated answer can never reach a different call.
    pub(super) fn complete(&self, call_id: &str, text: String, is_error: bool) -> Result<()> {
        let sender = self
            .lock()?
            .remove(call_id)
            .context("no host tool call is pending with that call_id")?;
        // The waiting call may have just given up; a closed receiver is not an error for the host.
        let _ = sender.send(HostResult { text, is_error });
        Ok(())
    }

    /// Refuses a second registration. Host tools are registered once per RPC process because
    /// the agent cannot replace an extension tool schema after registration.
    pub(super) fn ensure_unregistered(&self) -> Result<()> {
        ensure!(
            !self.registered.load(Ordering::SeqCst),
            "host tools are already registered for this process"
        );
        Ok(())
    }

    /// Marks registration as done only after the agent accepted every tool.
    pub(super) fn mark_registered(&self, names: Vec<String>) {
        *self
            .declared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = names;
        self.registered.store(true, Ordering::SeqCst);
    }

    /// Forgets the registration and every pending call, and returns the names that were declared
    /// so the caller can remove them from the session being left. Waiting calls end as cancelled.
    pub(super) fn reset(&self) -> Vec<String> {
        self.registered.store(false, Ordering::SeqCst);
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        std::mem::take(
            &mut *self
                .declared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    fn open_call(&self) -> Result<(String, mpsc::Receiver<HostResult>)> {
        let id = format!("host-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = mpsc::sync_channel(1);
        self.lock()?.insert(id.clone(), tx);
        Ok((id, rx))
    }

    fn close_call(&self, call_id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(call_id);
        }
    }
}

/// Runs one forwarded call to completion. `deliver` publishes the request for `call_id` and
/// reports whether a client received it; `gone` reports whether the transport has failed.
/// The call ends on the host's answer, on `timeout`, or when `interrupt` is raised (abort).
pub(super) fn run_call(
    bridge: &HostBridge,
    interrupt: &AtomicBool,
    label: &str,
    timeout: Duration,
    deliver: impl FnOnce(&str) -> bool,
    gone: impl Fn() -> bool,
) -> ToolOutput {
    let (call_id, answers) = match bridge.open_call() {
        Ok(opened) => opened,
        Err(error) => return ToolOutput::err(format!("{error:#}")),
    };
    if !deliver(&call_id) {
        bridge.close_call(&call_id);
        return ToolOutput::err(format!(
            "{label} could not be delivered: no client is attached"
        ));
    }
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        if interrupt.load(Ordering::SeqCst) || gone() {
            break Err(format!("{label} was cancelled"));
        }
        if Instant::now() >= deadline {
            break Err(format!(
                "{label} timed out after {} ms",
                timeout.as_millis()
            ));
        }
        match answers.recv_timeout(POLL) {
            Ok(result) => break Ok(result),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(format!("{label} was cancelled"));
            }
        }
    };
    bridge.close_call(&call_id);
    match outcome {
        Ok(result) if result.is_error => ToolOutput::err(result.text),
        Ok(result) => ToolOutput::ok(result.text),
        Err(message) => ToolOutput::err(message),
    }
}

/// One host tool as declared in `set_host_tools`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HostToolSpec {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) parameters: Value,
    pub(super) timeout: Duration,
}

/// Validates a `set_host_tools` payload. Every rejection names the offending tool.
pub(super) fn parse_specs(tools: &Value) -> Result<Vec<HostToolSpec>> {
    let items = tools
        .as_array()
        .context("set_host_tools requires params.tools as an array")?;
    ensure!(
        !items.is_empty(),
        "set_host_tools requires at least one tool"
    );
    ensure!(
        items.len() <= MAX_HOST_TOOLS,
        "set_host_tools accepts at most {MAX_HOST_TOOLS} tools"
    );
    let mut seen = BTreeSet::new();
    items
        .iter()
        .map(|item| {
            let name = item["name"].as_str().context("host tool requires a name")?;
            ensure!(
                !name.is_empty()
                    && name.chars().count() <= MAX_NAME_CHARS
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "host tool name {name:?} must be 1-{MAX_NAME_CHARS} characters of a-z, 0-9 or _"
            );
            ensure!(
                seen.insert(name.to_owned()),
                "duplicate host tool name {name:?}"
            );
            let description = item["description"]
                .as_str()
                .with_context(|| format!("host tool {name:?} requires a description"))?;
            ensure!(
                !description.trim().is_empty()
                    && description.chars().count() <= MAX_DESCRIPTION_CHARS,
                "host tool {name:?} description must be 1-{MAX_DESCRIPTION_CHARS} characters"
            );
            let parameters = item["parameters"].clone();
            ensure!(
                parameters["type"].as_str() == Some("object") && parameters.is_object(),
                "host tool {name:?} parameters must be a JSON object schema"
            );
            ensure!(
                parameters.to_string().len() <= MAX_SCHEMA_BYTES,
                "host tool {name:?} parameters exceed {MAX_SCHEMA_BYTES} bytes"
            );
            let timeout_ms = match item.get("timeout_ms") {
                None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
                Some(value) => value
                    .as_u64()
                    .filter(|ms| (1..=MAX_TIMEOUT_MS).contains(ms))
                    .with_context(|| {
                        format!("host tool {name:?} timeout_ms must be 1-{MAX_TIMEOUT_MS}")
                    })?,
            };
            Ok(HostToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                parameters,
                timeout: Duration::from_millis(timeout_ms),
            })
        })
        .collect()
}

/// A model-visible tool whose execution belongs to the host.
pub(super) struct HostTool {
    spec: HostToolSpec,
    bridge: Arc<HostBridge>,
    out: Sender,
    interrupt: Arc<AtomicBool>,
}

impl HostTool {
    pub(super) fn new(
        spec: HostToolSpec,
        bridge: Arc<HostBridge>,
        out: Sender,
        interrupt: Arc<AtomicBool>,
    ) -> Self {
        Self {
            spec,
            bridge,
            out,
            interrupt,
        }
    }
}

impl Tool for HostTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.spec.name,
                "description": self.spec.description,
                "parameters": self.spec.parameters,
            }
        })
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn call(&self, _ctx: &ToolContext, args: &Value) -> ToolOutput {
        run_call(
            &self.bridge,
            &self.interrupt,
            &format!("host tool {}", self.spec.name),
            self.spec.timeout,
            |call_id| {
                self.out.emit(json!({
                    "event":"host_tool_call",
                    "call_id": call_id,
                    "tool": self.spec.name,
                    "arguments": args,
                }))
            },
            || self.out.unavailable(),
        )
    }
}

#[cfg(test)]
mod tests;
