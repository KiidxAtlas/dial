//! Safe next actions from observed failures. No retries, installation or permission grants.
use crate::agent::prompts::recovery as policy;
pub(in crate::agent) use crate::agent::prompts::recovery::{FIRST_FAILURE, PROVIDER_RECOVERY};
use crate::{
    llm::ToolCall,
    runtime::{
        execution::{Effects, Retry, ToolStatus},
        tool_output::ToolOutput,
    },
};

mod launch;
pub(in crate::agent) use launch::{Capability, LaunchCapabilities, shell_launch_recovery};

use serde::Serialize;
use serde_json::{Value, json};

/// Diagnostic suggestions only. Dispatch, permissions and retry authority never read this data.
#[derive(Debug, Serialize)]
struct Advisory<'a> {
    advisory_only: bool,
    category: &'a str,
    source_call_id: &'a str,
    evidence_ref: &'static str,
    required_observation: &'a str,
    candidates: Vec<Candidate>,
}

#[derive(Debug, Serialize)]
struct Candidate {
    tool: String,
    availability: Capability,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<Value>,
}

impl Candidate {
    fn new(tool: &str, availability: Capability) -> Self {
        Self {
            tool: tool.into(),
            availability,
            arguments: None,
        }
    }

    /// Exact native arguments only; an oversized or invalid proposal is omitted, never repaired
    /// by guessing. Shell tool mistakes deliberately carry no reconstructed call arguments.
    fn native(tool: &str, availability: Capability, arguments: Value) -> Self {
        let schema = match tool {
            "bash" => crate::tools::schemas(&["bash"], crate::config::types::EditMode::default())
                .ok()
                .and_then(|mut schemas| schemas.pop()),
            "bg" => crate::tools::exec::background::tools(&crate::config::types::Config::default())
                .into_iter()
                .find(|tool| tool.name() == "bg")
                .map(|tool| tool.schema()),
            _ => None,
        };
        let valid = availability.usable()
            && arguments.to_string().len() <= 8192
            && schema.as_ref().is_some_and(|schema| {
                crate::tools::surface::catalog::validate_arguments(schema, &arguments).is_ok()
            });
        Self {
            tool: tool.into(),
            availability,
            arguments: valid.then_some(arguments),
        }
    }

    fn load_guidance(&self) -> String {
        if self.availability == Capability::Loadable {
            format!(
                "{} is eligible but not offered: use tool_load with {}, then wait for its schema before any permitted action. ",
                self.tool,
                json!({"names":[self.tool]})
            )
        } else {
            String::new()
        }
    }
}

impl<'a> Advisory<'a> {
    fn new(
        call: &'a ToolCall,
        category: &'a str,
        evidence_ref: &'static str,
        required_observation: &'a str,
    ) -> Self {
        Self {
            advisory_only: true,
            category,
            source_call_id: &call.id,
            evidence_ref,
            required_observation,
            candidates: Vec::new(),
        }
    }

    fn attach(&self, output: &mut ToolOutput) {
        // Each native record is bounded; producer values (including colliding keys) stay intact.
        // Existing array entries are opaque data, never treated as trusted recovery decisions.
        let bounded = |text: &str, max: usize| text.chars().take(max).collect::<String>();
        let record = json!({
            "advisory_only": self.advisory_only,
            "category": bounded(self.category, 128),
            "source_call_id": bounded(self.source_call_id, 256),
            "evidence_ref": self.evidence_ref,
            "required_observation": bounded(self.required_observation, 2048),
            "candidates": self.candidates.iter().take(3).collect::<Vec<_>>(),
        });
        let data = output.data.get_or_insert_with(|| json!({}));
        if !data.is_object() {
            *data = json!({"producer_data": std::mem::take(data)});
        }
        if let Some(object) = data.as_object_mut() {
            use serde_json::map::Entry;
            match object.entry("recovery_advisory") {
                Entry::Vacant(entry) => {
                    entry.insert(json!([record]));
                }
                Entry::Occupied(mut entry) => match entry.get_mut() {
                    Value::Array(records) => records.push(record),
                    old => {
                        let original = std::mem::take(old);
                        *old = json!([original, record]);
                    }
                },
            }
        }
    }
}

/// Failed edits and writes carry one cause-specific line instead of the general retry policy;
/// the general re-read guidance lives in the editing tool's own description.
const SOURCE_KIND: &str = "source_or_call_contract";

/// The advice sentence(s) for one failure. `consecutive` counts failures of this tool and kind in
/// a row, including this one.
fn advice_body(kind: &str, advice: &str, consecutive: u32) -> String {
    let mut parts = Vec::with_capacity(3);
    if kind != SOURCE_KIND {
        parts.push(FIRST_FAILURE.to_owned());
    }
    if consecutive > 1 {
        parts.push(format!("Failed {consecutive} times in a row."));
    }
    parts.push(advice.to_owned());
    parts.join(" ")
}

/// One recovery notice appended to a failed result, in the shared harness envelope. Single-line
/// bodies stay on one line to keep stored history small.
fn recovery_notice(kind: &str, body: &str) -> String {
    if body.contains('\n') {
        format!("\n{}", policy::notice(kind, body))
    } else {
        format!("\n<dial-notice kind=\"{kind}\">{body}</dial-notice>")
    }
}

/// Recovery feedback appended to a failed result. The text depends only on its inputs, so stored
/// history stays byte-stable.
pub(in crate::agent) fn feedback(kind: &str, advice: &str, consecutive: u32) -> String {
    recovery_notice(kind, &advice_body(kind, advice, consecutive))
}

/// Render one recovery decision from controller-owned facts, without interpreting diagnostic
/// prose as proof of rollback or success. The original result remains immediately above it.
pub(in crate::agent) fn report(
    call: &ToolCall,
    output: &mut ToolOutput,
    kind: &str,
    advice: &str,
    consecutive: u32,
) -> String {
    Advisory::new(call, kind, "execution.recovery", advice).attach(output);
    let effects = match output.execution.effects {
        Effects::None => "No effects recorded for this operation.",
        Effects::Known => "Recorded effects are known; failure does not undo completed work.",
        Effects::Unknown => {
            "Effects are uncertain; partial work may exist. Automatic continuation is stopped."
        }
    };
    let Some(recovery) = output.recovery() else {
        return feedback(kind, advice, consecutive);
    };
    // An ordinary nonzero shell exit (missing program, failed build step) whose process ran to
    // completion is routine: the exit code and output above already say what happened. The full
    // recovery block cost about 900 bytes of history on every such failure in benchmark runs, so
    // the first one gets a compact line; repeats, uncertain effects, commits, partial capture and
    // other kinds keep the full report.
    let routine_exit = kind == "tool_failure"
        && call.name == "bash"
        && consecutive == 1
        && output.execution.effects != Effects::Unknown
        && recovery.committed_paths == 0
        && recovery.retry == Retry::AfterCorrection
        && output.process_outcome().is_some_and(|p| {
            p.started && !p.timed_out && !p.cancelled && p.signal.is_none() && p.output_complete
        });
    if routine_exit {
        return recovery_notice(
            kind,
            "Command failed; no committed file paths recorded (not proof that nothing changed). Correct the named cause before retrying; a workaround must still deliver the requested final state.",
        );
    }
    // Any other first failure with known effects, no committed work, complete capture and a plain
    // correct-then-retry decision gets the same one line: the diagnostic above plus the
    // cause-specific advice. The multi-line state block is kept for cases where it adds facts.
    let clean_first = consecutive == 1
        && output.execution.effects != Effects::Unknown
        && recovery.committed_paths == 0
        && recovery.retry == Retry::AfterCorrection
        && output.process_outcome().is_none_or(|p| {
            p.started && !p.timed_out && !p.cancelled && p.signal.is_none() && p.output_complete
        });
    if clean_first {
        let body = advice_body(kind, advice, 1);
        return recovery_notice(
            kind,
            &format!("{body} No committed file paths recorded (not proof that nothing changed)."),
        );
    }
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
        Retry::Withheld if kind == CALL_CONTRACT.0 => {
            "Fix the arguments and retry; no permission boundary withholds this call."
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
    recovery_notice(
        kind,
        &format!(
            "Recovery state: tool `{}`; status {}. {effects} {changed}\nRetry: {retry}\nRetained: {retained}\nNext action: {}",
            call.name,
            recovery.status.as_str(),
            advice_body(kind, advice, consecutive),
        ),
    )
}

pub(in crate::agent) fn provider_failure(error: &anyhow::Error) -> String {
    format!("{error:#}\n{PROVIDER_RECOVERY}")
}

/// Classification for a call the controller refused against the offered contract before
/// execution. The controller supplies this fact; diagnostic prose never selects it.
pub(in crate::agent) const CALL_CONTRACT: (&str, &str) = ("call_contract", policy::CALL_CONTRACT);

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
            CheckStatus::CodeFailed | CheckStatus::FailedUnknownCause => Some((
                "verification_failed_unclassified",
                policy::VERIFICATION_FAILED_UNCLASSIFIED,
            )),
            _ => Some(("verification_unavailable", policy::VERIFICATION_UNAVAILABLE)),
        };
    }
    if call.name == "write" {
        return Some((SOURCE_KIND, policy::WRITE_NOT_APPLIED));
    }
    if matches!(call.name.as_str(), "edit" | "ast_edit") {
        return Some((SOURCE_KIND, policy::EDIT_NOT_APPLIED));
    }
    Some(("tool_failure", policy::TOOL_FAILURE))
}

/// Coach a known tool invoked as a shell executable, using both the original call and a
/// completed process failure: exit 127 "command not found", or a Dial tool name that bash
/// resolves to a job-control builtin (`bg: no job control`). This is advice only: catalog
/// membership grants no permission.
fn shell_tool_mistake(
    call: &ToolCall,
    output: &ToolOutput,
    known: impl Fn(&str) -> bool,
) -> Option<String> {
    /// Dial tool names that bash runs as a builtin failing without job control.
    const SHADOWED_BUILTINS: &[&str] = &["bg"];
    if call.name != "bash"
        || ToolStatus::from_output(output) != ToolStatus::Error
        || output.execution.effects == Effects::Unknown
    {
        return None;
    }
    let process = output.execution.process.as_ref()?;
    if !process.started
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
    let no_job_control = format!("{name}: no job control");
    let observed = |line: &str| match process.exit_code {
        Some(127) => {
            line == missing
                || line.ends_with(&format!(": {missing}"))
                || line.ends_with(&format!("command not found: {name}"))
        }
        Some(code) if code != 0 && SHADOWED_BUILTINS.contains(&name.as_str()) => {
            line == no_job_control || line.ends_with(&format!(": {no_job_control}"))
        }
        _ => false,
    };
    if !output.text.lines().any(|line| observed(line.trim())) {
        return None;
    }
    Some(name.to_owned())
}

/// Resolve shell-tool coaching against the actual root or fixed worker surface. Unknown names
/// are not Dial tools; loadability is supplied only by the existing exposure/readiness owners.
pub(in crate::agent) fn shell_tool_recovery(
    call: &ToolCall,
    output: &mut ToolOutput,
    state: impl Fn(&str) -> Option<Capability>,
) -> Option<String> {
    let name = shell_tool_mistake(call, output, |name| state(name).is_some())?;
    let candidate = Candidate::new(&name, state(&name)?);
    let next = match candidate.availability {
        Capability::Offered => "Call it directly with its supplied schema; do not translate shell words into guessed tool arguments.".to_owned(),
        Capability::Loadable => candidate.load_guidance(),
        Capability::Prohibited => "This tool is prohibited by the current surface. Report that limit; do not load it or switch tools to bypass the boundary.".to_owned(),
        Capability::Unavailable => "This tool is unavailable or has an unmet prerequisite. Inspect the named prerequisite or report the gap; this surface offers no usable loader for it.".to_owned(),
    };
    let mut advisory = Advisory::new(
        call,
        "tool_not_shell",
        "execution.process + diagnostic",
        &next,
    );
    advisory.candidates.push(candidate);
    advisory.attach(output);
    Some(policy::shell_tool_mistake(&name, &next))
}

/// A completed shell command whose program is missing (exit 127 "command not found").
/// Advice only: reuse recorded availability or batch one same-environment probe; result text
/// remains diagnostic data and grants no installation or permission.
pub(in crate::agent) fn missing_program_recovery(
    call: &ToolCall,
    output: &ToolOutput,
) -> Option<String> {
    if call.name != "bash"
        || ToolStatus::from_output(output) != ToolStatus::Error
        || output.execution.effects == Effects::Unknown
    {
        return None;
    }
    let process = output.execution.process.as_ref()?;
    if !process.started || process.exit_code != Some(127) || !process.output_complete {
        return None;
    }
    let program = output.text.lines().find_map(|line| {
        let line = line.trim();
        let name = match line.strip_suffix(": command not found") {
            Some(prefix) => prefix.rsplit(": ").next()?,
            None => line.rsplit_once("command not found: ")?.1,
        };
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+-/".contains(c));
        valid.then(|| name.to_owned())
    })?;
    Some(policy::missing_program(&program))
}

#[cfg(test)]
mod tests;
