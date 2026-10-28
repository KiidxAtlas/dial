use serde_json::{Value, json};

use super::ReportToolIssue;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::testkit;

fn run(args: Value) -> crate::tools::ToolOutput {
    let (_dir, ws) = testkit::workspace();
    let config = Config::default();
    ReportToolIssue.call(
        &ToolContext {
            workspace: &ws,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &args,
    )
}

#[test]
fn drafts_report_with_sections_and_environment_without_sending_anything() {
    let out = run(json!({
        "tool": "image_gen",
        "summary": "output extension check is wrong",
        "expected": "a PNG file",
        "actual": "JPEG bytes written to .png",
        "steps": "call image_gen with output fox.png",
        "category": "bug"
    }));

    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Nothing was sent"), "{}", out.text);
    assert!(out.text.contains("# Tool issue: image_gen"), "{}", out.text);
    assert!(out.text.contains("- Category: bug"), "{}", out.text);
    assert!(
        out.text.contains("JPEG bytes written to .png"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("call image_gen with output fox.png"),
        "{}",
        out.text
    );
    assert!(out.text.contains(std::env::consts::OS), "{}", out.text);
}

#[test]
fn redacts_credentials_pasted_into_the_report() {
    let token = format!("ghp_{}", "a".repeat(36));
    let out = run(json!({
        "tool": "github",
        "summary": "auth failed with token",
        "actual": format!("error printed {token}"),
    }));

    assert!(!out.is_error, "{}", out.text);
    assert!(!out.text.contains(&token), "{}", out.text);
    assert!(out.text.contains("[REDACTED:"), "{}", out.text);
}

#[test]
fn bounds_oversized_fields() {
    let out = run(json!({
        "tool": "bash",
        "summary": "huge output",
        "steps": "x".repeat(100_000),
    }));

    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.len() < 20_000,
        "draft grew to {} bytes",
        out.text.len()
    );
    assert!(out.text.contains("[truncated]"));
}

#[test]
fn rejects_missing_summary_and_unknown_category() {
    let missing = run(json!({"tool": "bash"}));
    assert!(missing.is_error);
    assert!(missing.text.contains("summary"), "{}", missing.text);

    let unknown = run(json!({"tool": "bash", "summary": "x", "category": "rant"}));
    assert!(unknown.is_error);
    assert!(
        unknown.text.contains("category must be one of"),
        "{}",
        unknown.text
    );
}

#[test]
fn needs_no_approval() {
    assert!(!ReportToolIssue.needs_approval(&json!({"tool": "x", "summary": "y"})));
}
