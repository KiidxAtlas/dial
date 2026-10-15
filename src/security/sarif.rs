//! SARIF 2.1.0 export and import. Export maps a scan to one run whose
//! rules are keyed by rule id. Import turns each result into a [`Finding`] and rejects artifact
//! URIs that resolve outside the repository.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

use super::contracts::{
    self, Confidence, Disposition, Finding, Location, Producer, Scan, ScanStatus, Severity,
    TargetKind,
};

const SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";

fn level_for(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical | Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
        Severity::Informational => "none",
    }
}

fn security_score(severity: Severity) -> f64 {
    match severity {
        Severity::Critical => 9.5,
        Severity::High => 7.5,
        Severity::Medium => 5.0,
        Severity::Low => 2.0,
        Severity::Informational => 0.0,
    }
}

/// Exports a scan as a SARIF 2.1.0 log.
pub fn export(scan: &Scan) -> Value {
    let mut rules: Vec<&Finding> = Vec::new();
    for finding in &scan.findings {
        if !rules.iter().any(|r| r.rule_id == finding.rule_id) {
            rules.push(finding);
        }
    }
    let rule_values: Vec<Value> = rules
        .iter()
        .map(|finding| {
            json!({
                "id": finding.rule_id,
                "name": finding.rule_id,
                "shortDescription": { "text": finding.title },
                "fullDescription": { "text": finding.description.clone().unwrap_or_else(|| finding.title.clone()) },
                "properties": {
                    "tags": [finding.category],
                    "security-severity": security_score(finding.severity),
                },
            })
        })
        .collect();
    let results: Vec<Value> = scan
        .findings
        .iter()
        .map(|finding| {
            let locations: Vec<Value> = finding
                .locations
                .iter()
                .map(|location| {
                    let mut region = Map::new();
                    if let Some(v) = location.start_line {
                        region.insert("startLine".into(), json!(v));
                    }
                    if let Some(v) = location.end_line {
                        region.insert("endLine".into(), json!(v));
                    }
                    if let Some(v) = location.start_column {
                        region.insert("startColumn".into(), json!(v));
                    }
                    if let Some(v) = location.end_column {
                        region.insert("endColumn".into(), json!(v));
                    }
                    json!({
                        "physicalLocation": {
                            "artifactLocation": { "uri": location.path, "uriBaseId": "%SRCROOT%" },
                            "region": region,
                        }
                    })
                })
                .collect();
            json!({
                "ruleId": finding.rule_id,
                "level": level_for(finding.severity),
                "message": { "text": finding.description.clone().unwrap_or_else(|| finding.title.clone()) },
                "locations": locations,
                "fingerprints": { "dial-security/v1": finding.fingerprint },
                "properties": {
                    "findingId": finding.id,
                    "confidence": contracts::label(&finding.confidence),
                    "disposition": contracts::label(&finding.disposition),
                    "category": finding.category,
                    "security-severity": security_score(finding.severity),
                },
            })
        })
        .collect();
    let mut driver = json!({
        "name": scan.producer.name,
        "informationUri": "https://github.com/KiidxAtlas/dial",
        "rules": rule_values,
    });
    if let Some(version) = &scan.producer.version {
        driver["version"] = json!(version);
    }
    json!({
        "$schema": SCHEMA,
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": driver },
            "results": results,
            "originalUriBaseIds": { "%SRCROOT%": { "uri": format!("file://{}/", scan.repository_root) } },
        }],
    })
}

fn severity_for(score: Option<f64>, level: &str) -> Severity {
    if let Some(score) = score {
        if score >= 9.0 {
            return Severity::Critical;
        }
        if score >= 7.0 {
            return Severity::High;
        }
        if score >= 4.0 {
            return Severity::Medium;
        }
        if score > 0.0 {
            return Severity::Low;
        }
        return Severity::Informational;
    }
    match level {
        "error" => Severity::High,
        "warning" => Severity::Medium,
        "note" => Severity::Low,
        _ => Severity::Informational,
    }
}

/// Resolves a SARIF artifact URI (a percent-encoded URI reference or `file://` URI) against the
/// canonical repository root, then checks containment via realpath.
fn repository_relative(repo_root: &Path, uri: &str) -> Result<String> {
    let base = url::Url::from_directory_path(repo_root)
        .map_err(|()| anyhow::anyhow!("repository root is not an absolute path"))?;
    let candidate = base
        .join(uri)
        .with_context(|| format!("SARIF artifact URI is invalid: {uri}"))?
        .to_file_path()
        .map_err(|()| anyhow::anyhow!("SARIF artifact is not a local file: {uri}"))?;
    let resolved = fs::canonicalize(&candidate)
        .with_context(|| format!("SARIF artifact could not be resolved: {uri}"))?;
    ensure!(
        resolved.starts_with(repo_root),
        "SARIF artifact resolves outside the repository: {uri}"
    );
    let relative = resolved.strip_prefix(repo_root)?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

/// Imports a SARIF 2.1.0 log into a completed scan of the repository at `repo_root`.
pub fn import(
    log: &Value,
    repo_root: &Path,
    project_key: &str,
    scan_id: &str,
    created_at: &str,
) -> Result<Scan> {
    ensure!(
        log.get("version").and_then(Value::as_str) == Some("2.1.0")
            && log.get("runs").is_some_and(Value::is_array),
        "Expected SARIF 2.1.0 input"
    );
    let repo = fs::canonicalize(repo_root)?;
    let mut findings = Vec::new();
    let mut producer = Producer {
        kind: "sarif-import".into(),
        name: "SARIF import".into(),
        vendor: "SARIF".into(),
        version: None,
    };
    for run in log
        .get("runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(name) = run.pointer("/tool/driver/name").and_then(Value::as_str) {
            producer.name = name.to_owned();
        }
        let rules = run
            .pointer("/tool/driver/rules")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for result in run
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let rule_id = result
                .get("ruleId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let rule = rules
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(rule_id.as_str()));
            let title = rule
                .and_then(|r| r.pointer("/shortDescription/text"))
                .and_then(Value::as_str)
                .or_else(|| result.pointer("/message/text").and_then(Value::as_str))
                .unwrap_or(&rule_id)
                .to_owned();
            let score = result
                .pointer("/properties/security-severity")
                .or_else(|| rule.and_then(|r| r.pointer("/properties/security-severity")))
                .and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                });
            let level = result
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("warning");
            let mut locations = Vec::new();
            for loc in result
                .get("locations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(uri) = loc
                    .pointer("/physicalLocation/artifactLocation/uri")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let path = repository_relative(&repo, uri)?;
                let region = loc.pointer("/physicalLocation/region");
                let num = |key: &str| region.and_then(|r| r.get(key)).and_then(Value::as_u64);
                locations.push(Location {
                    path,
                    start_line: num("startLine"),
                    end_line: num("endLine"),
                    start_column: num("startColumn"),
                    end_column: num("endColumn"),
                    role: None,
                });
            }
            let category = "sarif".to_owned();
            let fingerprint = contracts::finding_fingerprint(&rule_id, &category, None, &locations);
            findings.push(Finding {
                id: contracts::finding_id(&fingerprint),
                fingerprint,
                rule_id,
                title,
                category,
                severity: severity_for(score, level),
                confidence: Confidence::Medium,
                disposition: Disposition::Open,
                locations,
                anchor: None,
                description: result
                    .pointer("/message/text")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                validation_status: None,
                validation_summary: None,
                validation_evidence: vec![],
            });
        }
    }
    Ok(Scan {
        document_type: contracts::SCAN_DOCUMENT_TYPE.to_owned(),
        schema_version: contracts::SCHEMA_VERSION.to_owned(),
        id: scan_id.to_owned(),
        project_key: project_key.to_owned(),
        repository_root: repo.display().to_string(),
        target_kind: TargetKind::Imported,
        status: ScanStatus::Completed,
        created_at: created_at.to_owned(),
        completed_at: Some(created_at.to_owned()),
        producer,
        plan_id: None,
        revision: None,
        cloud: None,
        findings,
    })
}

/// Rejects a non-object or unversioned document early with a clear error.
pub fn parse_log(text: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(text).context("Expected SARIF 2.1.0 input")?;
    if !value.is_object() {
        bail!("Expected SARIF 2.1.0 input");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
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
}
