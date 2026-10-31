//! ACP client filesystem bridge (`fs/read_text_file`, `fs/write_text_file`).
//!
//! Installed on the ACP agent's workspace as its [`ClientFileIo`]. Whether reads and writes are
//! bridged follows the capabilities the client advertised at `initialize`, read live from the
//! adapter, so a client that offers nothing keeps every file operation local. Each operation emits
//! `client_fs_request`, which the adapter turns into an agent-to-client JSON-RPC request, and
//! blocks until the client answers (relayed back as `_fs_result`), the request times out, or the
//! turn is aborted. Approvals, confinement and checkpoints run before this bridge is reached.

use super::Sender;
use super::host_tools::{HostBridge, run_call};
use crate::tools::ClientFileIo;
use anyhow::Result;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Pending client fs requests, completed by the controller from the client's responses.
#[derive(Default)]
pub(super) struct ClientFsCalls {
    bridge: HostBridge,
}

impl ClientFsCalls {
    pub(super) fn complete(&self, call_id: &str, output: String, is_error: bool) -> Result<()> {
        self.bridge.complete(call_id, output, is_error)
    }
}

pub(super) struct ClientFs {
    calls: Arc<ClientFsCalls>,
    out: Sender,
    interrupt: Arc<AtomicBool>,
}

impl ClientFs {
    pub(super) fn new(calls: Arc<ClientFsCalls>, out: Sender, interrupt: Arc<AtomicBool>) -> Self {
        Self {
            calls,
            out,
            interrupt,
        }
    }

    fn capabilities(&self) -> (bool, bool) {
        self.out
            .mapping()
            .ok()
            .and_then(|output| output.acp.as_ref().map(crate::acp::Adapter::client_fs))
            .unwrap_or((false, false))
    }

    fn call(&self, operation: &str, path: &Path, content: Option<&str>) -> Result<String, String> {
        // ACP paths are JSON strings; refuse a non-UTF-8 path instead of sending a lossy one.
        let path = path
            .to_str()
            .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?;
        let output = run_call(
            &self.calls.bridge,
            &self.interrupt,
            &format!("client fs/{operation}_text_file for {path}"),
            TIMEOUT,
            |call_id| {
                let mut event = json!({
                    "event":"client_fs_request",
                    "call_id":call_id,
                    "operation":operation,
                    "path":path,
                });
                if let Some(content) = content {
                    event["content"] = json!(content);
                }
                self.out.emit(event)
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

impl ClientFileIo for ClientFs {
    fn reads(&self) -> bool {
        self.capabilities().0
    }
    fn writes(&self) -> bool {
        self.capabilities().1
    }
    fn read_text(&self, path: &Path) -> Result<String, String> {
        self.call("read", path, None)
    }
    fn write_text(&self, path: &Path, content: &str) -> Result<(), String> {
        self.call("write", path, Some(content)).map(drop)
    }
}

#[cfg(test)]
mod tests;
