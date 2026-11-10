//! Why an errored tool call failed, as a fixed label for the stats ledger.
//!
//! Labels come from typed result fields only (the edit outcome, the check counts, the process
//! outcome). Diagnostic prose is never parsed, and no payload, path or argument is retained.
use crate::runtime::{
    execution::{Effects, ToolStatus},
    tool_output::ToolOutput,
};

/// A label for a call whose status is [`ToolStatus::Error`], or `None` for every other status:
/// blocked, skipped, timed-out and aborted calls are already told apart by the status itself.
///
/// `check_failed` is a check that ran and reported failures, which is the tool working; every
/// other label is a call that did not do what was asked.
pub fn error_class(tool: &str, status: ToolStatus, output: &ToolOutput) -> Option<&'static str> {
    if status != ToolStatus::Error {
        return None;
    }
    if output.execution.effects == Effects::Unknown {
        return Some("effects_unknown");
    }
    let data = output.data.as_ref();
    match tool {
        "edit" | "ast_edit" => {
            return Some(
                match data.and_then(|data| data["edit"]["outcome"].as_str()) {
                    Some("source_changed") => "edit_source_changed",
                    Some("target_not_found") => "edit_target_not_found",
                    _ => "edit_refused",
                },
            );
        }
        "verify" => {
            let failed = data.and_then(|data| data["counts"]["failed"].as_u64());
            return Some(if failed.is_some_and(|failed| failed > 0) {
                "check_failed"
            } else {
                "verify_refused"
            });
        }
        _ => {}
    }
    if output
        .execution
        .integration
        .as_ref()
        .is_some_and(|integration| integration.failure.is_some())
    {
        return Some("integration");
    }
    if let Some(process) = &output.execution.process {
        return Some(if !process.started {
            "not_started"
        } else if process.signal.is_some() {
            "signalled"
        } else if process.exit_code == Some(127) {
            // POSIX shells reserve 127 and 126 for a program that could not be found or run.
            "command_not_found"
        } else if process.exit_code == Some(126) {
            "not_executable"
        } else if process.exit_code.is_some_and(|code| code != 0) {
            "nonzero_exit"
        } else {
            "tool_error"
        });
    }
    Some("tool_error")
}

#[cfg(test)]
mod tests;
