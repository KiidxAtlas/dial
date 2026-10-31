//! Safe next actions from observed failures. No retries, installation or permission grants.
use crate::agent::prompts::recovery as policy;
pub(in crate::agent) use crate::agent::prompts::recovery::{
    DIAGNOSE_FIRST, FIRST_FAILURE, PROVIDER_RECOVERY,
};
use crate::{
    llm::ToolCall,
    runtime::{
        execution::{Effects, Retry, ToolStatus},
        tool_output::ToolOutput,
    },
};

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
    let Some(recovery) = output.recovery() else {
        return format!("\n{}", feedback(kind, advice, consecutive));
    };
    let changed = match recovery.committed_paths {
        0 => "No committed file paths recorded (not proof that nothing changed).".to_owned(),
        n => format!(
            "Committed file paths recorded: {n}; preserve these changes while resolving the failure."
        ),
    };
    let retry = match recovery.retry {
        Retry::AfterCorrection if recovery.committed_paths > 0 => {
            "Committed work is preserved: inspect it and correct the named cause; do not repeat the whole operation."
        }
        Retry::AfterCorrection => "Retry only after correcting the named cause.",
        Retry::AfterInspection => "Do not retry until the recorded state or process is inspected.",
        Retry::OnlyByDirection => {
            "Do not replay automatically; resume only on the user's direction."
        }
        Retry::Withheld => "A boundary withholds this operation; do not retry or bypass it.",
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
        "\nRecovery state: tool `{}`; status {}. {effects} {changed}\nRetry: {retry}\nRetained: {retained}\nNext action:{}",
        call.name,
        recovery.status.as_str(),
        feedback(kind, advice, consecutive),
    )
}

pub(in crate::agent) fn provider_failure(error: &anyhow::Error) -> String {
    format!("{error:#}\n{PROVIDER_RECOVERY}")
}

pub(in crate::agent) fn classify(
    call: &ToolCall,
    output: &ToolOutput,
) -> Option<(&'static str, &'static str)> {
    if output.execution.effects == Effects::Unknown {
        return Some(("effects_unknown", policy::EFFECTS_UNKNOWN));
    }
    match ToolStatus::from_output(output) {
        ToolStatus::Blocked => {
            return Some(("permission_or_policy", policy::PERMISSION_OR_POLICY));
        }
        ToolStatus::Aborted => {
            return Some(("interrupted", policy::INTERRUPTED));
        }
        ToolStatus::Timeout => {
            return Some(("timeout", policy::TIMEOUT));
        }
        ToolStatus::Skipped => {
            return Some(("not_executed", policy::NOT_EXECUTED));
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
            CheckStatus::EnvironmentUnavailable => {
                Some(("verification_environment", policy::VERIFICATION_ENVIRONMENT))
            }
            CheckStatus::CodeFailed if check.is_some() => {
                Some(("verification_code", policy::VERIFICATION_CODE))
            }
            CheckStatus::CodeFailed => Some((
                "verification_failed_unclassified",
                policy::VERIFICATION_FAILED_UNCLASSIFIED,
            )),
            _ => Some(("verification_unavailable", policy::VERIFICATION_UNAVAILABLE)),
        };
    }
    if matches!(call.name.as_str(), "edit" | "ast_edit" | "write") {
        return Some(("source_or_call_contract", policy::SOURCE_OR_CALL_CONTRACT));
    }
    Some(("tool_failure", policy::TOOL_FAILURE))
}

/// Coach a known tool invoked as a shell executable, using both the original call and a
/// completed process failure. This is advice only: catalog membership grants no permission.
pub(in crate::agent) fn shell_tool_mistake(
    call: &ToolCall,
    output: &ToolOutput,
    known: impl Fn(&str) -> bool,
) -> Option<String> {
    if call.name != "bash"
        || ToolStatus::from_output(output) != ToolStatus::Error
        || output.execution.effects == Effects::Unknown
    {
        return None;
    }
    let process = output.execution.process.as_ref()?;
    if !process.started
        || process.exit_code != Some(127)
        || process.signal.is_some()
        || process.timed_out
        || process.cancelled
        || !process.output_complete
    {
        return None;
    }
    let words = crate::agent::safety::approval::shell_words(call.arguments["command"].as_str()?)?;
    let name = words.first()?;
    if !known(name) {
        return None;
    }
    let missing = format!("{name}: command not found");
    let reverse = format!("command not found: {name}");
    if !output.text.lines().any(|line| {
        let line = line.trim();
        line == missing || line.ends_with(&format!(": {missing}")) || line.ends_with(&reverse)
    }) {
        return None;
    }
    Some(policy::shell_tool_mistake(name))
}

/// An empty eligible search is not proof that every capability is absent. Keep the raw
/// search result and offer a bounded next step, without activating fallback tools.
pub(in crate::agent) fn empty_discovery(
    call: &ToolCall,
    output: &ToolOutput,
) -> Option<&'static str> {
    (call.name == "tool_search"
        && ToolStatus::from_output(output) == ToolStatus::Ok
        && output.execution.effects != Effects::Unknown
        && output.text.trim() == "[]")
        .then_some(policy::EMPTY_DISCOVERY)
}

#[cfg(test)]
mod tests;
