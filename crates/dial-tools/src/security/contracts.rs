//! Security contracts: canonical JSON, deterministic fingerprints and
//! IDs, and the persisted document types. Field names are camelCase on disk.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: &str = "1.0";
pub const PLAN_DOCUMENT_TYPE: &str = "dial-security.scan-plan";
pub const SCAN_DOCUMENT_TYPE: &str = "dial-security.scan";

/// Lower-case hex SHA-256 of `input`.
pub fn sha256_hex(input: &[u8]) -> String {
    crate::hex(&Sha256::digest(input))
}

/// JSON with object keys sorted recursively and no insignificant whitespace. Absent optional fields must be omitted by the caller, as JS
/// `undefined` is dropped.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// A repository-relative location of a finding or evidence item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

fn normalize_fingerprint_path(value: &str) -> String {
    let slashed = value.replace('\\', "/");
    slashed
        .strip_prefix("./")
        .map(str::to_owned)
        .unwrap_or(slashed)
}

/// Sort key columns (path, lines, columns, role) followed by the normalized object.
type LocationRow = (
    String,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<String>,
    Value,
);

/// Locations normalized and sorted (undefined sorts first).
fn normalized_locations(locations: &[Location]) -> Vec<Value> {
    let mut rows: Vec<LocationRow> = locations
        .iter()
        .map(|location| {
            let path = normalize_fingerprint_path(&location.path);
            let mut object = Map::new();
            object.insert("path".into(), Value::String(path.clone()));
            if let Some(v) = location.start_line {
                object.insert("startLine".into(), v.into());
            }
            if let Some(v) = location.end_line {
                object.insert("endLine".into(), v.into());
            }
            if let Some(v) = location.start_column {
                object.insert("startColumn".into(), v.into());
            }
            if let Some(v) = location.end_column {
                object.insert("endColumn".into(), v.into());
            }
            if let Some(v) = &location.role {
                object.insert("role".into(), Value::String(v.clone()));
            }
            (
                path,
                location.start_line,
                location.end_line,
                location.start_column,
                location.end_column,
                location.role.clone(),
                Value::Object(object),
            )
        })
        .collect();
    rows.sort_by(|a, b| (&a.0, a.1, a.2, a.3, a.4, &a.5).cmp(&(&b.0, b.1, b.2, b.3, b.4, &b.5)));
    rows.into_iter().map(|row| row.6).collect()
}

/// `dial-security/v1:sha256:<hex>` over rule, category, anchor and normalized locations.
pub fn finding_fingerprint(
    rule_id: &str,
    category: &str,
    anchor: Option<&str>,
    locations: &[Location],
) -> String {
    let mut material = Map::new();
    material.insert(
        "ruleId".into(),
        Value::String(rule_id.trim().to_lowercase()),
    );
    material.insert(
        "category".into(),
        Value::String(category.trim().to_lowercase()),
    );
    if let Some(anchor) = anchor
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
    {
        material.insert("anchor".into(), Value::String(anchor));
    }
    material.insert(
        "locations".into(),
        Value::Array(normalized_locations(locations)),
    );
    format!(
        "dial-security/v1:sha256:{}",
        sha256_hex(canonical_json(&Value::Object(material)).as_bytes())
    )
}

pub fn finding_id(fingerprint: &str) -> String {
    format!("secf_{}", &sha256_hex(fingerprint.as_bytes())[..24])
}

/// Scan IDs are random: `secscan_` plus 32 hex digits.
pub fn new_scan_id() -> String {
    format!("secscan_{}", crate::hex(&rand::random::<[u8; 16]>()))
}

pub fn plan_id(fingerprint: &str) -> String {
    format!("secplan_{}", &sha256_hex(fingerprint.as_bytes())[..24])
}

/// Readable tail of the root plus 12 hex digits of its hash.
pub fn project_key(repository_root: &str) -> String {
    let normalized = repository_root.replace('\\', "/");
    let normalized = normalized
        .strip_suffix('/')
        .unwrap_or(&normalized)
        .to_owned();
    let stripped = normalized.strip_prefix('/').unwrap_or(&normalized);
    let mut readable = String::new();
    let mut in_run = false;
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            readable.push(ch);
            in_run = false;
        } else if !in_run {
            readable.push('-');
            in_run = true;
        }
    }
    let trimmed = readable.trim_matches('-');
    let chars: Vec<char> = trimmed.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(80)..].iter().collect();
    let label = if tail.is_empty() {
        "project".to_owned()
    } else {
        tail
    };
    format!("{label}-{}", &sha256_hex(normalized.as_bytes())[..12])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Informational,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    Planned,
    Running,
    Completed,
    Partial,
    Cancelled,
    Failed,
}

impl ScanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Partial => "partial",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Open,
    FalsePositive,
    AcceptedRisk,
    Fixed,
    WontFix,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::FalsePositive => "false_positive",
            Self::AcceptedRisk => "accepted_risk",
            Self::Fixed => "fixed",
            Self::WontFix => "wont_fix",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "open" => Self::Open,
            "false_positive" => Self::FalsePositive,
            "accepted_risk" => Self::AcceptedRisk,
            "fixed" => Self::Fixed,
            "wont_fix" => Self::WontFix,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Unvalidated,
    Validated,
    Rejected,
    Partial,
    Error,
}

impl ValidationStatus {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "unvalidated" => Self::Unvalidated,
            "validated" => Self::Validated,
            "rejected" => Self::Rejected,
            "partial" => Self::Partial,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Repository,
    ScopedPath,
    RefDiff,
    WorkingTree,
    Imported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Producer {
    pub kind: String,
    pub name: String,
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One persisted finding inside a scan's `findings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub fingerprint: String,
    pub rule_id: String,
    pub title: String,
    pub category: String,
    pub severity: Severity,
    pub confidence: Confidence,
    pub disposition: Disposition,
    pub locations: Vec<Location>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_status: Option<ValidationStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_evidence: Vec<ValidationEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationEvidence {
    pub label: String,
    pub explanation: String,
}

/// The persisted scan record (`scan.json`). Written last: its presence is the commit marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scan {
    pub document_type: String,
    pub schema_version: String,
    pub id: String,
    pub project_key: String,
    pub repository_root: String,
    pub target_kind: TargetKind,
    pub status: ScanStatus,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub producer: Producer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudRef>,
    pub findings: Vec<Finding>,
}

/// Link from a scan to its Codex Security cloud origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudRef {
    pub configuration_id: String,
    pub repository_id: String,
    pub repository_url: String,
    pub environment_id: String,
}

/// Output-directory state recorded in a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputPlan {
    pub root: String,
    pub archive_existing: bool,
    pub existing_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetPlan {
    pub kind: TargetKind,
    pub repository_root: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_revision: Option<String>,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub tree_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeBaseRef {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

/// A reviewer account reference. Only the OAuth form is produced by Dial (`chatgpt`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRef {
    pub provider: String,
    /// ChatGPT OAuth account `client_id` (Dial's credential key).
    pub credential_id: String,
}

/// A reviewed scan plan (`plans/<id>.json`). The fingerprint covers repository, target, knowledge
/// bases, output, model, account and both config and workflow fingerprints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub document_type: String,
    pub schema_version: String,
    pub id: String,
    pub created_at: String,
    pub repository_root: String,
    pub target: TargetPlan,
    pub knowledge_bases: Vec<KnowledgeBaseRef>,
    pub output: OutputPlan,
    pub model: ModelRef,
    pub account: AccountRef,
    pub config_fingerprint: String,
    pub workflow_fingerprint: String,
    pub fingerprint: String,
}

/// Canonical JSON of a value serialized through serde.
pub fn canonical_of<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(canonical_json(&serde_json::to_value(value)?))
}

/// Serde string form of a unit enum (e.g. `Severity::High` -> `"high"`).
pub fn label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
