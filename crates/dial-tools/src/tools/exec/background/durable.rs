//! Explicit, finite-lifetime pipe jobs owned by a separate process, never a replay queue.
//!
//! The launch contract travels once over stdin. Only identity and bounded observations survive
//! on disk. Local control authority is the current user's private socket directory.
use crate::tools::{ToolOutput, Workspace};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

/// Private re-exec entry point; it is not a user-facing command.
pub const OWNER_ARG: &str = "--dial-background-owner";

#[cfg(any(unix, windows))]
mod owner;

/// Metadata only: discovery never contacts an owner, reads output, or acquires control.
#[derive(serde::Serialize)]
pub struct Discovery {
    pub name: String,
    pub status: &'static str,
    pub lifetime_secs: Option<u64>,
}

pub fn discover(home: &std::path::Path, root: &std::path::Path) -> anyhow::Result<Vec<Discovery>> {
    #[cfg(any(unix, windows))]
    {
        owner::discover(home, root)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (home, root);
        Ok(Vec::new())
    }
}

pub(super) fn dispatch(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    interrupt: &AtomicBool,
) -> Result<ToolOutput, String> {
    #[cfg(any(unix, windows))]
    {
        owner::dispatch(ws, args, cancel, interrupt)
            .map_err(|error| format!("durable bg: {error:#}"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (ws, args, cancel, interrupt);
        Err(
            "durable bg is supported only on Unix and Windows; ordinary session jobs are unchanged"
                .into(),
        )
    }
}

/// Run the private owner. Invalid or missing launch input fails closed, without starting a job.
pub fn owner_main() -> i32 {
    #[cfg(any(unix, windows))]
    {
        match owner::owner_main() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("background owner: {error:#}");
                1
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        1
    }
}
