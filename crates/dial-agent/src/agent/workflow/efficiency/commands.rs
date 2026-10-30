//! Lossless-for-diagnostics filtering of recognized successful command chatter.
use crate::llm::ToolCall;
use crate::runtime::execution::{Completeness, Effects, ToolStatus};
use crate::runtime::tool_output::ToolOutput;
use std::sync::LazyLock;

/// Smaller outputs are already cheap; avoid adding an evidence header to them.
const MIN_COMMAND_BYTES: usize = 4096;

/// Remove only recognized progress, never infer success from printed prose.
/// `artifact` must identify the caller-retained complete result. Partial presentations cannot be
/// filtered here because this helper has no artifact store with which to recover missing lines.
pub(in crate::agent) fn compact_command(
    call: &ToolCall,
    output: &ToolOutput,
    artifact: Option<&str>,
) -> Option<String> {
    let artifact = artifact.filter(|uri| !uri.trim().is_empty())?;
    let process = output.execution.process.as_ref()?;
    if !matches!(call.name.as_str(), "bash" | "bg")
        || output.text.len() <= MIN_COMMAND_BYTES
        || output.is_error
        || ToolStatus::from_output(output) != ToolStatus::Ok
        || output.execution.effects == Effects::Unknown
        || !process.started
        || process.exit_code != Some(0)
        || process.signal.is_some()
        || process.timed_out
        || process.cancelled
        || !process.output_complete
        || output.execution.output.as_ref().is_some_and(|integrity| {
            integrity.capture != Completeness::Complete
                || integrity.presentation != Completeness::Complete
                || integrity.retention == Completeness::Partial
        })
    {
        return None;
    }

    // Match only ordinary Cargo/rust-test lines, not arbitrary successful command output.
    static NOISE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"^(?: {3,}(?:Compiling|Checking) [A-Za-z0-9_-]+ v[^\s]+(?: \([^\r\n]*\))?",
            r"| {3,}Finished `[^`]+` profile [^\r\n]* target\(s\) in [0-9.]+[smh]",
            r"|test [A-Za-z0-9_:]+ \.\.\. ok)$"
        ))
        .expect("constant command noise regex")
    });
    static DIAGNOSTIC: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:warning|error|panic|failed|failure)\b|^\s*(?:-->|\|)")
            .expect("constant diagnostic regex")
    });
    // ANSI-bearing lines remain byte-for-byte intact: terminal controls can splice diagnostics
    // into apparently routine lines, so do not strip escapes just to broaden recognition.
    let mut preserved = String::new();
    let mut removed = 0;
    let mut diagnostic = false;
    for line in output.text.split_inclusive('\n') {
        let plain = line.trim_end_matches(['\r', '\n']);
        diagnostic |= DIAGNOSTIC.is_match(plain);
        if !diagnostic && NOISE.is_match(plain) {
            removed += 1;
        } else {
            preserved.push_str(line);
        }
    }
    if removed == 0 {
        return None;
    }
    let compact = format!(
        "[Command exited 0; omitted {removed} recognized progress/test-success lines]\n{preserved}\n[Complete tool evidence: {artifact}; retrieve with artifact_read]"
    );
    (compact.len() < output.text.len()).then_some(compact)
}

#[cfg(test)]
mod tests;
