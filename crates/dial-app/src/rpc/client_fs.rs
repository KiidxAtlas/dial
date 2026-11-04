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
use crate::runtime::tool_output::ToolOutput;
use crate::tools::{ClientFileIo, ClientWriteError};
use anyhow::Result;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Pending client fs requests, completed by the controller from the client's responses.
#[derive(Default)]
pub(super) struct ClientFsCalls {
    bridge: HostBridge,
    writes: Mutex<BTreeMap<PathBuf, PendingWrite>>,
}

struct PendingWrite {
    call_id: String,
    acknowledged: bool,
}

impl ClientFsCalls {
    pub(super) fn complete(&self, call_id: &str, output: String, is_error: bool) -> Result<()> {
        let late_write = {
            let mut writes = self
                .writes
                .lock()
                .map_err(|_| anyhow::anyhow!("client write state unavailable"))?;
            let pending = writes
                .values_mut()
                .find(|pending| pending.call_id == call_id && !pending.acknowledged);
            if let Some(pending) = pending {
                pending.acknowledged = true;
                true
            } else {
                false
            }
        };
        match self.bridge.complete(call_id, output, is_error) {
            Ok(()) => Ok(()),
            Err(_) if late_write => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn write_call(
        &self,
        path: &Path,
        interrupt: &AtomicBool,
        timeout: Duration,
        deliver: impl FnOnce(&str) -> bool,
        gone: impl Fn() -> bool,
    ) -> ToolOutput {
        let mut reserved = None;
        let mut refused = None;
        let output = run_call(
            &self.bridge,
            interrupt,
            "client fs/write_text_file",
            timeout,
            |id| {
                let reservation = (|| -> Result<(), String> {
                    let mut writes = self
                        .writes
                        .lock()
                        .map_err(|_| "client write state unavailable".to_owned())?;
                    if let Some(pending) = writes.get(path) {
                        return Err(format!(
                            "request {} has unresolved write effects; wait for its acknowledgement, then re-read this file before retrying",
                            pending.call_id
                        ));
                    }
                    if writes.len() >= 128 {
                        return Err("client write reconciliation capacity reached; resolve earlier writes before retrying".into());
                    }
                    writes.insert(
                        path.to_owned(),
                        PendingWrite {
                            call_id: id.into(),
                            acknowledged: false,
                        },
                    );
                    Ok(())
                })();
                if let Err(error) = reservation {
                    refused = Some(error);
                    return false;
                }
                reserved = Some(id.to_owned());
                deliver(id)
            },
            gone,
        );
        if let Some(error) = refused {
            // This attempt did not run, but the earlier write is still uncertain.
            return ToolOutput::err(error)
                .with_effects(crate::runtime::execution::Effects::Unknown);
        }
        if (!output.is_error
            || output.execution.effects == crate::runtime::execution::Effects::None)
            && let Some(id) = reserved
        {
            self.reconcile(path, &id);
        }
        output
    }

    fn acknowledged(&self, path: &Path) -> Option<String> {
        self.writes
            .lock()
            .ok()?
            .get(path)
            .filter(|pending| pending.acknowledged)
            .map(|pending| pending.call_id.clone())
    }

    fn reconcile(&self, path: &Path, id: &str) {
        if let Ok(mut writes) = self.writes.lock()
            && writes
                .get(path)
                .is_some_and(|pending| pending.call_id == id)
        {
            writes.remove(path);
        }
    }

    fn read_call(
        &self,
        path: &Path,
        interrupt: &AtomicBool,
        timeout: Duration,
        deliver: impl FnOnce(&str) -> bool,
        gone: impl Fn() -> bool,
    ) -> ToolOutput {
        // A read racing an unfinished write must never release its retry barrier.
        let acknowledged = self.acknowledged(path);
        let output = run_call(
            &self.bridge,
            interrupt,
            "client fs/read_text_file",
            timeout,
            deliver,
            gone,
        );
        if !output.is_error
            && let Some(id) = acknowledged
        {
            self.reconcile(path, &id);
        }
        output
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

    fn call(&self, operation: &str, path: &Path, content: Option<&str>) -> ToolOutput {
        let Some(shown) = path.to_str() else {
            return ToolOutput::err(format!("{} is not valid UTF-8", path.display()))
                .with_effects(crate::runtime::execution::Effects::None);
        };
        let deliver = |call_id: &str| {
            let mut event = json!({
                "event":"client_fs_request", "call_id":call_id,
                "operation":operation, "path":shown,
            });
            if let Some(content) = content {
                event["content"] = json!(content);
            }
            self.out.emit(event)
        };
        if content.is_some() {
            self.calls
                .write_call(path, &self.interrupt, TIMEOUT, deliver, || {
                    self.out.unavailable()
                })
        } else {
            self.calls
                .read_call(path, &self.interrupt, TIMEOUT, deliver, || {
                    self.out.unavailable()
                })
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
        let output = self.call("read", path, None);
        if output.is_error {
            Err(output.text)
        } else {
            Ok(output.text)
        }
    }
    fn write_text(&self, path: &Path, content: &str) -> Result<(), String> {
        self.write_text_confirmed(path, content)
            .map_err(|error| error.to_string())
    }
    fn write_text_confirmed(&self, path: &Path, content: &str) -> Result<(), ClientWriteError> {
        let output = self.call("write", path, Some(content));
        if !output.is_error {
            Ok(())
        } else if output.execution.effects == crate::runtime::execution::Effects::None {
            Err(ClientWriteError::NotWritten(output.text))
        } else {
            Err(ClientWriteError::Unknown(output.text))
        }
    }
}

#[cfg(test)]
mod tests;
