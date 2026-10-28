//! `report_tool_issue`: drafts a bug report about a Dial tool.
//!
//! The draft is redacted and bounded, then retained as a local artifact (or returned inline when
//! no artifact store is attached). Nothing is posted: filing the report stays with the user, so the
//! tool needs no network access and no approval.

use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};

use crate::build_info::VERSION;
use crate::config::types::Config;
use crate::storage::redact::redact_text;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, tool_schema};

const TOOL: &str = "report_tool_issue";
const CATEGORIES: &[&str] = &["bug", "crash", "performance", "docs", "other"];
const FIELD_LIMIT: usize = 4_096;
const DRAFT_LIMIT: usize = 16_384;
const DESCRIPTION: &str = "Draft a bug report about a Dial tool: summary, expected and actual \
    behavior, reproduction steps, and environment. The draft is redacted and saved locally as an \
    artifact; nothing is sent anywhere. Use it to record a tool malfunction for the user to file.";

pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ReportToolIssue)]
}

struct ReportToolIssue;

impl Tool for ReportToolIssue {
    fn name(&self) -> &str {
        TOOL
    }

    fn schema(&self) -> Value {
        tool_schema(
            TOOL,
            DESCRIPTION,
            json!({
                "tool": {
                    "type": "string",
                    "description": "Name of the Dial tool the report concerns."
                },
                "summary": {
                    "type": "string",
                    "description": "One-line description of the problem."
                },
                "expected": {
                    "type": "string",
                    "description": "What the tool should have done."
                },
                "actual": {
                    "type": "string",
                    "description": "What the tool actually did, including error text."
                },
                "steps": {
                    "type": "string",
                    "description": "Steps that reproduce the problem."
                },
                "category": {
                    "type": "string",
                    "enum": CATEGORIES,
                    "description": "Kind of problem. Defaults to bug."
                }
            }),
            &["tool", "summary"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match draft(ctx, args) {
            Ok(output) => output,
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }
}

fn required(args: &Value, field: &str) -> Result<String> {
    let value = args[field].as_str().map_or("", str::trim);
    ensure!(
        !value.is_empty(),
        "report_tool_issue requires a non-empty {field}"
    );
    Ok(value.to_owned())
}

fn optional(args: &Value, field: &str) -> Result<Option<String>> {
    match args.get(field).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow!("report_tool_issue {field} must be a string"))?
                .trim();
            Ok((!text.is_empty()).then(|| text.to_owned()))
        }
    }
}

fn category(args: &Value) -> Result<&'static str> {
    match args.get("category").filter(|value| !value.is_null()) {
        None => Ok("bug"),
        Some(value) => {
            let requested = value
                .as_str()
                .ok_or_else(|| anyhow!("report_tool_issue category must be a string"))?;
            CATEGORIES
                .iter()
                .copied()
                .find(|category| *category == requested)
                .ok_or_else(|| {
                    anyhow!(
                        "report_tool_issue category must be one of {}",
                        CATEGORIES.join(", ")
                    )
                })
        }
    }
}

fn draft(ctx: &ToolContext, args: &Value) -> Result<ToolOutput> {
    let tool = required(args, "tool")?;
    let summary = required(args, "summary")?;
    let category = category(args)?;
    let expected = optional(args, "expected")?;
    let actual = optional(args, "actual")?;
    let steps = optional(args, "steps")?;

    let markdown = render(&Report {
        tool: &tool,
        category,
        summary: &summary,
        expected: expected.as_deref(),
        actual: actual.as_deref(),
        steps: steps.as_deref(),
    });
    let markdown = bounded(&redact_text(&markdown), DRAFT_LIMIT);

    Ok(match ctx.workspace.artifacts() {
        Some(store) => {
            let id = store.retain(&markdown)?;
            ToolOutput::ok(format!(
                "Drafted a report for `{tool}` and saved it as artifact {id}. Nothing was sent; \
                 review the draft and file it yourself."
            ))
        }
        None => ToolOutput::ok(format!(
            "Drafted a report for `{tool}`. Nothing was sent; review the draft and file it \
             yourself.\n\n{markdown}"
        )),
    })
}

struct Report<'a> {
    tool: &'a str,
    category: &'a str,
    summary: &'a str,
    expected: Option<&'a str>,
    actual: Option<&'a str>,
    steps: Option<&'a str>,
}

fn render(report: &Report) -> String {
    let section = |heading: &str, body: Option<&str>| {
        format!(
            "## {heading}\n\n{}\n\n",
            body.map_or_else(
                || "(not provided)".into(),
                |text| bounded(text, FIELD_LIMIT)
            )
        )
    };
    format!(
        "# Tool issue: {tool}\n\n\
         - Category: {category}\n\
         - Dial: {VERSION}\n\
         - Platform: {os} ({arch})\n\n\
         ## Summary\n\n{summary}\n\n{expected}{actual}{steps}",
        tool = report.tool,
        category = report.category,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        summary = bounded(report.summary, FIELD_LIMIT),
        expected = section("Expected", report.expected),
        actual = section("Actual", report.actual),
        steps = section("Steps to reproduce", report.steps),
    )
}

/// Clips `text` to `limit` characters, marking the cut.
fn bounded(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}\n[truncated]", &text[..cut]),
    }
}

#[cfg(test)]
mod tests;
