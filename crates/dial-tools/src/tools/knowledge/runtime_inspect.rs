//! Offline runtime-source interpretation, never application execution or profiling.
mod chrome;
mod folded;
mod otlp;
#[cfg(test)]
mod tests;

use super::evidence::{publish, source};
use crate::config::types::Config;
use crate::runtime::evidence::{
    Coverage, CoverageExtent, Evidence, EvidenceContext, EvidenceKind, Location, Method,
    Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

// Parsing is intentionally tighter than the shared source reader's 64 MiB capture ceiling.
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 20_000;
const MAX_TEXT: usize = 4096;

/// Register the local runtime source interpreter.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(RuntimeInspect)]
}
struct RuntimeInspect;
impl Tool for RuntimeInspect {
    fn name(&self) -> &str {
        "runtime_inspect"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "runtime_inspect",
            "Interpret an explicitly selected local Chrome Trace JSON, OTLP JSON export, or folded-stack CPU profile offline. Never executes/profiles an app or contacts a collector. Chrome X/B/E timestamps are microseconds; B/E pairs are per process/thread in source order, not causal links. OTLP decimal-string nanoseconds use exact arithmetic; explicit trace/parent IDs only. Folded integer weights are counts, not wall time; inclusive weight counts each function once per stack even under recursion. Source must be <=8 MiB and <=20000 records (folded <=256 frames/line). Unsupported Chrome events are counted, not interpreted; malformed supported records fail. Returns bounded slowest spans or function weights, coverage and evidence with source digest; no original-byte retention, clock alignment, causal or verification claims.",
            json!({
                "action":{"type":"string","enum":["inspect"]},
                "path":{"type":"string"},
                "format":{"type":"string","enum":["chrome_trace","otlp_json","folded_stacks"]},
                "limit":{"type":"integer","minimum":1,"maximum":200}
            }),
            &["action", "path", "format"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        false
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        run(ctx.workspace, args).unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    path: String,
    format: Format,
    limit: Option<usize>,
}
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Format {
    ChromeTrace,
    OtlpJson,
    FoldedStacks,
}

/// Inspect bounded local bytes and publish only the calculated representation plus source identity.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())?;
    ensure!(
        args.action == "inspect",
        "unknown runtime inspection action"
    );
    let limit = args.limit.unwrap_or(50);
    ensure!((1..=200).contains(&limit), "limit must be 1..=200");
    let bytes = source::read(ws, &args.path)?;
    let result = inspect(&bytes, args.format, limit)?;
    let representation = serde_json::to_vec(&result)?;
    let truncated = result["coverage"]["rows_omitted"].as_u64().unwrap_or(0) > 0;
    let item = Evidence {
        source: args.path, location: Location::Whole, method: Method::Calculation,
        producer: "dial/runtime-inspect-v1".into(), kind: EvidenceKind::Observation,
        summary: "Offline calculations from runtime source records; no application execution".into(),
        representation: Representation::of_bytes("application/json", &representation),
        limitations: vec!["Source-reported timing, IDs and weights are untrusted; no causality, clock alignment or independent verification. Unsupported fields are not interpreted. Original bytes are not retained.".into()],
        context: EvidenceContext { snapshot: Some(source::snapshot(&bytes)?), coverage: Some(Coverage {
            extent: CoverageExtent::Partial, inspected: vec![Location::Whole], omitted: vec![],
            truncated, failures: vec![],
        }), ..Default::default() },
    };
    publish(ws, ToolOutput::structured(result), item, &representation)
}
fn inspect(bytes: &[u8], format: Format, limit: usize) -> Result<Value> {
    ensure!(
        bytes.len() <= MAX_BYTES,
        "runtime source exceeds 8 MiB parser limit"
    );
    let mut result = match format {
        Format::ChromeTrace => chrome::inspect(&serde_json::from_slice(bytes)?, limit)?,
        Format::OtlpJson => otlp::inspect(&serde_json::from_slice(bytes)?, limit)?,
        Format::FoldedStacks => folded::inspect(std::str::from_utf8(bytes)?, limit)?,
    };
    result["execution"] = json!("none; offline source interpretation only");
    result["verification_credit"] = json!(false);
    result["limits"] = json!({"bytes":MAX_BYTES,"records":MAX_RECORDS,"output_rows":limit});
    Ok(result)
}
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    let s = value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string {field}"))?;
    ensure!(
        !s.is_empty() && s.len() <= MAX_TEXT,
        "{field} must contain 1..=4096 bytes"
    );
    Ok(s)
}
fn array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("missing array {field}"))
}
