//! Codex Security cloud client and import.
//!
//! Requests go through `llm::oauth::util::request` with the ChatGPT OAuth access token and
//! account id. A 401 re-acquires the access once; a second 401 is an authentication failure.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};

use super::contracts::{
    self, CloudRef, Confidence, Disposition, Finding, Location, Producer, Scan, ScanStatus,
    Severity, TargetKind, ValidationEvidence, ValidationStatus,
};
use super::store::Store;
use crate::llm::chatgpt;
use crate::llm::oauth::util::{self, Body};
use crate::runtime::clock::format_utc;

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/aardvark";
const USER_AGENT: &str = "dial";
const PAGE_LIMIT: u64 = 500;
const FINDING_PAGE_LIMIT: u64 = 500;
const ALL_FINDING_STATUSES: &[&str] = &[
    "new",
    "triaged",
    "in_progress",
    "fixed",
    "wontfix",
    "duplicate",
    "false_positive",
];

/// Returns `(ChatGPT-Account-Id header value, bearer token)` for one request.
type Access = Box<dyn Fn() -> Result<(String, String)> + Send + Sync>;

/// Producer recorded on every imported cloud scan.
pub fn producer() -> Producer {
    Producer {
        kind: "codex-security-cloud".into(),
        name: "Codex Security cloud".into(),
        vendor: "OpenAI".into(),
        version: None,
    }
}

/// Scan lookback: a positive day count or the whole history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookback {
    Days(u64),
    All,
}

/// Parses `lookbackDays` from its textual form: a positive integer or `all`.
pub fn parse_lookback(raw: &str) -> Result<Lookback> {
    let trimmed = raw.trim();
    if trimmed == "all" {
        return Ok(Lookback::All);
    }
    match trimmed.parse::<u64>() {
        Ok(days) if days >= 1 => Ok(Lookback::Days(days)),
        _ => bail!("lookbackDays must be a positive integer or 'all'"),
    }
}

#[derive(Debug, Clone)]
pub struct Configuration {
    pub id: String,
    pub source_id: Option<String>,
    pub repository_id: String,
    pub repository_url: String,
    pub environment_id: String,
    pub state: Option<String>,
    pub current_step: Option<String>,
}

/// Inputs for creating a cloud configuration.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub repository_id: String,
    pub repository_url: String,
    pub environment_id: String,
    pub lookback: Option<Lookback>,
    pub maintainer_attack_concerns: Option<String>,
    pub maintainer_focus_areas: Option<String>,
    pub maintainer_additional_context: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FindingCounts {
    pub critical: f64,
    pub high: f64,
    pub medium: f64,
    pub low: f64,
    pub informational: f64,
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub pending_commits: f64,
    pub finished_commits: f64,
    pub failed_commits: f64,
    pub finding_counts: FindingCounts,
    pub current_step: Option<String>,
    pub last_scanned_commit: Option<String>,
}

/// Authenticated client for the Codex Security cloud API.
pub struct CloudClient {
    base_url: String,
    access: Access,
    cancel: Arc<AtomicBool>,
}

impl CloudClient {
    /// Client for a ChatGPT OAuth account in `user_dir`, using the production base URL.
    pub fn for_account(user_dir: &Path, account_id: &str) -> Self {
        Self::with_access(DEFAULT_BASE_URL, Self::account_access(user_dir, account_id))
    }

    /// Client against an explicit base URL with an explicit access source.
    pub fn with_access(base_url: &str, access: Access) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            access,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn account_access(user_dir: &Path, account_id: &str) -> Access {
        let dir = user_dir.to_path_buf();
        let account = account_id.to_owned();
        let cancel = Arc::new(AtomicBool::new(false));
        Box::new(move || {
            let (header_account, token) =
                chatgpt::scoped_account_access(&dir, &account, Arc::clone(&cancel))?;
            Ok((header_account, token.as_str().to_owned()))
        })
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let url = self.url(path, query);
        for attempt in 0..2 {
            let (account, token) = (self.access)()?;
            let auth = format!("Bearer {token}");
            let headers = [
                ("Accept", "application/json"),
                ("Authorization", auth.as_str()),
                ("ChatGPT-Account-Id", account.as_str()),
                ("User-Agent", USER_AGENT),
            ];
            let payload = match body {
                Some(value) => Body::Json(value),
                None => Body::Empty,
            };
            let response = util::request(method, &url, &headers, payload, &self.cancel)?;
            if response.status == 401 {
                if attempt == 0 {
                    continue;
                }
                bail!("Codex Security cloud authentication refresh failed");
            }
            ensure!(
                response.ok(),
                "Codex Security cloud request failed ({}) at {path}",
                response.status
            );
            return Ok(response.json_or_null());
        }
        bail!("Codex Security cloud authentication refresh failed")
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        let mut url = format!("{}/{path}", self.base_url);
        if !query.is_empty() {
            let pairs: Vec<String> = query
                .iter()
                .map(|(key, value)| format!("{key}={}", percent_encode(value)))
                .collect();
            url.push('?');
            url.push_str(&pairs.join("&"));
        }
        url
    }

    fn configuration_page(&self, cursor: Option<&str>) -> Result<ConfigurationPage> {
        let mut query = vec![("limit", PAGE_LIMIT.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_owned()));
        }
        let page = self.request("GET", "scan_configurations", &query, None)?;
        let items = match field(&page, "items") {
            Some(Value::Array(items)) => items
                .iter()
                .map(normalize_configuration)
                .collect::<Result<Vec<_>>>()?,
            _ => Vec::new(),
        };
        Ok(ConfigurationPage {
            items,
            next_cursor: opt_str(&page, "next_cursor"),
        })
    }

    /// Every configuration visible to the account, paged.
    pub fn all_configurations(&self) -> Result<Vec<Configuration>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.configuration_page(cursor.as_deref())?;
            out.extend(page.items);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(out),
            }
        }
    }

    /// Finds a configuration by its `id` or source id, paging until found.
    pub fn configuration(&self, id: &str) -> Result<Configuration> {
        let mut cursor: Option<String> = None;
        loop {
            let page = self.configuration_page(cursor.as_deref())?;
            if let Some(found) = page
                .items
                .into_iter()
                .find(|item| item.id == id || item.source_id.as_deref() == Some(id))
            {
                return Ok(found);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => bail!("Unknown Codex Security cloud configuration: {id}"),
            }
        }
    }

    /// Creates a configuration for the repository.
    pub fn create_configuration(&self, request: &StartRequest) -> Result<Configuration> {
        let (_, token) = (self.access)()?;
        let owner_id = jwt_subject(&token)?;
        let lookback = match request.lookback.unwrap_or(Lookback::Days(30)) {
            Lookback::All => Value::Null,
            Lookback::Days(days) => json!(days),
        };
        let mut scan_input = Map::new();
        scan_input.insert("environment_id".into(), json!(request.environment_id));
        scan_input.insert("lookback_days".into(), lookback);
        scan_input.insert("notification_rules".into(), json!([]));
        scan_input.insert("owner_id".into(), json!(owner_id));
        scan_input.insert("repo_id".into(), json!(request.repository_id));
        scan_input.insert("repo_url".into(), json!(request.repository_url));
        scan_input.insert("share_targets".into(), json!([]));
        scan_input.insert("state".into(), json!("enabled"));
        let optional = [
            (
                "maintainer_attack_concerns",
                &request.maintainer_attack_concerns,
            ),
            ("maintainer_focus_areas", &request.maintainer_focus_areas),
            (
                "maintainer_additional_context",
                &request.maintainer_additional_context,
            ),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                scan_input.insert(key.into(), json!(value));
            }
        }
        let body = json!({ "scan_input": Value::Object(scan_input) });
        let raw = self.request("POST", "scan_configurations", &[], Some(&body))?;
        normalize_configuration(&raw)
    }

    /// Stats for one configuration.
    pub fn stats(&self, configuration_id: &str) -> Result<Stats> {
        let path = format!(
            "scan_configurations/{}/stats",
            percent_encode(configuration_id)
        );
        let raw = self.request("GET", &path, &[], None)?;
        Ok(Stats {
            pending_commits: finite(field(&raw, "pending_commits")),
            finished_commits: finite(field(&raw, "finished_commits")),
            failed_commits: finite(field(&raw, "failed_commits")),
            finding_counts: FindingCounts {
                critical: finite(field(&raw, "critical_findings")),
                high: finite(field(&raw, "high_findings")),
                medium: finite(field(&raw, "medium_findings")),
                low: finite(field(&raw, "low_findings")),
                informational: finite(field(&raw, "informational_findings")),
            },
            current_step: opt_str(&raw, "current_step"),
            last_scanned_commit: opt_str(&raw, "last_scanned_commit_hash"),
        })
    }

    /// Finding details for a configuration, attributed to it.
    ///
    /// Details are fetched one at a time, not in concurrent batches of eight.
    pub fn finding_details(
        &self,
        repository_url: &str,
        configuration: &Configuration,
    ) -> Result<Vec<Value>> {
        let mut summaries: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![
                ("repo", repository_url.to_owned()),
                ("limit", FINDING_PAGE_LIMIT.to_string()),
                ("status", ALL_FINDING_STATUSES.join(",")),
            ];
            if let Some(cursor) = &cursor {
                query.push(("cursor", cursor.clone()));
            }
            let page = self.request("GET", "scan-findings", &query, None)?;
            if let Some(Value::Array(items)) = field(&page, "items") {
                for item in items {
                    ensure!(
                        item.is_object(),
                        "Codex Security cloud returned an invalid object"
                    );
                    summaries.push(item.clone());
                }
            }
            match opt_str(&page, "next_cursor") {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let mut ids: Vec<&str> = vec![configuration.id.as_str()];
        if let Some(source) = configuration.source_id.as_deref() {
            ids.push(source);
        }
        let mut details = Vec::new();
        for summary in summaries.iter().filter(|item| attributed(item, &ids)) {
            let id = req_str(
                field(summary, "hid").or_else(|| field(summary, "id")),
                "finding id",
            )?;
            let path = format!("scan-findings/{}", percent_encode(&id));
            let detail = self.request("GET", &path, &[], None)?;
            ensure!(
                detail.is_object(),
                "Codex Security cloud returned an invalid object"
            );
            details.push(detail);
        }
        // The list is scoped only by repository URL; re-check attribution on each detail.
        Ok(details
            .into_iter()
            .filter(|detail| attributed(detail, &ids))
            .collect())
    }
}

struct ConfigurationPage {
    items: Vec<Configuration>,
    next_cursor: Option<String>,
}

/// A finding is attributed when it has no configured scan id or the id names this configuration.
fn attributed(item: &Value, ids: &[&str]) -> bool {
    match opt_str(item, "configured_scan_id") {
        None => true,
        Some(id) => ids.iter().any(|known| *known == id),
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|v| !v.is_null())
}

fn opt_str(value: &Value, key: &str) -> Option<String> {
    field(value, key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn req_str(value: Option<&Value>, label: &str) -> Result<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Codex Security cloud response is missing {label}"))
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn finite(value: Option<&Value>) -> f64 {
    number(value).unwrap_or(0.0)
}

fn positive_integer(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|n| *n >= 1)
}

fn object_field<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    match field(value, key) {
        Some(found) if found.is_object() => Ok(found),
        _ => bail!("Codex Security cloud returned an invalid object"),
    }
}

fn normalize_configuration(raw: &Value) -> Result<Configuration> {
    ensure!(
        raw.is_object(),
        "Codex Security cloud returned an invalid object"
    );
    let scan_input = object_field(raw, "scan_input")?;
    let id = req_str(
        field(raw, "hid").or_else(|| field(raw, "id")),
        "configuration id",
    )?;
    let source_id = opt_str(raw, "id").filter(|source| *source != id);
    let repository_id = req_str(field(scan_input, "repo_id"), "repository id")?;
    let repository_url = req_str(field(scan_input, "repo_url"), "repository URL")?;
    let environment_id = req_str(field(scan_input, "environment_id"), "environment id")?;
    Ok(Configuration {
        id,
        source_id,
        repository_id,
        repository_url,
        environment_id,
        state: opt_str(scan_input, "state"),
        current_step: opt_str(raw, "current_step"),
    })
}

fn decode_jwt(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The authenticated user id: JWT `sub`, else `user_id`.
fn jwt_subject(token: &str) -> Result<String> {
    let claims = decode_jwt(token)
        .ok_or_else(|| anyhow!("The selected ChatGPT credential is not a valid JWT"))?;
    let subject = field(&claims, "sub").or_else(|| field(&claims, "user_id"));
    req_str(subject, "authenticated user id")
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Repo-relative, forward-slash path; rejects absolute, drive-qualified and `..` paths.
fn normalize_path(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?;
    let slashed = raw.trim().replace('\\', "/");
    let normalized = slashed.strip_prefix("./").unwrap_or(&slashed).to_owned();
    let bytes = normalized.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    if normalized.is_empty()
        || normalized.starts_with('/')
        || drive
        || normalized.split('/').any(|part| part == "..")
    {
        return None;
    }
    Some(normalized)
}

fn severity_of(value: Option<&Value>) -> Severity {
    match value.and_then(Value::as_str) {
        Some("critical") => Severity::Critical,
        Some("high") => Severity::High,
        Some("medium") => Severity::Medium,
        Some("low") => Severity::Low,
        _ => Severity::Informational,
    }
}

fn confidence_of(commit: &Value) -> Confidence {
    match number(field(commit, "validation_confidence")) {
        Some(score) if score >= 0.67 => Confidence::High,
        Some(score) if score >= 0.34 => Confidence::Medium,
        Some(_) => Confidence::Low,
        None if commit.get("validated") == Some(&Value::Bool(true)) => Confidence::High,
        None => Confidence::Medium,
    }
}

fn disposition_of(value: Option<&Value>) -> Disposition {
    match value.and_then(Value::as_str) {
        Some("fixed") => Disposition::Fixed,
        Some("false_positive") => Disposition::FalsePositive,
        Some("wontfix") => Disposition::WontFix,
        Some("duplicate") => Disposition::AcceptedRisk,
        _ => Disposition::Open,
    }
}

fn locations_of(commit: &Value) -> Result<Vec<Location>> {
    let mut locations = Vec::new();
    if let Some(Value::Array(lines)) = field(commit, "relevant_lines") {
        for line in lines {
            let Some(path) = normalize_path(field(line, "path")) else {
                continue;
            };
            let Some(start) = positive_integer(field(line, "start_line_number")) else {
                continue;
            };
            let end = positive_integer(field(line, "end_line_number")).filter(|end| *end >= start);
            locations.push(Location {
                path,
                start_line: Some(start),
                end_line: end,
                start_column: None,
                end_column: None,
                role: None,
            });
        }
    }
    if locations.is_empty()
        && let Some(Value::Array(files)) = field(commit, "files_involved")
    {
        for file in files {
            if let Some(path) = normalize_path(Some(file)) {
                locations.push(Location {
                    path,
                    start_line: Some(1),
                    end_line: None,
                    start_column: None,
                    end_column: None,
                    role: Some("cloud-file".into()),
                });
            }
        }
    }
    ensure!(
        !locations.is_empty(),
        "Codex Security cloud finding has no usable repository-relative location"
    );
    Ok(locations)
}

/// Normalizes one cloud finding detail.
///
/// Dropped here: source excerpts, remediation text, severity and disposition
/// rationale, cloud provenance ids, and the provenance metadata block. Dial's `Finding` does
/// not carry them.
fn normalize_finding(raw: &Value) -> Result<Finding> {
    let commit = field(raw, "commit_analysis")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let title = req_str(
        field(&commit, "title").or_else(|| field(raw, "title")),
        "finding title",
    )?;
    let rule_id = opt_str(&commit, "rule_id").unwrap_or_else(|| {
        let digest = contracts::sha256_hex(title.trim().to_lowercase().as_bytes());
        format!("codex-security:{}", &digest[..16])
    });
    let category = opt_str(&commit, "category").unwrap_or_else(|| "codex-security".to_owned());
    req_str(field(raw, "hid").or_else(|| field(raw, "id")), "finding id")?;
    let locations = locations_of(&commit)?;
    let fingerprint = contracts::finding_fingerprint(&rule_id, &category, None, &locations);
    let validation_report = opt_str(&commit, "validation_report");
    let validation_evidence = validation_report
        .clone()
        .or_else(|| opt_str(&commit, "fix_check_report"))
        .map(|explanation| {
            vec![ValidationEvidence {
                label: "Cloud validation".into(),
                explanation,
            }]
        })
        .unwrap_or_default();
    let validated = commit.get("validated") == Some(&Value::Bool(true));
    Ok(Finding {
        id: contracts::finding_id(&fingerprint),
        fingerprint,
        rule_id,
        title: title.clone(),
        category,
        severity: severity_of(field(raw, "criticality").or_else(|| field(&commit, "criticality"))),
        confidence: confidence_of(&commit),
        disposition: disposition_of(field(raw, "status")),
        locations,
        anchor: None,
        description: Some(
            opt_str(&commit, "description")
                .or_else(|| opt_str(raw, "description"))
                .unwrap_or(title),
        ),
        validation_status: Some(if validated {
            ValidationStatus::Validated
        } else {
            ValidationStatus::Unvalidated
        }),
        validation_summary: validation_report,
        validation_evidence,
    })
}

/// Lowercased `host/path` identity for an origin URL or scp-style remote.
pub fn repository_identity(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if let Some((user, rest)) = trimmed.split_once('@').filter(|_| !trimmed.contains("://"))
        && !user.is_empty()
        && let Some((host, path)) = rest.split_once(':')
        && !host.is_empty()
        && !path.is_empty()
    {
        return format!(
            "{}/{}",
            host.to_lowercase(),
            path.trim_start_matches('/').to_lowercase()
        );
    }
    match url::Url::parse(trimmed) {
        Ok(parsed) => format!(
            "{}/{}",
            parsed.host_str().unwrap_or_default().to_lowercase(),
            parsed.path().trim_start_matches('/').to_lowercase()
        ),
        Err(_) => trimmed.to_lowercase(),
    }
}

/// The `origin` remote URL of the repository, read-only (`git remote get-url origin`).
pub fn origin_remote(repository_root: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repository_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!url.is_empty()).then_some(url)
}

fn assert_origin_matches(configuration: &Configuration, repository_root: &str) -> Result<()> {
    let Some(origin) = origin_remote(repository_root) else {
        bail!(
            "Codex Security cloud import requires a verifiable repository identity; this project has no 'origin' remote"
        );
    };
    ensure!(
        repository_identity(&origin) == repository_identity(&configuration.repository_url),
        "Codex Security cloud configuration does not match this project's origin remote"
    );
    Ok(())
}

/// The Markdown report stored beside an imported scan.
pub fn report(configuration: &Configuration, stats: &Stats, findings: &[Finding]) -> String {
    let mut lines = vec![
        "# Codex Security cloud results".to_owned(),
        String::new(),
        format!("- Configuration: {}", configuration.id),
        format!("- Repository: {}", configuration.repository_url),
        format!(
            "- Current step: {}",
            stats
                .current_step
                .as_deref()
                .or(configuration.current_step.as_deref())
                .unwrap_or("unknown")
        ),
        format!(
            "- Last scanned commit: {}",
            stats.last_scanned_commit.as_deref().unwrap_or("unknown")
        ),
        format!("- Findings imported: {}", findings.len()),
        String::new(),
        "## Findings".to_owned(),
        String::new(),
    ];
    for finding in findings {
        lines.push(format!(
            "- **{}** {} ({})",
            contracts::label(&finding.severity),
            finding.title,
            finding.id
        ));
    }
    format!("{}\n", lines.join("\n"))
}

/// Builds the imported scan record for a cloud configuration.
fn build_scan(
    configuration: &Configuration,
    stats: &Stats,
    findings: Vec<Finding>,
    store: &Store,
    scan_id: &str,
    imported_at: &str,
) -> Scan {
    Scan {
        document_type: contracts::SCAN_DOCUMENT_TYPE.into(),
        schema_version: contracts::SCHEMA_VERSION.into(),
        id: scan_id.to_owned(),
        project_key: store.project_key().to_owned(),
        repository_root: store.repository_root().to_owned(),
        target_kind: TargetKind::Imported,
        status: ScanStatus::Completed,
        created_at: imported_at.to_owned(),
        completed_at: Some(imported_at.to_owned()),
        producer: producer(),
        plan_id: None,
        revision: stats.last_scanned_commit.clone(),
        cloud: Some(CloudRef {
            configuration_id: configuration.id.clone(),
            repository_id: configuration.repository_id.clone(),
            repository_url: configuration.repository_url.clone(),
            environment_id: configuration.environment_id.clone(),
        }),
        findings,
    }
}

/// Imports a cloud configuration's findings as a new completed scan.
/// The scan, `results.sarif`, and `report.md` are persisted.
pub fn pull(client: &CloudClient, store: &Store, configuration_id: &str) -> Result<Scan> {
    let imported_at = format_utc(crate::llm::oauth::util::now_ms() / 1000);
    let configuration = client.configuration(configuration_id)?;
    let stats = client.stats(&configuration.id)?;
    assert_origin_matches(&configuration, store.repository_root())?;
    let details = client.finding_details(&configuration.repository_url, &configuration)?;
    let findings = details
        .iter()
        .map(normalize_finding)
        .collect::<Result<Vec<_>>>()?;
    let scan_id = contracts::new_scan_id();
    let scan = build_scan(
        &configuration,
        &stats,
        findings,
        store,
        &scan_id,
        &imported_at,
    );
    store.save_scan(&scan)?;
    let sarif = serde_json::to_vec_pretty(&super::sarif::export(&scan))?;
    store.write_scan_artifact(&scan_id, "results.sarif", &sarif)?;
    let markdown = report(&configuration, &stats, &scan.findings);
    store.write_scan_artifact(&scan_id, "report.md", markdown.as_bytes())?;
    Ok(scan)
}

#[cfg(test)]
mod tests;
