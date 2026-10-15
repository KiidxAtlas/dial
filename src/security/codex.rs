//! Codex Security bundle import. Reads the four bundle
//! documents, validates document types, schema version and scan-id agreement, and maps each
//! finding into a flat Dial [`Scan`]. Fields Dial's flat finding model does not carry (evidence,
//! remediation, coverage, upstream provenance) are validated but not persisted.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use super::contracts::{
    self, Confidence, Disposition, Finding, Location, Producer, Scan, ScanStatus, Severity,
    TargetKind, ValidationEvidence, ValidationStatus,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    document_type: Option<String>,
    schema_version: Option<String>,
    scan: Option<ManifestScan>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestScan {
    id: Option<String>,
    producer: Option<ManifestProducer>,
    started_at: Option<String>,
    completed_at: Option<String>,
    target: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ManifestProducer {
    name: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindingsDocument {
    document_type: Option<String>,
    schema_version: Option<String>,
    scan_id: Option<String>,
    #[serde(default)]
    findings: Vec<SourceFinding>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CoverageDocument {
    document_type: Option<String>,
    schema_version: Option<String>,
    scan_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceFinding {
    finding_id: Option<String>,
    rule_id: Option<String>,
    identity: Option<Identity>,
    fingerprints: Option<Fingerprints>,
    title: Option<String>,
    summary: Option<String>,
    severity: Option<SeverityIn>,
    confidence: Option<ConfidenceIn>,
    taxonomy: Option<Taxonomy>,
    #[serde(default)]
    locations: Vec<LocationIn>,
    #[serde(default)]
    code_evidence: Vec<EvidenceIn>,
    validation: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Identity {
    anchor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fingerprints {
    primary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SeverityIn {
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConfidenceIn {
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Taxonomy {
    category: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocationIn {
    path: Option<String>,
    start_line: Option<u64>,
    end_line: Option<u64>,
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EvidenceIn {
    id: Option<String>,
    label: Option<String>,
    explanation: Option<String>,
}

/// Imported location path: a repository-relative, non-traversing POSIX path.
pub fn imported_location_path(value: &str) -> Result<String> {
    let normalized = value.replace('\\', "/");
    let normalized = normalized
        .strip_prefix("./")
        .unwrap_or(&normalized)
        .to_owned();
    let drive = normalized.len() >= 3
        && normalized.as_bytes()[0].is_ascii_alphabetic()
        && normalized.as_bytes()[1..3] == *b":/";
    ensure!(
        !normalized.is_empty()
            && !normalized.starts_with('/')
            && !drive
            && !normalized.split('/').any(|s| s == ".."),
        "Codex Security bundle locations must be repository-relative: {value}"
    );
    Ok(normalized)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

const SEVERITIES: [(&str, Severity); 5] = [
    ("critical", Severity::Critical),
    ("high", Severity::High),
    ("medium", Severity::Medium),
    ("low", Severity::Low),
    ("informational", Severity::Informational),
];

fn map_validation_status(value: Option<&Value>) -> ValidationStatus {
    value
        .and_then(|v| v.get("status"))
        .and_then(Value::as_str)
        .and_then(ValidationStatus::parse)
        .unwrap_or(ValidationStatus::Unvalidated)
}

/// Imports a bundle directory into a completed scan of `repo_root`.
pub fn import_bundle(
    bundle_dir: &Path,
    repo_root: &Path,
    project_key: &str,
    scan_id: &str,
    created_at_override: Option<&str>,
) -> Result<Scan> {
    let root = bundle_dir;
    let manifest: Manifest = read_json(&root.join("scan-manifest.json"))?;
    let findings_doc: FindingsDocument = read_json(&root.join("findings.json"))?;
    let coverage: CoverageDocument = read_json(&root.join("coverage.json"))?;
    ensure!(
        manifest.document_type.as_deref() == Some("codex-security.scan-manifest")
            && manifest.schema_version.as_deref() == Some("1.0"),
        "Unsupported Codex Security scan manifest"
    );
    ensure!(
        findings_doc.document_type.as_deref() == Some("codex-security.findings")
            && findings_doc.schema_version.as_deref() == Some("1.0"),
        "Unsupported Codex Security findings document"
    );
    ensure!(
        coverage.document_type.as_deref() == Some("codex-security.coverage")
            && coverage.schema_version.as_deref() == Some("1.0"),
        "Unsupported Codex Security coverage document"
    );
    let manifest_scan = manifest
        .scan
        .context("Unsupported Codex Security scan manifest")?;
    let bundle_scan_id = manifest_scan.id.clone().unwrap_or_default();
    ensure!(
        !bundle_scan_id.is_empty()
            && findings_doc.scan_id.as_deref() == Some(bundle_scan_id.as_str())
            && coverage.scan_id.as_deref() == Some(bundle_scan_id.as_str()),
        "Codex Security bundle scan IDs do not agree"
    );

    let repo = fs::canonicalize(repo_root)?;
    let created_at = created_at_override
        .map(str::to_owned)
        .or(manifest_scan.started_at.clone())
        .unwrap_or_else(|| super::store::format_utc(now_secs()));
    let producer = Producer {
        kind: "codex-security-bundle".into(),
        name: manifest_scan
            .producer
            .as_ref()
            .and_then(|p| p.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "codex-security".into()),
        vendor: "openai".into(),
        version: manifest_scan
            .producer
            .as_ref()
            .and_then(|p| p.version.clone()),
    };
    let target = manifest_scan.target.clone().unwrap_or(Value::Null);
    let target_kind = match target.get("kind").and_then(Value::as_str).unwrap_or("") {
        "git_diff" => TargetKind::RefDiff,
        "git_worktree" => TargetKind::WorkingTree,
        _ => TargetKind::Imported,
    };
    let revision = target
        .get("revision")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut findings = Vec::new();
    for source in findings_doc.findings {
        let rule_id = source
            .rule_id
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "codex-security.unknown".into());
        let category = source
            .taxonomy
            .as_ref()
            .and_then(|t| t.category.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| {
                rule_id
                    .split(['.', '/', '-'])
                    .next()
                    .unwrap_or("security")
                    .to_owned()
            });
        let has_source_locations = source
            .locations
            .iter()
            .any(|l| l.path.is_some() && l.start_line.is_some());
        let mut locations = Vec::new();
        for location in &source.locations {
            let (Some(path), Some(start)) = (&location.path, location.start_line) else {
                continue;
            };
            locations.push(Location {
                path: imported_location_path(path)?,
                start_line: Some(start),
                end_line: location.end_line,
                start_column: None,
                end_column: None,
                role: location.role.clone(),
            });
        }
        if locations.is_empty() {
            locations.push(Location {
                path: "unknown".into(),
                start_line: Some(1),
                end_line: None,
                start_column: None,
                end_column: None,
                role: Some("unknown".into()),
            });
        }
        let anchor = source
            .identity
            .as_ref()
            .and_then(|i| i.anchor.clone())
            .filter(|a| !a.is_empty())
            .or_else(|| {
                source
                    .fingerprints
                    .as_ref()
                    .and_then(|f| f.primary.clone())
                    .filter(|a| !a.is_empty())
            })
            .or_else(|| {
                if has_source_locations {
                    None
                } else {
                    source.finding_id.clone()
                }
            });
        let fingerprint =
            contracts::finding_fingerprint(&rule_id, &category, anchor.as_deref(), &locations);
        let severity = source
            .severity
            .as_ref()
            .and_then(|s| s.level.as_deref())
            .and_then(|level| {
                SEVERITIES
                    .iter()
                    .find(|(name, _)| *name == level)
                    .map(|(_, s)| *s)
            })
            .unwrap_or(Severity::Informational);
        let confidence = match source.confidence.as_ref().and_then(|c| c.level.as_deref()) {
            Some("high") => Confidence::High,
            Some("low") => Confidence::Low,
            _ => Confidence::Medium,
        };
        let validation_status = map_validation_status(source.validation.as_ref());
        let validation_summary = source.validation.as_ref().map(|v| v.to_string());
        let validation_evidence = source
            .code_evidence
            .iter()
            .enumerate()
            .map(|(index, item)| ValidationEvidence {
                label: item
                    .label
                    .clone()
                    .or_else(|| item.id.clone())
                    .unwrap_or_else(|| format!("Evidence {}", index + 1)),
                explanation: item.explanation.clone().unwrap_or_default(),
            })
            .collect();
        findings.push(Finding {
            id: contracts::finding_id(&fingerprint),
            fingerprint,
            rule_id: rule_id.clone(),
            title: source
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or(rule_id),
            category,
            severity,
            confidence,
            disposition: Disposition::Open,
            locations,
            anchor,
            description: source.summary.clone().filter(|s| !s.is_empty()),
            validation_status: Some(validation_status),
            validation_summary,
            validation_evidence,
        });
    }

    Ok(Scan {
        document_type: contracts::SCAN_DOCUMENT_TYPE.to_owned(),
        schema_version: contracts::SCHEMA_VERSION.to_owned(),
        id: scan_id.to_owned(),
        project_key: project_key.to_owned(),
        repository_root: repo.display().to_string(),
        target_kind,
        status: ScanStatus::Completed,
        created_at: created_at.clone(),
        completed_at: Some(manifest_scan.completed_at.unwrap_or(created_at)),
        producer,
        plan_id: None,
        revision,
        cloud: None,
        findings,
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_producer_version_is_carried_onto_the_scan() {
        let dir = std::env::temp_dir().join(format!("codex-bundle-version-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("scan-manifest.json"),
            r#"{"documentType":"codex-security.scan-manifest","schemaVersion":"1.0","scan":{"id":"s1","producer":{"name":"codex","version":"0.9.1"},"startedAt":"2024-01-01T00:00:00Z","completedAt":"2024-01-01T00:01:00Z","target":{"kind":"git_worktree"}}}"#,
        )
        .unwrap();
        fs::write(
            dir.join("findings.json"),
            r#"{"documentType":"codex-security.findings","schemaVersion":"1.0","scanId":"s1","findings":[]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("coverage.json"),
            r#"{"documentType":"codex-security.coverage","schemaVersion":"1.0","scanId":"s1"}"#,
        )
        .unwrap();
        let result = import_bundle(&dir, &std::env::temp_dir(), "proj", "scan-1", None);
        fs::remove_dir_all(&dir).ok();
        let scan = result.unwrap();
        assert_eq!(scan.producer.name, "codex");
        assert_eq!(scan.producer.version.as_deref(), Some("0.9.1"));
    }

    #[test]
    fn multibyte_location_path_does_not_panic() {
        assert_eq!(imported_location_path("a1€/x.rs").unwrap(), "a1€/x.rs");
        assert!(imported_location_path("C:/x.rs").is_err());
    }
}
