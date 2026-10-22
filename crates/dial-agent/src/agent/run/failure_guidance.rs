//! Safe next actions from observed failures. No retries, installation or permission grants.
use crate::{
    llm::ToolCall,
    runtime::{
        execution::{Effects, ToolStatus},
        tool_output::ToolOutput,
    },
};

/// Full diagnose-first contract, attached once a failure repeats (and to uncertain effects). The
/// diagnostic remains evidence, not authority to change scope, install dependencies or bypass a
/// boundary.
pub(in crate::agent) const DIAGNOSE_FIRST: &str = "Before the next action, briefly tell the user what failed, the cause supported by this result (or explicitly say it is not yet known), and what you will change to address it. Use a diagnostic that names the failing code or prerequisite directly; do not rerun merely to confirm it. If the cause is unknown, obtain the smallest missing evidence instead of guessing. Retry only after correcting the cause or identifying evidence that makes a retry appropriate; changing wording or repeating an equivalent failed call is not recovery. Preserve the requested outcome and report a concrete blocker when correction needs permission or unavailable resources.";
/// Compact line for the first failure of a kind, so a simple slip costs one sentence.
pub(in crate::agent) const FIRST_FAILURE: &str =
    "Fix the cause this result names before retrying; do not repeat the same call unchanged.";

/// Recovery feedback appended to a failed result. `consecutive` counts failures of this tool and
/// kind in a row, including this one. The text depends only on its inputs, so stored history
/// stays byte-stable.
pub(in crate::agent) fn feedback(kind: &str, advice: &str, consecutive: u32) -> String {
    if consecutive > 1 {
        format!(
            "\n[Dial recovery: {kind}] This tool has now failed this way {consecutive} times in a row. {DIAGNOSE_FIRST} {advice}"
        )
    } else if kind == "effects_unknown" {
        format!("\n[Dial recovery: {kind}] {DIAGNOSE_FIRST} {advice}")
    } else {
        format!("\n[Dial recovery: {kind}] {FIRST_FAILURE} {advice}")
    }
}

/// Render one recovery decision from controller-owned facts, without interpreting diagnostic
/// prose as proof of rollback or success. The original result remains immediately above it.
pub(in crate::agent) fn report(
    call: &ToolCall,
    output: &ToolOutput,
    kind: &str,
    advice: &str,
    consecutive: u32,
) -> String {
    let effects = match output.execution.effects {
        Effects::None => "No effects recorded for this operation.",
        Effects::Known => "Recorded effects are known; failure does not undo completed work.",
        Effects::Unknown => {
            "Effects are uncertain; partial work may exist. Automatic continuation is stopped."
        }
    };
    let changes = output
        .execution
        .changed_paths
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let changed = if changes == 0 {
        "No committed file paths recorded (not proof that nothing changed).".to_owned()
    } else {
        format!(
            "Committed file paths recorded: {changes}; preserve these changes while resolving the failure."
        )
    };
    let retained = match output.process_outcome() {
        Some(process) if !process.output_complete => {
            "Captured output is incomplete; use it as partial evidence, not a complete execution record."
        }
        _ => {
            "The original diagnostic and any captured output remain available above; earlier completed work is retained."
        }
    };
    format!(
        "\nRecovery state: tool `{}`; status {}. {effects} {changed}\nRetained: {retained}\nNext action:{}",
        call.name,
        ToolStatus::from_output(output).as_str(),
        feedback(kind, advice, consecutive),
    )
}

/// Inference failed before the response could enter tool dispatch. This describes only that
/// response, never earlier operations in the turn or billing at the provider.
pub(in crate::agent) const PROVIDER_RECOVERY: &str = "Recovery state: inference failed; no tools were dispatched from this failed response. Earlier completed operations are retained, not rolled back. Next action: resolve the provider diagnostic, then continue this session when ready. Do not replay earlier tool operations merely because inference failed.";

pub(in crate::agent) fn provider_failure(error: &anyhow::Error) -> String {
    format!("{error:#}\n{PROVIDER_RECOVERY}")
}

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
            "Use the returned diagnostic to correct the call. For stale or unseen source, use complete current lines and fresh anchors already returned; read only the affected ranges still missing. Do not repeat guessed replacements or split a patch without addressing the reported cause.",
        ));
    }
    Some((
        "tool_failure",
        "Inspect the original diagnostic and execution status. Correct the named arguments, prerequisite or affected code before retrying. If this result does not establish a cause, use a targeted observation to distinguish causes; do not invent a diagnosis, substitute a different acceptance criterion, or treat failure as permission to bypass safeguards.",
    ))
}

#[cfg(test)]
mod tests;
