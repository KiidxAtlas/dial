//! Security operations shared by the `security_scan` tool and the `/security` command.
//!
//! Each operation takes a JSON argument object and returns the text shown to the user. Operations that
//! need the model-driven reviewer (`scan`, `start`, `validate`) run through the [`super::review::Reviewer`]
//! seam that the `/security` command supplies; `status` and `cancel` act on the store.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::types::Config;

use super::auth;
use super::cloud::{self, CloudClient};
use super::codex;
use super::comparison;
use super::contracts::{self, AccountRef, Disposition, ModelRef, Plan, Scan, label};
use super::coordinator;
use super::preflight::{self, PlanRequest, TargetRequest};
use super::sarif;
use super::scan;
use super::store::Store;
use crate::runtime::clock::format_utc;

/// Provider string recorded for a ChatGPT OAuth reviewer account (Dial's credential key).
const ACCOUNT_PROVIDER: &str = "openai-codex";

/// Every operation name accepted by [`run`].
pub const OPERATIONS: &[&str] = &[
    "preflight",
    "scan",
    "start",
    "status",
    "cancel",
    "validate",
    "cloud_configs",
    "cloud_create",
    "cloud_status",
    "cloud_pull",
    "scans",
    "show",
    "disposition",
    "export",
    "import",
    "compare",
    "lineage",
];

/// Everything an operation needs from the session configuration.
#[derive(Debug, Clone)]
pub struct Env {
    pub user_dir: PathBuf,
    pub project_root: PathBuf,
    pub provider: String,
    pub model: String,
    /// Operator policy: when set, only these ChatGPT account ids may act as reviewer.
    pub allowed_accounts: Option<BTreeSet<String>>,
}

impl Env {
    pub fn from_config(config: &Config) -> Self {
        Self {
            user_dir: config.user_dir.clone(),
            project_root: config.project_root.clone(),
            provider: config.provider.clone().unwrap_or_default(),
            model: config.model.clone(),
            allowed_accounts: config.chatgpt_allowed_accounts.clone(),
        }
    }
}

/// Runs one operation. Returns the user-facing text on success.
pub fn run(
    env: &Env,
    op: &str,
    args: &Value,
    runner: Option<&mut dyn super::review::Reviewer>,
) -> Result<String> {
    match op {
        "preflight" => preflight_op(env, args),
        "scan" | "start" => scan_op(env, args, runner),
        "status" => status_op(env, args),
        "cancel" => cancel_op(env, args),
        "validate" => validate_op(env, args, runner),
        "cloud_configs" => cloud_configs(env, args),
        "cloud_create" => cloud_create(env, args),
        "cloud_status" => cloud_status(env, args),
        "cloud_pull" => cloud_pull(env, args),
        "scans" => scans(env),
        "show" => show(env, args),
        "disposition" => disposition(env, args),
        "export" => export(env, args),
        "import" => import(env, args),
        "compare" => compare(env, args),
        "lineage" => lineage(env, args),
        other => bail!("Unknown security operation: {other}"),
    }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn required(args: &Value, key: &str) -> Result<String> {
    str_arg(args, key).with_context(|| format!("{key} is required"))
}

fn list_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn now_utc() -> String {
    format_utc(crate::llm::oauth::util::now_ms() / 1000)
}

/// A resolved ChatGPT reviewer account.
struct Reviewer {
    client_id: String,
}

/// The signed-in reviewer account, honoring an optional `account` pin and operator policy.
fn reviewer(env: &Env, args: &Value) -> Result<Reviewer> {
    let account = auth::select_account(&env.user_dir, str_arg(args, "account").as_deref())?;
    if let Some(allowed) = &env.allowed_accounts
        && !allowed.contains(&account.client_id)
    {
        bail!(
            "ChatGPT account {} is not permitted by policy",
            account.client_id
        );
    }
    Ok(Reviewer {
        client_id: account.client_id,
    })
}

/// Repository-local store, rooted at the Git toplevel of the project.
fn open_store(env: &Env) -> Result<Store> {
    let root = repository_root(env)?;
    Store::open(&env.user_dir, &root)
}

fn repository_root(env: &Env) -> Result<String> {
    preflight::repository_root(&env.project_root)?.with_context(|| {
        format!(
            "Security scans require a Git repository: {}",
            env.project_root.display()
        )
    })
}

fn cloud_client(env: &Env, args: &Value) -> Result<CloudClient> {
    let reviewer = reviewer(env, args)?;
    Ok(CloudClient::for_account(&env.user_dir, &reviewer.client_id))
}

fn target_request(args: &Value) -> Result<TargetRequest> {
    let include = list_arg(args, "include");
    let exclude = list_arg(args, "exclude");
    let kind = str_arg(args, "target").unwrap_or_else(|| "repository".to_owned());
    Ok(match kind.as_str() {
        "repository" => TargetRequest::Repository { include, exclude },
        "scoped_path" => TargetRequest::ScopedPath { include, exclude },
        "ref_diff" => TargetRequest::RefDiff {
            base: required(args, "base")?,
            head: required(args, "head")?,
            include,
            exclude,
        },
        other => bail!("Unknown security target: {other}"),
    })
}

fn make_plan(env: &Env, args: &Value) -> Result<Plan> {
    let target = target_request(args)?;
    let reviewer = reviewer(env, args)?;
    let output = match str_arg(args, "output") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                env.project_root.join(path)
            }
        }
        None => env.user_dir.join("security").join("outputs"),
    };
    let model = ModelRef {
        provider: env.provider.clone(),
        model_id: env.model.clone(),
        thinking_level: None,
    };
    let request = PlanRequest {
        cwd: &env.project_root,
        target,
        knowledge_bases: list_arg(args, "knowledgeBases"),
        output_root: &output,
        archive_existing: flag(args, "archiveExisting"),
        model,
        account: AccountRef {
            provider: ACCOUNT_PROVIDER.to_owned(),
            credential_id: reviewer.client_id,
        },
        config_material: json!({ "model": env.model, "provider": env.provider }),
        workflow_fingerprint: workflow_fingerprint(),
    };
    preflight::create_plan(&request, &now_utc())
}

fn preflight_op(env: &Env, args: &Value) -> Result<String> {
    let plan = make_plan(env, args)?;
    let store = Store::open(&env.user_dir, &plan.repository_root)?;
    store.save_plan(&plan)?;
    Ok(format!(
        "Created security plan {}\nFingerprint: {}\nTarget: {} ({})\nOutput: {}\nReviewer account: {}",
        plan.id,
        plan.fingerprint,
        label(&plan.target.kind),
        plan.target.display_name,
        plan.output.root,
        reviewer(env, args)?.client_id,
    ))
}

fn scan_op(
    env: &Env,
    args: &Value,
    runner: Option<&mut dyn super::review::Reviewer>,
) -> Result<String> {
    let Some(runner) = runner else {
        bail!(
            "Security scans run the reviewer agent; start them from an interactive session with `/security scan`."
        );
    };
    reviewer(env, args)?;
    let store = open_store(env)?;
    let plan = match str_arg(args, "plan") {
        Some(id) => store.load_plan(&id)?,
        None => {
            let plan = make_plan(env, args)?;
            store.save_plan(&plan)?;
            plan
        }
    };
    preflight::assert_fresh(
        &plan,
        json!({ "model": env.model, "provider": env.provider }),
    )?;
    let run = coordinator::start(&store, &plan, runner)?;
    let scan = &run.scan;
    let mut lines = vec![
        format!("Scan {}", scan.id),
        format!("Status: {}", scan.status.as_str()),
        format!("Target: {}", label(&scan.target_kind)),
        format!("Findings: {}", scan.findings.len()),
    ];
    if !run.deferred.is_empty() {
        lines.push(format!("Deferred: {}", run.deferred.len()));
        lines.extend(run.deferred.iter().map(|item| format!("  {item}")));
    }
    Ok(lines.join("\n"))
}

fn status_op(env: &Env, args: &Value) -> Result<String> {
    let store = open_store(env)?;
    match str_arg(args, "scan") {
        Some(id) => {
            let mut scan = store.load_scan(&id)?;
            let note = if scan::reconcile_interrupted(&store, &mut scan)? {
                "\nSecurity scan was interrupted by a process restart"
            } else {
                ""
            };
            let phase = scan::phase(&scan.id).map_or(String::new(), |phase| {
                format!("\nPhase: {}", phase.as_str())
            });
            Ok(format!(
                "Scan {}\nStatus: {}{phase}\nFindings: {}{note}",
                scan.id,
                scan.status.as_str(),
                scan.findings.len()
            ))
        }
        None => {
            let scans = store.scans()?;
            if scans.is_empty() {
                return Ok("No security scans.".to_owned());
            }
            let lines: Vec<String> = scans
                .iter()
                .map(|scan| {
                    format!(
                        "{}  {}  findings {}",
                        scan.id,
                        scan.status.as_str(),
                        scan.findings.len()
                    )
                })
                .collect();
            Ok(lines.join("\n"))
        }
    }
}

fn cancel_op(env: &Env, args: &Value) -> Result<String> {
    let id = required(args, "scan")?;
    if scan::request_cancel(&id) {
        return Ok(format!("Cancellation requested for security scan {id}."));
    }
    let store = open_store(env)?;
    if store.scan_claimed(&id) {
        store.request_cancel_marker(&id)?;
        return Ok(format!(
            "Cancellation requested for security scan {id}; it stops after its current batch."
        ));
    }
    // Check and mark under the index lock: a scan that finishes concurrently must not be
    // overwritten with a stale "running" document.
    let status = store.update_scan_with(&id, |scan| {
        if scan.status == contracts::ScanStatus::Running {
            scan.status = contracts::ScanStatus::Cancelled;
            scan.completed_at = Some(now_utc());
            return Ok(None);
        }
        Ok(Some(scan.status.as_str()))
    })?;
    match status {
        None => Ok(format!(
            "Security scan {id} was interrupted by a process restart; marked cancelled."
        )),
        Some(status) => bail!("Security scan {id} is {status}, not running."),
    }
}

fn validate_op(
    env: &Env,
    args: &Value,
    runner: Option<&mut dyn super::review::Reviewer>,
) -> Result<String> {
    let Some(runner) = runner else {
        bail!(
            "Finding validation runs the reviewer agent; run it from an interactive session with `/security validate`."
        );
    };
    let store = open_store(env)?;
    let scan_id = required(args, "scan")?;
    let finding_id = required(args, "finding")?;
    scan::validate(&store, &scan_id, &finding_id, runner)
}

fn workflow_fingerprint() -> String {
    format!(
        "dial-security-workflow/v1:sha256:{}",
        contracts::sha256_hex(scan::workflow_material().as_bytes())
    )
}

fn cloud_configs(env: &Env, args: &Value) -> Result<String> {
    let client = cloud_client(env, args)?;
    let configurations = client.all_configurations()?;
    if configurations.is_empty() {
        return Ok("No Codex Security cloud configurations.".to_owned());
    }
    let lines: Vec<String> = configurations
        .iter()
        .map(|c| {
            format!(
                "{}  {}  state {}  step {}",
                c.id,
                c.repository_url,
                c.state.as_deref().unwrap_or("unknown"),
                c.current_step.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

fn cloud_create(env: &Env, args: &Value) -> Result<String> {
    let client = cloud_client(env, args)?;
    let repository_url = match str_arg(args, "repositoryUrl") {
        Some(url) => url,
        None => {
            let root = repository_root(env)?;
            cloud::origin_remote(&root).context("No origin remote; pass repositoryUrl")?
        }
    };
    let request = cloud::StartRequest {
        repository_id: required(args, "repositoryId")?,
        repository_url,
        environment_id: required(args, "environmentId")?,
        lookback: str_arg(args, "lookback")
            .map(|raw| cloud::parse_lookback(&raw))
            .transpose()?,
        maintainer_attack_concerns: str_arg(args, "attackConcerns"),
        maintainer_focus_areas: str_arg(args, "focusAreas"),
        maintainer_additional_context: str_arg(args, "additionalContext"),
    };
    let created = client.create_configuration(&request)?;
    Ok(format!(
        "Created Codex Security cloud configuration {}\nRepository: {}\nState: {}",
        created.id,
        created.repository_url,
        created.state.as_deref().unwrap_or("unknown"),
    ))
}

fn cloud_status(env: &Env, args: &Value) -> Result<String> {
    let client = cloud_client(env, args)?;
    let configuration = client.configuration(&required(args, "configuration")?)?;
    let stats = client.stats(&configuration.id)?;
    Ok(format!(
        "Configuration {}\nPending commits: {}\nFinished commits: {}\nFailed commits: {}\nFindings: critical {}, high {}, medium {}, low {}, informational {}\nLast scanned commit: {}",
        configuration.id,
        stats.pending_commits,
        stats.finished_commits,
        stats.failed_commits,
        stats.finding_counts.critical,
        stats.finding_counts.high,
        stats.finding_counts.medium,
        stats.finding_counts.low,
        stats.finding_counts.informational,
        stats.last_scanned_commit.as_deref().unwrap_or("none"),
    ))
}

fn cloud_pull(env: &Env, args: &Value) -> Result<String> {
    let client = cloud_client(env, args)?;
    let store = open_store(env)?;
    let scan = cloud::pull(&client, &store, &required(args, "configuration")?)?;
    Ok(format!(
        "Imported Codex Security cloud scan {} with {} findings\nArtifacts: results.sarif, report.md",
        scan.id,
        scan.findings.len(),
    ))
}

fn scans(env: &Env) -> Result<String> {
    let store = open_store(env)?;
    let scans = store.scans()?;
    if scans.is_empty() {
        return Ok("No security scans.".to_owned());
    }
    let lines: Vec<String> = scans
        .iter()
        .map(|scan| {
            format!(
                "{}  {}  {}  {} findings  {}",
                scan.id,
                scan.status.as_str(),
                label(&scan.target_kind),
                scan.findings.len(),
                scan.created_at,
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

fn show(env: &Env, args: &Value) -> Result<String> {
    let scan = open_store(env)?.load_scan(&required(args, "scan")?)?;
    let mut out = format!(
        "Scan {}\nStatus: {}\nTarget: {}\nCreated: {}\nFindings: {}",
        scan.id,
        scan.status.as_str(),
        label(&scan.target_kind),
        scan.created_at,
        scan.findings.len(),
    );
    for finding in &scan.findings {
        out.push_str(&format!(
            "\n- {}  [{}] {}  ({}, {})",
            finding.id,
            label(&finding.severity),
            finding.title,
            label(&finding.disposition),
            finding
                .locations
                .first()
                .map(|l| l.path.as_str())
                .unwrap_or("no location"),
        ));
    }
    Ok(out)
}

fn disposition(env: &Env, args: &Value) -> Result<String> {
    let store = open_store(env)?;
    let scan_id = required(args, "scan")?;
    let finding_id = required(args, "finding")?;
    let status = required(args, "status")?;
    let disposition = Disposition::parse(&status)
        .with_context(|| format!("Unknown security disposition: {status}"))?;
    store.update_scan_with(&scan_id, |scan| {
        let finding = scan
            .findings
            .iter_mut()
            .find(|finding| finding.id == finding_id)
            .with_context(|| format!("Unknown security finding: {finding_id}"))?;
        finding.disposition = disposition;
        Ok(())
    })?;
    Ok(format!(
        "Set finding {finding_id} in scan {scan_id} to {}",
        disposition.as_str()
    ))
}

fn export(env: &Env, args: &Value) -> Result<String> {
    let scan = open_store(env)?.load_scan(&required(args, "scan")?)?;
    Ok(serde_json::to_string_pretty(&sarif::export(&scan))?)
}

fn import(env: &Env, args: &Value) -> Result<String> {
    let store = open_store(env)?;
    let raw = required(args, "path")?;
    let path = {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            env.project_root.join(path)
        }
    };
    let scan_id = contracts::new_scan_id();
    let created_at = now_utc();
    let repo_root = PathBuf::from(store.repository_root());
    let scan = if path.is_dir() {
        codex::import_bundle(
            &path,
            &repo_root,
            store.project_key(),
            &scan_id,
            Some(&created_at),
        )?
    } else {
        let text =
            fs::read_to_string(&path).with_context(|| format!("Cannot read {}", path.display()))?;
        let log = sarif::parse_log(&text)?;
        sarif::import(&log, &repo_root, store.project_key(), &scan_id, &created_at)?
    };
    store.save_scan(&scan)?;
    Ok(format!(
        "Imported scan {} with {} findings from {}",
        scan.id,
        scan.findings.len(),
        path.display(),
    ))
}

fn compare(env: &Env, args: &Value) -> Result<String> {
    let store = open_store(env)?;
    let before = store.load_scan(&required(args, "before")?)?;
    let after = store.load_scan(&required(args, "after")?)?;
    let result = comparison::compare_findings(&before.findings, &after.findings);
    let mut out = format!(
        "Matched {} (before {}, after {})\nBefore only: {}\nAfter only: {}",
        result.matched.len(),
        before.findings.len(),
        after.findings.len(),
        result.before_only.len(),
        result.after_only.len(),
    );
    for pair in &result.matched {
        out.push_str(&format!(
            "\nmatched ({}): {} -> {}",
            pair.method.as_str(),
            before.findings[pair.before].title,
            after.findings[pair.after].title,
        ));
    }
    for &index in &result.before_only {
        out.push_str(&format!("\nresolved: {}", before.findings[index].title));
    }
    for &index in &result.after_only {
        out.push_str(&format!("\nnew: {}", after.findings[index].title));
    }
    Ok(out)
}

fn lineage(env: &Env, args: &Value) -> Result<String> {
    let store = open_store(env)?;
    let before: Scan = store.load_scan(&required(args, "before")?)?;
    let after: Scan = store.load_scan(&required(args, "after")?)?;
    let entries = comparison::lineage(&before, &after)?;
    if entries.is_empty() {
        return Ok("No findings in either scan.".to_owned());
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|e| format!("{}  {}  {}", e.status, e.fingerprint, e.title))
        .collect();
    Ok(lines.join("\n"))
}
