//! Print-mode structured output: `--output-schema` validation and `-o/--output-last-message` files.
//!
//! The schema check reuses the runtime validator that subagent `output_schema` uses, and the
//! correction budget is the same `task.max_schema_retries` setting. Nothing here calls a model:
//! [`enforce`] takes the correction turn as a closure so the policy is testable without a provider.

use crate::agent::window::output_schema;
use crate::agent::{TurnOutcome, TurnReport};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::path::Path;

/// Read and check a `--output-schema` file. Runs before any request, so a bad file fails the run
/// without contacting a model.
pub fn load_schema(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read --output-schema {}", path.display()))?;
    let schema: Value = serde_json::from_str(&raw)
        .with_context(|| format!("--output-schema {} is not valid JSON", path.display()))?;
    output_schema::check_schema(&schema)
        .map_err(|why| anyhow!("invalid --output-schema {}: {why}", path.display()))?;
    Ok(schema)
}

/// The final answer parsed (fenced JSON is accepted) and checked against `schema`.
pub fn structured_answer(text: &str, schema: &Value) -> Result<Value, String> {
    let value: Value = serde_json::from_str(crate::agent::json_text(text))
        .map_err(|error| format!("it is not valid JSON ({error})"))?;
    output_schema::validate(schema, &value)?;
    Ok(value)
}

/// Outcome of [`enforce`]. A valid answer carries its report (text replaced by the compact JSON)
/// and the parsed value. A rejected answer carries the report with its text cleared, so the run
/// reports no answer, plus the validation error.
pub enum Checked {
    Valid(TurnReport, Value),
    Rejected(TurnReport, String),
}

/// Validate `report`'s answer; on rejection ask for a corrected answer up to `retries` times.
/// `correct` runs one more turn with the correction message. An interrupted turn stops the loop
/// at once rather than spending the budget.
pub fn enforce(
    mut report: TurnReport,
    schema: &Value,
    retries: u32,
    mut correct: impl FnMut(&str) -> Result<TurnReport>,
) -> Result<Checked> {
    let mut attempt = 0;
    loop {
        match structured_answer(&report.text, schema) {
            Ok(value) => {
                report.text = value.to_string();
                return Ok(Checked::Valid(report, value));
            }
            Err(why) if !report.interrupted && attempt < retries => {
                attempt += 1;
                let message = format!(
                    "Your answer was rejected: {why}. Reply again with only a JSON value that validates against the schema. Schema correction attempt {attempt} of {retries}."
                );
                report = correct(&message)?;
            }
            Err(why) => {
                report.text.clear();
                if matches!(
                    report.outcome,
                    TurnOutcome::Completed | TurnOutcome::Answered
                ) {
                    report.outcome = TurnOutcome::Incomplete;
                }
                return Ok(Checked::Rejected(report, why));
            }
        }
    }
}

/// Write the final answer to `path` atomically (temporary file, then rename). An empty answer
/// writes nothing: the file is neither created nor overwritten. Returns whether it was written.
pub fn write_last_message(path: &Path, answer: &str) -> Result<bool> {
    if answer.trim().is_empty() {
        return Ok(false);
    }
    let mut bytes = answer.as_bytes().to_vec();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    crate::platform::atomic::write_replacing(path, &bytes)
        .with_context(|| format!("cannot write --output-last-message {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests;
