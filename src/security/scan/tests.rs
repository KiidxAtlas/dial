use super::*;

#[test]
fn render_fills_values_and_drops_empty_blocks() {
    let template = "A {{x}}{{#if d}}\nDIFF {{d}}\n{{/if}}Z";
    let kept = render(template, &[("x", "1".into()), ("d", "patch".into())]);
    assert_eq!(kept, "A 1\nDIFF patch\nZ");
    let dropped = render(template, &[("x", "1".into()), ("d", String::new())]);
    assert_eq!(dropped, "A 1Z");
}

#[test]
fn reviewer_prompt_is_the_expected_body() {
    assert!(REVIEWER_PROMPT.starts_with("Review assigned repository scope only."));
    assert!(REVIEWER_PROMPT.ends_with("state what was reviewed."));
}

#[test]
fn parses_report_and_fingerprints_findings() {
    let text = r#"{
            "coverage_summary": "reviewed src/a.rs",
            "reviewed_paths": ["src/a.rs"],
            "findings": [{
                "rule_id": "sql-injection", "title": "SQLi", "summary": "concat",
                "severity": "high", "confidence": "medium", "category": "injection",
                "locations": [{"path": "src/a.rs", "start_line": 7}],
                "cwe": ["CWE-89"], "evidence": []
            }]
        }"#;
    let report = parse_report(text).unwrap();
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].severity, Severity::High);
    assert_eq!(report.findings[0].disposition, Disposition::Open);
    assert_eq!(report.reviewed, vec!["src/a.rs".to_owned()]);
    assert!(!report.has_deferred);
}

#[test]
fn rejects_non_json_reviewer_output() {
    let error = parse_report("no json here").unwrap_err().to_string();
    assert_eq!(error, "Security reviewer returned invalid output");
}

#[test]
fn rejects_traversal_in_reviewer_locations() {
    let text = r#"{"coverage_summary":"x","findings":[{"rule_id":"r","title":"t","summary":"s",
            "severity":"low","confidence":"low","category":"c",
            "locations":[{"path":"../etc/passwd","start_line":1}],"cwe":[],"evidence":[]}]}"#;
    assert!(parse_report(text).is_err());
}

#[test]
fn cancel_registration_round_trip() {
    let (flag, registration) = register("secscan_test_cancel");
    assert!(is_active("secscan_test_cancel"));
    assert!(request_cancel("secscan_test_cancel"));
    assert!(flag.load(Ordering::SeqCst));
    drop(registration);
    assert!(!is_active("secscan_test_cancel"));
    assert!(!request_cancel("secscan_test_cancel"));
}
