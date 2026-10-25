//! Execution outcomes are independent of presentation hooks.
use crate::runtime::Phase;
pub use crate::runtime::execution::{Effects, Execution, ToolStatus};
use crate::tools::ToolOutput;
use serde_json::Value;

pub fn normalize(phase: Phase, output: &ToolOutput) -> Phase {
    if effects(output) == Effects::Unknown {
        return Phase::Incomplete;
    }
    match output.execution.status {
        Some(ToolStatus::Error) => return Phase::Failed,
        Some(ToolStatus::Aborted) => return Phase::Interrupted,
        Some(ToolStatus::Blocked) => return Phase::Blocked,
        Some(ToolStatus::Timeout | ToolStatus::Skipped) => return Phase::Incomplete,
        _ => {}
    }
    match phase {
        Phase::Running => Phase::Incomplete,
        Phase::Completed if output.is_error => Phase::Failed,
        terminal => terminal,
    }
}

pub fn result_phase(name: &str, args: &Value, output: &ToolOutput) -> Phase {
    if effects(output) == Effects::Unknown {
        return Phase::Incomplete;
    }
    let fallback = normalize(
        if output.is_error {
            Phase::Failed
        } else {
            Phase::Completed
        },
        output,
    );
    // Observing, starting or controlling a worker is a completed operation of its own;
    // the worker's independent body retains its work outcome.
    if name != "task"
        || matches!(
            args["action"].as_str(),
            Some("start" | "status" | "wait" | "cancel")
        )
    {
        return fallback;
    }
    let jobs = output
        .data
        .as_ref()
        .and_then(|data| data.as_array().or_else(|| data["jobs"].as_array()));
    if let Some(jobs) = jobs {
        let phases = jobs
            .iter()
            .map(|job| Phase::from_status(job["status"].as_str().unwrap_or("failed")))
            .collect::<Vec<_>>();
        if !phases.contains(&Phase::Running) {
            return [
                Phase::Failed,
                Phase::Incomplete,
                Phase::Interrupted,
                Phase::Blocked,
                Phase::NeedsInput,
            ]
            .into_iter()
            .find(|phase| phases.contains(phase))
            .unwrap_or(fallback);
        }
    }
    fallback
}

pub fn effects(output: &ToolOutput) -> Effects {
    output.execution.effects
}

/// Persist the same typed outcome used by presentation and telemetry in the model history.
pub fn annotate(message: &mut Value, output: &ToolOutput) {
    message["dial_execution_status"] = serde_json::json!(ToolStatus::from_output(output));
    message["dial_effects"] = serde_json::json!(effects(output));
    message["dial_execution"] = serde_json::json!(output.execution);
}

pub fn message(id: &str, output: &ToolOutput) -> Value {
    let mut message = crate::llm::tool_message(id, &output.text);
    annotate(&mut message, output);
    message
}

pub fn skipped(id: &str, reason: &str) -> Value {
    message(
        id,
        &ToolOutput::err(reason)
            .with_status(ToolStatus::Skipped)
            .with_effects(Effects::None),
    )
}

#[cfg(test)]
mod tests;
