//! End-of-turn report: what changed, what was checked, what cannot be undone, and what can resume.
//! It renders only the `turn_summary` the agent saves at turn end, so it keeps no state of its own.

use serde_json::Value;

/// Changed files named before the report summarizes the rest.
const LISTED_FILES: usize = 8;

/// The report for a turn that edited files or did not complete; `None` for a plain answer or
/// when no summary was saved (for example, a turn that failed before it finished).
pub(super) fn render(summary: &Value) -> Option<String> {
    let outcome = summary["outcome"].as_str()?;
    let files: Vec<&str> = summary["changed_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let stopped = outcome != "completed";
    if files.is_empty() && !stopped {
        return None;
    }
    // The full cause is printed just above the report; the class says what kind of failure it was.
    let mut lines = vec![match summary["error_class"].as_str() {
        Some(class) => format!("Outcome: {outcome} ({})", class.replace('_', " ")),
        None => format!("Outcome: {outcome}"),
    }];
    lines.push(if files.is_empty() {
        "Changed: no files".to_owned()
    } else {
        format!("Changed: {}", changed_line(&files))
    });
    lines.push(checks_line(summary));
    let open = summary["open_tasks"].as_u64().unwrap_or(0);
    if open > 0 {
        lines.push(format!("Open tasks: {open} not finished"));
    }
    if !files.is_empty() {
        lines.push(
            "Undo: /undo can revert checkpointed edits; /review-file shows their diffs.".to_owned(),
        );
    }
    let shell = summary["uncheckpointed_shell_commands"]
        .as_u64()
        .unwrap_or(0);
    let extension = summary["uncheckpointed_extension_calls"]
        .as_u64()
        .unwrap_or(0);
    lines.push(if shell + extension > 0 {
        format!(
            "Not reversible: {shell} shell command(s) and {extension} extension call(s) may have changed files outside checkpoints."
        )
    } else {
        "Not reversible: no shell or extension effects recorded.".to_owned()
    });
    if stopped {
        lines.push(
            "Resume: send your next message; the session and its open tasks are kept.".to_owned(),
        );
    }
    Some(lines.join("\n"))
}

fn changed_line(files: &[&str]) -> String {
    let shown = files
        .iter()
        .take(LISTED_FILES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    match files.len().saturating_sub(LISTED_FILES) {
        0 => format!("{} file(s): {shown}", files.len()),
        rest => format!("{} file(s): {shown} and {rest} more", files.len()),
    }
}

fn checks_line(summary: &Value) -> String {
    let checks: Vec<String> = summary["recorded_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| {
            format!(
                "{} {}",
                check["name"].as_str().unwrap_or("check"),
                check["status"].as_str().unwrap_or("unverified")
            )
        })
        .collect();
    if checks.is_empty() {
        return "Checks: none run".to_owned();
    }
    let current = if summary["checks_current"].as_bool() == Some(true) {
        ""
    } else {
        "; not current with the final edits"
    };
    format!("Checks: {}{current}", checks.join(", "))
}

#[cfg(test)]
mod tests;
