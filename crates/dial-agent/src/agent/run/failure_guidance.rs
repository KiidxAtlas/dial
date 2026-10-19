//! Safe next actions from observed failures. No retries, installation or permission grants.
use crate::{
    llm::ToolCall,
    runtime::{
        execution::{Effects, ToolStatus},
        tool_output::ToolOutput,
    },
};

pub(in crate::agent) fn classify(
    call: &ToolCall,
    output: &ToolOutput,
) -> Option<(&'static str, &'static str)> {
    if output.execution.effects == Effects::Unknown {
        return Some((
            "effects_unknown",
            "Inspect affected state before any retry. Partial work may exist; do not infer rollback or replay the operation.",
        ));
    }
    match ToolStatus::from_output(output) {
        ToolStatus::Blocked => {
            return Some((
                "permission_or_policy",
                "Respect the reported boundary. Do not switch tools to bypass it; finish reachable work and report the exact missing permission or prerequisite.",
            ));
        }
        ToolStatus::Aborted => {
            return Some((
                "interrupted",
                "Preserve completed effects. Do not automatically replay the interrupted operation; follow the user's direction and inspect state if continuation is requested.",
            ));
        }
        ToolStatus::Timeout => {
            return Some((
                "timeout",
                "Inspect the recorded process/task state and completed output. Continue with a narrower bounded operation only when its effects are understood; a timeout is not a code diagnosis.",
            ));
        }
        ToolStatus::Skipped => {
            return Some((
                "not_executed",
                "Resolve the reported ordering, cancellation or prerequisite before making a new call. Do not report this operation as executed.",
            ));
        }
        ToolStatus::Ok => return None,
        ToolStatus::Error => {}
    }
    if call.name == "bash"
        && (call.id.starts_with("focused-check:")
            || call.arguments["command"]
                .as_str()
                .is_some_and(crate::agent::memory::selfplay::is_check_command))
    {
        use crate::agent::verify::verification::{CheckStatus, FocusedCheck, classify_output};
        let check = call
            .id
            .strip_prefix("focused-check:")
            .map(|name| FocusedCheck {
                name: name.to_owned(),
                command: call.arguments["command"].as_str().unwrap_or("").to_owned(),
                timeout_secs: 0,
            });
        return match classify_output(check.as_ref(), output, false) {
            CheckStatus::EnvironmentUnavailable => Some((
                "verification_environment",
                "Diagnose the named runner or environment prerequisite; do not edit application code to hide this failure or install dependencies without approval. Use an already available equivalent check or report the coverage gap.",
            )),
            CheckStatus::CodeFailed if check.is_some() => Some((
                "verification_code",
                "Use the observed failing assertion/compiler diagnostic to repair the affected behavior, then rerun that check after the final edit. Do not change the acceptance criterion to obtain a pass.",
            )),
            CheckStatus::CodeFailed => Some((
                "verification_failed_unclassified",
                "The process failed, but its runner has no controller-selected setup classification. Inspect the diagnostic to distinguish an environment prerequisite from a code defect before editing application code; preserve the failing acceptance criterion.",
            )),
            _ => Some((
                "verification_unavailable",
                "This result does not establish a code defect or a passing check. Inspect termination and runner evidence, then report or resolve the specific verification gap.",
            )),
        };
    }
    if matches!(call.name.as_str(), "edit" | "ast_edit" | "write") {
        return Some((
            "source_or_call_contract",
            "Use the returned diagnostic to correct the call. If it reports stale or unseen source, read only the affected current range and use its fresh anchors; do not repeat guessed replacements.",
        ));
    }
    None
}

#[cfg(test)]
mod tests;
