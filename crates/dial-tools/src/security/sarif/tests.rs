use super::*;

#[test]
fn severity_thresholds_follow_scores() {
    assert_eq!(severity_for(Some(9.0), "note"), Severity::Critical);
    assert_eq!(severity_for(Some(7.0), "note"), Severity::High);
    assert_eq!(severity_for(Some(4.0), "note"), Severity::Medium);
    assert_eq!(severity_for(Some(0.1), "note"), Severity::Low);
    assert_eq!(severity_for(Some(0.0), "error"), Severity::Informational);
    assert_eq!(severity_for(None, "error"), Severity::High);
    assert_eq!(severity_for(None, "warning"), Severity::Medium);
    assert_eq!(severity_for(None, "other"), Severity::Informational);
}

#[test]
fn import_rejects_non_sarif_and_outside_artifacts() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.rs"), "x").unwrap();
    let bad = json!({"version": "1.0", "runs": []});
    assert!(import(&bad, repo.path(), "k", "secscan_a", "t").is_err());

    let log = json!({
        "version": "2.1.0",
        "runs": [{"tool": {"driver": {"name": "tool"}}, "results": [{
            "ruleId": "r1", "level": "error",
            "message": {"text": "boom"},
            "locations": [{"physicalLocation": {"artifactLocation": {"uri": outside.path().join("x.rs").display().to_string()}}}]
        }]}]
    });
    let error = import(&log, repo.path(), "k", "secscan_a", "t")
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("SARIF artifact resolves outside the repository"),
        "{error}"
    );
}

#[test]
fn import_then_export_keeps_rule_and_location() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/a.rs"), "a").unwrap();
    let log = json!({
        "version": "2.1.0",
        "runs": [{"tool": {"driver": {"name": "tool", "rules": [
            {"id": "r1", "shortDescription": {"text": "Title"}, "properties": {"security-severity": "8.1"}}
        ]}}, "results": [{
            "ruleId": "r1", "level": "warning",
            "message": {"text": "boom"},
            "locations": [{"physicalLocation": {
                "artifactLocation": {"uri": "src/a.rs"},
                "region": {"startLine": 3}
            }}]
        }]}]
    });
    let scan = import(&log, repo.path(), "k", "secscan_a", "t").unwrap();
    assert_eq!(scan.findings.len(), 1);
    assert_eq!(scan.findings[0].severity, Severity::High);
    assert_eq!(scan.findings[0].locations[0].path, "src/a.rs");
    let out = export(&scan);
    assert_eq!(out["runs"][0]["results"][0]["ruleId"], "r1");
    assert_eq!(
        out["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
        3
    );
}

#[test]
fn export_names_the_producer_version_only_when_known() {
    let repo = tempfile::tempdir().unwrap();
    let log = serde_json::json!({"version": "2.1.0", "runs": [{"tool": {"driver": {"name": "tool"}}, "results": []}]});
    let mut scan = import(&log, repo.path(), "proj", "scan-1", "2024-01-01T00:00:00Z").unwrap();
    assert!(
        export(&scan)["runs"][0]["tool"]["driver"]
            .get("version")
            .is_none()
    );
    scan.producer.version = Some("1.2.3".into());
    assert_eq!(
        export(&scan)["runs"][0]["tool"]["driver"]["version"],
        "1.2.3"
    );
}
