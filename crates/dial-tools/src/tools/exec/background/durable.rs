//! Explicit, finite-lifetime pipe jobs owned by a separate process, never a replay queue.
//!
//! The launch contract travels once over stdin. Only identity and bounded observations survive
//! on disk. Local control authority is the current user's private socket directory.
use crate::tools::{ToolOutput, Workspace};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

/// Private re-exec entry point; it is not a user-facing command.
pub const OWNER_ARG: &str = "--dial-background-owner";

#[cfg(unix)]
mod unix;

pub(super) fn dispatch(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    interrupt: &AtomicBool,
) -> Result<ToolOutput, String> {
    #[cfg(unix)]
    {
        unix::dispatch(ws, args, cancel, interrupt)
            .map_err(|error| format!("durable bg: {error:#}"))
    }
    #[cfg(not(unix))]
    {
        let _ = (ws, args, cancel, interrupt);
        Err(
            "durable bg is currently supported only on Unix; ordinary session jobs are unchanged"
                .into(),
        )
    }
}

/// Run the private owner. Invalid or missing launch input fails closed, without starting a job.
pub fn owner_main() -> i32 {
    #[cfg(unix)]
    {
        match unix::owner_main() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("background owner: {error:#}");
                1
            }
        }
    }
    #[cfg(not(unix))]
    {
        1
    }
}
