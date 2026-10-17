use super::*;

const GOOD: &str = r#"Here you go:
```json
{
  "name": "No Any!",
  "description": "Never use any",
  "condition": ": any",
  "scope": ["tool:edit(*.ts)", "tool:write(*.ts)"],
  "body": "Use unknown."
}
```"#;

#[test]
fn parses_fenced_rule_and_assembles_frontmatter() {
    let candidate = candidate_from_reply(GOOD).expect("valid rule");
    assert_eq!(candidate.name, "no-any");
    assert!(candidate.markdown.starts_with("---\nname: \"no-any\"\n"));
    assert!(
        candidate
            .markdown
            .contains("scope: [\"tool:edit(*.ts)\", \"tool:write(*.ts)\"]")
    );
    assert!(candidate.markdown.trim_end().ends_with("Use unknown."));
}

#[test]
fn falls_back_to_balanced_object_without_fence() {
    let reply =
        r#"noise {"name":"x","description":"d","condition":"a}b","scope":"text","body":"b"} tail"#;
    let candidate = candidate_from_reply(reply).expect("valid rule");
    assert_eq!(candidate.name, "x");
}

#[test]
fn rejects_invalid_rules_with_reasons() {
    assert_eq!(
        candidate_from_reply("no json here").unwrap_err(),
        "Missing generated rule JSON object"
    );
    assert_eq!(
        candidate_from_reply(
            r#"{"name":"!!!","description":"d","condition":"a","scope":"text","body":"b"}"#
        )
        .unwrap_err(),
        "Rule name must contain at least one letter or digit"
    );
    assert_eq!(
        candidate_from_reply(r#"{"name":"n","description":"d","scope":"text","body":"b"}"#)
            .unwrap_err(),
        "Generated rule JSON must include condition or astCondition"
    );
    assert!(
        candidate_from_reply(
            r#"{"name":"n","description":"d","astCondition":"x","scope":"text","body":"b"}"#
        )
        .unwrap_err()
        .contains("AST triggers require a tool scope")
    );
    assert_eq!(
        candidate_from_reply(
            r#"{"name":"n","description":"d","condition":"a","scope":[],"body":"b"}"#
        )
        .unwrap_err(),
        "Generated rule JSON must include at least one scope"
    );
    assert!(
        candidate_from_reply(
            r#"{"name":"n","description":"d","condition":"(","scope":"text","body":"b"}"#
        )
        .is_err()
    );
}

#[test]
fn repairs_double_escaped_regex_once() {
    let reply = r#"{"name":"eval-call","description":"d","condition":"\\\\beval\\\\s*\\\\(","scope":"text","body":"b"}"#;
    let candidate = candidate_from_reply(reply).expect("repaired rule");
    assert!(
        candidate
            .markdown
            .contains(r#"condition: "\\beval\\s*\\(""#)
    );
}

#[test]
fn prompt_carries_complaint_and_feedback() {
    let fresh = build_prompt("  stop using any  ", "", None);
    assert!(fresh.contains("Complaint:\nstop using any\n"));
    assert!(!fresh.contains("Failed attempts"));
    let retry = build_prompt("c", "Attempt 1 failed", Some("{}"));
    assert!(retry.contains("Attempt 1 failed"));
    assert!(retry.contains("Latest candidate JSON:\n{}"));
}

#[test]
fn sanitizes_names_like_omp() {
    assert_eq!(sanitize_name("  \"Go Range!!Int\" "), "go-range-int");
    assert_eq!(sanitize_name("snake_case-ok"), "snake_case-ok");
}

#[test]
fn skips_braces_in_prose_before_the_rule() {
    let reply = r#"Use {name} placeholders. {"name":"x","description":"d","condition":"a","scope":"text","body":"b"}"#;
    let candidate = candidate_from_reply(reply).expect("rule after prose braces");
    assert_eq!(candidate.name, "x");
}
