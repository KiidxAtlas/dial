//! `github` op `run_watch`. Read-only: it polls GitHub Actions
//! through `gh api`, optionally pauses to collect concurrent failures, and returns failed-job logs.

use std::collections::HashMap;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};

use super::gh_common::{GH_TIMEOUT, default_gh_host, gh, gh_json, strings, valid_repo};
use crate::tools::{ToolOutput, Workspace, opt_str};

/// Poll cadence for the first minute of a watch, then the slower cadence.
const INTERVAL_FAST: Duration = Duration::from_secs(3);
const INTERVAL_SLOW: Duration = Duration::from_secs(15);
const FAST_WINDOW: Duration = Duration::from_secs(60);
/// A commit with no workflow runs after this long is reported instead of polled forever.
const NO_RUNS_GIVE_UP: Duration = Duration::from_secs(90);
const MAX_POLL_FAILURES: u32 = 5;
/// Pause after a failure is seen so concurrent failures land in the same report.
const GRACE: Duration = Duration::from_secs(5);
const TAIL_DEFAULT: usize = 15;
const TAIL_MAX: usize = 200;
const JOBS_PAGE_SIZE: usize = 100;
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Granularity at which waits notice cancellation.
const WAIT_SLICE: Duration = Duration::from_millis(50);
const CANCELLED: &str = "[cancelled]";
const RUN_REF_ERROR: &str =
    "run must be a numeric workflow run ID or a full GitHub Actions run URL";

const RUN_SUCCESS: &[&str] = &["success", "neutral", "skipped"];
const RUN_FAILURE: &[&str] = &[
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
];
const JOB_FAILURE: &[&str] = &["failure", "timed_out", "cancelled", "action_required"];

/// Run `op`, reporting each intermediate snapshot through `progress`.
pub(super) fn run_with_progress(
    op: &str,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<ToolOutput, String> {
    if op != "run_watch" {
        return Err(format!("unsupported run_watch op: {op}"));
    }
    match watch(ws, args, cancel, progress) {
        Err(_) if cancel.load(Ordering::Relaxed) => Err(CANCELLED.to_owned()),
        result => result,
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct RunRef {
    repo: Option<String>,
    run_id: Option<u64>,
}

/// A numeric run ID, or a full Actions run URL (which also names the repo).
fn parse_run_reference(value: Option<&str>) -> Result<RunRef, String> {
    let Some(run) = value.map(str::trim).filter(|run| !run.is_empty()) else {
        return Ok(RunRef::default());
    };
    if run.bytes().all(|byte| byte.is_ascii_digit()) {
        let id = run.parse::<u64>().map_err(|_| RUN_REF_ERROR.to_owned())?;
        return Ok(RunRef {
            repo: None,
            run_id: Some(id),
        });
    }
    let captures = run_url_pattern()
        .captures(run)
        .ok_or_else(|| RUN_REF_ERROR.to_owned())?;
    let id = captures[3]
        .parse::<u64>()
        .map_err(|_| RUN_REF_ERROR.to_owned())?;
    Ok(RunRef {
        repo: Some(format!("{}/{}", &captures[1], &captures[2])),
        run_id: Some(id),
    })
}

fn run_url_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^https://([^/]+)/([^/]+/[^/]+)/actions/runs/(\d+)(?:/.*)?$")
            .expect("static pattern")
    })
}

/// Absent means the default; otherwise a positive finite number, floored and capped.
fn resolve_tail_limit(args: &Value) -> Result<usize, String> {
    match args.get("tail") {
        None | Some(Value::Null) => Ok(TAIL_DEFAULT),
        Some(value) => match value.as_f64() {
            Some(number) if number.is_finite() && number > 0.0 => {
                Ok((number.floor().max(1.0) as usize).min(TAIL_MAX))
            }
            _ => Err("tail must be a positive number".to_owned()),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Repository identity (parseRepoRef, repoFromUrl, githubRepoSlugEquals)
// ---------------------------------------------------------------------------------------------

struct RepoRef<'a> {
    host: Option<&'a str>,
    slug: &'a str,
}

/// `[HOST/]OWNER/REPO`; anything with another shape is taken as a bare slug.
fn repo_ref(repo: &str) -> RepoRef<'_> {
    let Some(first) = repo.find('/') else {
        return RepoRef {
            host: None,
            slug: repo,
        };
    };
    let after = first + 1;
    match repo[after..].find('/') {
        Some(offset) if !repo[after + offset + 1..].contains('/') => RepoRef {
            host: Some(&repo[..first]),
            slug: &repo[after..],
        },
        _ => RepoRef {
            host: None,
            slug: repo,
        },
    }
}

/// `gh api` endpoints carry no host, so a known host is passed with `--hostname`.
fn api_args(repo: &RepoRef<'_>) -> Vec<String> {
    let mut args = strings(&["api"]);
    if let Some(host) = repo.host {
        args.extend(strings(&["--hostname"]));
        args.push(host.to_owned());
    }
    args
}

fn validate_host(host: &str) -> Result<(), String> {
    let valid = !host.is_empty()
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(format!("invalid GitHub host: {host}"))
    }
}

/// Validate an explicit `[HOST/]OWNER/REPO` before it reaches a `gh` argv.
fn checked_repo(repo: &str) -> Result<String, String> {
    let parsed = repo_ref(repo);
    if let Some(host) = parsed.host {
        validate_host(host)?;
    }
    valid_repo(parsed.slug)?;
    Ok(repo.to_owned())
}

fn effective_host(repo: &RepoRef<'_>) -> String {
    repo.host
        .map_or_else(default_gh_host, |host| host.to_ascii_lowercase())
}

/// Case-insensitive comparison over the instance each ref resolves to.
fn slug_equals(left: Option<&str>, right: &str) -> bool {
    let Some(left) = left else {
        return false;
    };
    let (left, right) = (repo_ref(left), repo_ref(right));
    effective_host(&left) == effective_host(&right) && left.slug.eq_ignore_ascii_case(right.slug)
}

/// `https://HOST/OWNER/REPO` to a repo identity; the host is dropped only when it is `gh`'s default.
fn repo_from_url(value: &str) -> Option<String> {
    let captures = repo_url_pattern().captures(value.trim())?;
    let host = captures[1].to_ascii_lowercase();
    let slug = format!("{}/{}", &captures[2], &captures[3]);
    Some(if host == default_gh_host() {
        slug
    } else {
        format!("{host}/{slug}")
    })
}

fn repo_url_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"^https?://([^/]+)/([^/]+)/([^/?#]+)").expect("static pattern"))
}

/// The checkout's repo, via its URL so the host is preserved.
fn resolve_repo_from_cwd(ws: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    let url = gh(
        ws,
        &strings(&["repo", "view", "--json", "url", "-q", ".url"]),
        GH_TIMEOUT,
        cancel,
    )?;
    let url = url.trim();
    if url.is_empty() {
        return Err("repo must not be empty".to_owned());
    }
    repo_from_url(url)
        .ok_or_else(|| format!("GitHub CLI returned an unrecognized repository URL: {url}"))
}

/// Explicit repo, else the run URL's repo, else the checkout's repo.
fn resolve_watch_repo(
    ws: &Workspace,
    explicit: Option<&str>,
    run_repo: Option<&str>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if let (Some(explicit), Some(run_repo)) = (explicit, run_repo)
        && !slug_equals(Some(explicit), run_repo)
    {
        return Err("run URL repository does not match the provided repo".to_owned());
    }
    if let Some(explicit) = explicit {
        return checked_repo(explicit);
    }
    if let Some(run_repo) = run_repo {
        return checked_repo(run_repo);
    }
    resolve_repo_from_cwd(ws, cancel)
}

/// JavaScript `encodeURIComponent`.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Local git (requireCurrentGitBranch / requireCurrentGitHead)
// ---------------------------------------------------------------------------------------------

/// Run `git` in the workspace; `None` when it fails, prints nothing, or times out.
fn git_stdout(
    ws: &Workspace,
    args: &[&str],
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    check_cancel(cancel)?;
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(ws.root())
        .env("GIT_TERMINAL_PROMPT", "0");
    let mut policy = crate::runtime::process::Policy::bounded(GIT_TIMEOUT, 1024 * 1024);
    policy.may_mutate = false;
    let output =
        crate::runtime::process::run_with_policy(command, None, policy, cancel, &mut |_, _| {})?;
    if output.cancelled {
        return Err(CANCELLED.to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((output.success() && !text.is_empty()).then_some(text))
}

fn require_current_git_branch(ws: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    git_stdout(ws, &["symbolic-ref", "--quiet", "--short", "HEAD"], cancel)?.ok_or_else(|| {
        "Current git branch is unavailable. Pass `branch` or `run` explicitly.".to_owned()
    })
}

fn require_current_git_head(ws: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    git_stdout(ws, &["rev-parse", "--verify", "--quiet", "HEAD"], cancel)?
        .ok_or_else(|| "Current git HEAD is unavailable. Pass `run` explicitly.".to_owned())
}

// ---------------------------------------------------------------------------------------------
// Models (GhRunJobSnapshot / GhRunSnapshot / GhFailedJobLog)
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Job {
    id: u64,
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    url: Option<String>,
}

#[derive(Clone)]
struct Run {
    id: u64,
    workflow_name: Option<String>,
    display_title: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    branch: Option<String>,
    head_sha: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    url: Option<String>,
    jobs: Vec<Job>,
}

struct FailedLog {
    run: Run,
    job: Job,
    /// Normalized full log; `None` when GitHub returned no log.
    full: Option<String>,
    /// Last `tail` lines of `full`.
    tail: Option<String>,
}

#[derive(Clone, Copy)]
enum LogMode {
    Tail(usize),
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Success,
    Failure,
    Pending,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Pending => "pending",
        }
    }
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Drops jobs without a numeric ID.
fn job_from_json(value: &Value) -> Option<Job> {
    let id = value.get("id").and_then(Value::as_u64)?;
    Some(Job {
        id,
        name: text_field(value, "name").unwrap_or_else(|| format!("job-{id}")),
        status: text_field(value, "status"),
        conclusion: text_field(value, "conclusion"),
        started_at: text_field(value, "started_at"),
        completed_at: text_field(value, "completed_at"),
        url: text_field(value, "html_url"),
    })
}

/// Normalizes a run snapshot.
fn run_from_json(value: &Value, jobs: Vec<Job>) -> Result<Run, String> {
    let id = value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "GitHub Actions run response did not include a run ID.".to_owned())?;
    Ok(Run {
        id,
        workflow_name: text_field(value, "name"),
        display_title: text_field(value, "display_title"),
        status: text_field(value, "status"),
        conclusion: text_field(value, "conclusion"),
        branch: text_field(value, "head_branch"),
        head_sha: text_field(value, "head_sha"),
        created_at: text_field(value, "created_at"),
        updated_at: text_field(value, "updated_at"),
        url: text_field(value, "html_url"),
        jobs,
    })
}

fn is_failed_job(job: &Job) -> bool {
    job.conclusion
        .as_deref()
        .is_some_and(|conclusion| JOB_FAILURE.contains(&conclusion))
}

fn failed_jobs(jobs: &[Job]) -> Vec<Job> {
    jobs.iter()
        .filter(|job| is_failed_job(job))
        .cloned()
        .collect()
}

fn failed_pairs(runs: &[Run]) -> Vec<(Run, Job)> {
    runs.iter()
        .flat_map(|run| {
            run.jobs
                .iter()
                .filter(|job| is_failed_job(job))
                .map(move |job| (run.clone(), job.clone()))
        })
        .collect()
}

fn conclusion_outcome(conclusion: Option<&str>) -> Outcome {
    match conclusion {
        Some(value) if RUN_SUCCESS.contains(&value) => Outcome::Success,
        Some(value) if RUN_FAILURE.contains(&value) => Outcome::Failure,
        _ => Outcome::Pending,
    }
}

fn run_outcome(run: &Run) -> Outcome {
    if run.status.as_deref() != Some("completed") {
        return Outcome::Pending;
    }
    conclusion_outcome(run.conclusion.as_deref())
}

/// Any failed job or failed run is a failure; otherwise pending until every run settles.
fn collection_outcome(runs: &[Run]) -> Outcome {
    if runs.is_empty() {
        return Outcome::Pending;
    }
    let mut pending = false;
    for run in runs {
        if run.jobs.iter().any(is_failed_job) {
            return Outcome::Failure;
        }
        match run_outcome(run) {
            Outcome::Failure => return Outcome::Failure,
            Outcome::Pending => pending = true,
            Outcome::Success => {}
        }
    }
    if pending {
        Outcome::Pending
    } else {
        Outcome::Success
    }
}

/// Sorted run IDs, used to detect new runs between polls.
fn run_signature(runs: &[Run]) -> String {
    let mut ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
    ids.sort_unstable();
    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

// ---------------------------------------------------------------------------------------------
// gh fetches
// ---------------------------------------------------------------------------------------------

fn fetch_run_jobs(
    ws: &Workspace,
    repo: &str,
    run_id: u64,
    cancel: &AtomicBool,
) -> Result<Vec<Job>, String> {
    let repo = repo_ref(repo);
    let mut jobs = Vec::new();
    let mut page = 1u64;
    loop {
        let mut args = api_args(&repo);
        args.extend(strings(&["--method", "GET"]));
        args.push(format!("/repos/{}/actions/runs/{run_id}/jobs", repo.slug));
        args.extend(["-F".to_owned(), format!("per_page={JOBS_PAGE_SIZE}")]);
        args.extend(["-F".to_owned(), format!("page={page}")]);
        let response = gh_json(ws, &args, GH_TIMEOUT, cancel)?;
        let empty: Vec<Value> = Vec::new();
        let raw = response
            .get("jobs")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        jobs.extend(raw.iter().filter_map(job_from_json));
        // The raw page length decides pagination: a malformed job must not end it early.
        if raw.len() < JOBS_PAGE_SIZE {
            break;
        }
        let total = response
            .get("total_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if total <= jobs.len() as u64 {
            break;
        }
        page += 1;
    }
    Ok(jobs)
}

fn fetch_run_snapshot(
    ws: &Workspace,
    repo: &str,
    run_id: u64,
    cancel: &AtomicBool,
) -> Result<Run, String> {
    let parsed = repo_ref(repo);
    let mut args = api_args(&parsed);
    args.extend(strings(&["--method", "GET"]));
    args.push(format!("/repos/{}/actions/runs/{run_id}", parsed.slug));
    let value = gh_json(ws, &args, GH_TIMEOUT, cancel)?;
    let jobs = fetch_run_jobs(ws, repo, run_id, cancel)?;
    run_from_json(&value, jobs)
}

/// Runs for `head` (a commit SHA). Completed runs reuse cached jobs; a run seen in progress evicts its entry,
/// so a re-run's jobs are never served from the first attempt.
fn fetch_runs_for_commit(
    ws: &Workspace,
    repo: &str,
    head: &str,
    cancel: &AtomicBool,
    cache: &mut HashMap<u64, Vec<Job>>,
) -> Result<Vec<Run>, String> {
    let parsed = repo_ref(repo);
    let mut args = api_args(&parsed);
    args.extend(strings(&["--method", "GET"]));
    args.push(format!("/repos/{}/actions/runs", parsed.slug));
    args.extend(["-F".to_owned(), format!("head_sha={head}")]);
    args.extend(["-F".to_owned(), format!("per_page={JOBS_PAGE_SIZE}")]);
    let value = gh_json(ws, &args, GH_TIMEOUT, cancel)?;
    let empty: Vec<Value> = Vec::new();
    let items = value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut runs = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let completed = text_field(item, "status").as_deref() == Some("completed");
        if !completed {
            cache.remove(&id);
        }
        let cached = if completed {
            cache.get(&id).cloned()
        } else {
            None
        };
        let jobs = match cached {
            Some(jobs) => jobs,
            None => {
                let fetched = fetch_run_jobs(ws, repo, id, cancel)?;
                if completed {
                    cache.insert(id, fetched.clone());
                }
                fetched
            }
        };
        runs.push(run_from_json(item, jobs)?);
    }
    Ok(runs)
}

fn resolve_branch_head(
    ws: &Workspace,
    repo: &str,
    branch: &str,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let parsed = repo_ref(repo);
    let mut args = api_args(&parsed);
    args.extend(strings(&["--method", "GET"]));
    args.push(format!(
        "/repos/{}/branches/{}",
        parsed.slug,
        encode_uri_component(branch)
    ));
    let value = gh_json(ws, &args, GH_TIMEOUT, cancel)?;
    value
        .get("commit")
        .and_then(|commit| text_field(commit, "sha"))
        .ok_or_else(|| format!("head SHA for branch {branch} must not be empty"))
}

/// Normalizes a block: LF line endings, tabs as four spaces, trailing whitespace removed.
fn normalize_block(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .trim_end()
        .to_owned()
}

/// Keeps the last `tail` lines. A tail of zero keeps every line, as JavaScript `slice(-0)` does.
fn tail_log_lines(log: &str, tail: usize) -> Option<String> {
    let normalized = normalize_block(log);
    if normalized.is_empty() {
        return None;
    }
    let lines: Vec<&str> = normalized.split('\n').collect();
    let start = if tail == 0 {
        0
    } else {
        lines.len().saturating_sub(tail)
    };
    Some(lines[start..].join("\n").trim_end().to_owned())
}

/// Fetch each failed job's log. A failed fetch leaves the log unavailable rather than failing the watch.
fn fetch_failed_job_logs(
    ws: &Workspace,
    repo: &str,
    pairs: Vec<(Run, Job)>,
    tail: usize,
    cancel: &AtomicBool,
) -> Result<Vec<FailedLog>, String> {
    let parsed = repo_ref(repo);
    let mut logs = Vec::with_capacity(pairs.len());
    for (run, job) in pairs {
        let mut args = api_args(&parsed);
        args.push(format!(
            "/repos/{}/actions/jobs/{}/logs",
            parsed.slug, job.id
        ));
        let full = match gh(ws, &args, GH_TIMEOUT, cancel) {
            Ok(stdout) => Some(normalize_block(&stdout)).filter(|log| !log.is_empty()),
            Err(_) => {
                check_cancel(cancel)?;
                None
            }
        };
        let tail_text = full.as_deref().and_then(|log| tail_log_lines(log, tail));
        logs.push(FailedLog {
            run,
            job,
            full,
            tail: tail_text,
        });
    }
    Ok(logs)
}

// ---------------------------------------------------------------------------------------------
// Waiting and cancellation
// ---------------------------------------------------------------------------------------------

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

/// Sleep for `duration`, waking in short slices so cancellation takes effect promptly.
fn wait_for(duration: Duration, cancel: &AtomicBool) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    loop {
        check_cancel(cancel)?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        sleep((deadline - now).min(WAIT_SLICE));
    }
}

fn current_interval(started: Instant) -> Duration {
    if started.elapsed() < FAST_WINDOW {
        INTERVAL_FAST
    } else {
        INTERVAL_SLOW
    }
}

/// Rate limits are retried; anything else ends the watch.
fn is_rate_limited(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("rate limit") || lower.contains("http 429") || lower.contains("abuse detection")
}

/// Rate limits back off on the slow interval, up to `MAX_POLL_FAILURES` in a row.
fn handle_poll_error(err: String, cancel: &AtomicBool, failures: &mut u32) -> Result<(), String> {
    check_cancel(cancel)?;
    *failures += 1;
    if !is_rate_limited(&err) || *failures > MAX_POLL_FAILURES {
        return Err(err);
    }
    wait_for(INTERVAL_SLOW, cancel)
}

fn failure_note() -> String {
    format!(
        "Failure detected. Waiting {}s to capture concurrent failures before fetching logs.",
        GRACE.as_secs()
    )
}

// ---------------------------------------------------------------------------------------------
// Timestamps (parseTimestampMs / getJobDurationSeconds)
// ---------------------------------------------------------------------------------------------

fn timestamp_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?(Z|[+-]\d{2}:\d{2})?$",
        )
        .expect("static pattern")
    })
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parse GitHub's RFC 3339 timestamps into epoch milliseconds.
fn parse_timestamp_ms(value: &str) -> Option<i64> {
    let captures = timestamp_pattern().captures(value.trim())?;
    let number = |index: usize| {
        captures
            .get(index)
            .and_then(|m| m.as_str().parse::<i64>().ok())
    };
    let (year, month, day) = (number(1)?, number(2)?, number(3)?);
    let (hour, minute, second) = (number(4)?, number(5)?, number(6)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis = captures.get(7).map_or(0, |fraction| {
        fraction
            .as_str()
            .chars()
            .chain(std::iter::repeat('0'))
            .take(3)
            .collect::<String>()
            .parse::<i64>()
            .unwrap_or(0)
    });
    let offset_seconds = match captures.get(8).map(|zone| zone.as_str()) {
        None | Some("Z") => 0,
        Some(zone) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[4..6].parse().ok()?;
            sign * (hours * 3_600 + minutes * 60)
        }
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some((seconds - offset_seconds) * 1000 + millis)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn job_duration_seconds(job: &Job, observed_ms: i64) -> Option<i64> {
    let started = parse_timestamp_ms(job.started_at.as_deref()?)?;
    let completed = job
        .completed_at
        .as_deref()
        .and_then(parse_timestamp_ms)
        .unwrap_or(observed_ms);
    Some((completed - started).div_euclid(1000).max(0))
}

// ---------------------------------------------------------------------------------------------
// Text rendering (pushLine / formatShortSha)
// ---------------------------------------------------------------------------------------------

/// Skips absent and empty values.
fn push_line(lines: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        lines.push(format!("{label}: {value}"));
    }
}

/// First 12 characters of a commit SHA.
fn short_sha(sha: &str) -> &str {
    match sha.char_indices().nth(12) {
        Some((index, _)) => &sha[..index],
        None => sha,
    }
}

fn job_state(job: &Job) -> &str {
    job.conclusion
        .as_deref()
        .or(job.status.as_deref())
        .unwrap_or("unknown")
}

fn render_jobs_section(jobs: &[Job]) -> Vec<String> {
    if jobs.is_empty() {
        return vec![
            "## Jobs".to_owned(),
            String::new(),
            "No jobs reported yet.".to_owned(),
        ];
    }
    let mut lines = vec![format!("## Jobs ({})", jobs.len()), String::new()];
    for job in jobs {
        lines.push(format!("- [{}] {}", job_state(job), job.name));
        push_line(&mut lines, "  Started", job.started_at.as_deref());
        push_line(&mut lines, "  Completed", job.completed_at.as_deref());
        push_line(&mut lines, "  URL", job.url.as_deref());
    }
    lines
}

fn render_failed_job_logs(logs: &[FailedLog], mode: LogMode) -> Vec<String> {
    if logs.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["## Failed Jobs".to_owned(), String::new()];
    for entry in logs {
        lines.push(format!(
            "### {} [{}]",
            entry.job.name,
            entry.job.conclusion.as_deref().unwrap_or("failed")
        ));
        push_line(&mut lines, "Run", Some(&format!("#{}", entry.run.id)));
        push_line(&mut lines, "Workflow", entry.run.workflow_name.as_deref());
        push_line(&mut lines, "Started", entry.job.started_at.as_deref());
        push_line(&mut lines, "Completed", entry.job.completed_at.as_deref());
        push_line(&mut lines, "URL", entry.job.url.as_deref());
        lines.push(String::new());
        let (heading, text, unavailable) = match mode {
            LogMode::Full => (
                "Full log:".to_owned(),
                entry.full.as_deref(),
                "Full log unavailable.",
            ),
            LogMode::Tail(tail) => (
                format!("Last {tail} log lines:"),
                entry.tail.as_deref(),
                "Log tail unavailable.",
            ),
        };
        match text {
            Some(text) => {
                lines.push(heading);
                lines.push("```text".to_owned());
                lines.push(text.to_owned());
                lines.push("```".to_owned());
            }
            None => lines.push(unavailable.to_owned()),
        }
        lines.push(String::new());
    }
    lines
}

fn push_run_fields(lines: &mut Vec<String>, repo: &str, run: &Run) {
    push_line(lines, "Repository", Some(repo));
    push_line(lines, "Workflow", run.workflow_name.as_deref());
    push_line(lines, "Title", run.display_title.as_deref());
    push_line(lines, "Branch", run.branch.as_deref());
    push_line(lines, "Status", run.status.as_deref());
    push_line(lines, "Conclusion", run.conclusion.as_deref());
    push_line(lines, "Created", run.created_at.as_deref());
    push_line(lines, "Updated", run.updated_at.as_deref());
    push_line(lines, "URL", run.url.as_deref());
}

fn format_run_snapshot(repo: &str, run: &Run, poll: u64, note: Option<&str>) -> String {
    let failed = failed_jobs(&run.jobs).len();
    let mut lines = vec![
        format!("# Watching GitHub Actions Run #{}", run.id),
        String::new(),
    ];
    push_run_fields(&mut lines, repo, run);
    push_line(&mut lines, "Poll", Some(&poll.to_string()));
    let failed_text = (failed > 0).then(|| failed.to_string());
    push_line(&mut lines, "Failed jobs", failed_text.as_deref());
    if let Some(note) = note {
        lines.push(String::new());
        lines.push(format!("Note: {note}"));
    }
    lines.push(String::new());
    lines.extend(render_jobs_section(&run.jobs));
    lines.join("\n").trim().to_owned()
}

fn format_run_result(repo: &str, run: &Run, logs: &[FailedLog], mode: LogMode) -> String {
    let mut lines = vec![format!("# GitHub Actions Run #{}", run.id), String::new()];
    push_run_fields(&mut lines, repo, run);
    lines.push(String::new());
    lines.extend(render_jobs_section(&run.jobs));
    if run.jobs.iter().any(is_failed_job) {
        lines.push(String::new());
        lines.extend(render_failed_job_logs(logs, mode));
        lines.push("Run failed.".to_owned());
    } else if conclusion_outcome(run.conclusion.as_deref()) == Outcome::Success {
        lines.push(String::new());
        lines.push("All jobs passed.".to_owned());
    } else {
        lines.push(String::new());
        lines.push(
            "Run completed without successful jobs, but no failed job logs were available."
                .to_owned(),
        );
    }
    lines.join("\n").trim().to_owned()
}

fn render_run_section(run: &Run) -> Vec<String> {
    let label = match run.workflow_name.as_deref() {
        Some(name) => format!("### Run #{} - {name}", run.id),
        None => format!("### Run #{}", run.id),
    };
    let mut lines = vec![label, String::new()];
    push_line(&mut lines, "Title", run.display_title.as_deref());
    push_line(&mut lines, "Branch", run.branch.as_deref());
    push_line(&mut lines, "Commit", run.head_sha.as_deref().map(short_sha));
    push_line(&mut lines, "Status", run.status.as_deref());
    push_line(&mut lines, "Conclusion", run.conclusion.as_deref());
    push_line(&mut lines, "Created", run.created_at.as_deref());
    push_line(&mut lines, "Updated", run.updated_at.as_deref());
    push_line(&mut lines, "URL", run.url.as_deref());
    lines.push(String::new());
    lines.extend(render_jobs_section(&run.jobs));
    lines
}

fn format_commit_snapshot(
    repo: &str,
    head: &str,
    branch: &str,
    runs: &[Run],
    poll: u64,
    note: Option<&str>,
) -> String {
    let failed: usize = runs
        .iter()
        .map(|run| run.jobs.iter().filter(|job| is_failed_job(job)).count())
        .sum();
    let completed = runs
        .iter()
        .filter(|run| run.status.as_deref() == Some("completed"))
        .count();
    let mut lines = vec![
        format!("# Watching GitHub Actions for {}", short_sha(head)),
        String::new(),
    ];
    push_line(&mut lines, "Repository", Some(repo));
    push_line(&mut lines, "Branch", Some(branch));
    push_line(&mut lines, "Commit", Some(head));
    push_line(&mut lines, "Poll", Some(&poll.to_string()));
    push_line(&mut lines, "Runs", Some(&runs.len().to_string()));
    push_line(
        &mut lines,
        "Completed runs",
        Some(&format!("{completed}/{}", runs.len())),
    );
    let failed_text = (failed > 0).then(|| failed.to_string());
    push_line(&mut lines, "Failed jobs", failed_text.as_deref());
    if let Some(note) = note {
        lines.push(String::new());
        lines.push(format!("Note: {note}"));
    }
    if runs.is_empty() {
        lines.push(String::new());
        lines.push("Waiting for workflow runs for this commit.".to_owned());
        return lines.join("\n").trim().to_owned();
    }
    for run in runs {
        lines.push(String::new());
        lines.extend(render_run_section(run));
    }
    lines.join("\n").trim().to_owned()
}

fn format_commit_result(
    repo: &str,
    head: &str,
    branch: &str,
    runs: &[Run],
    logs: &[FailedLog],
    mode: LogMode,
) -> String {
    let mut lines = vec![
        format!("# GitHub Actions for {}", short_sha(head)),
        String::new(),
    ];
    push_line(&mut lines, "Repository", Some(repo));
    push_line(&mut lines, "Branch", Some(branch));
    push_line(&mut lines, "Commit", Some(head));
    push_line(&mut lines, "Runs", Some(&runs.len().to_string()));
    for run in runs {
        lines.push(String::new());
        lines.extend(render_run_section(run));
    }
    if !logs.is_empty() {
        lines.push(String::new());
        lines.extend(render_failed_job_logs(logs, mode));
        lines.push("Workflow runs for this commit failed.".to_owned());
    } else if collection_outcome(runs) == Outcome::Success {
        lines.push(String::new());
        lines.push("All workflow runs for this commit passed.".to_owned());
    } else {
        lines.push(String::new());
        lines.push("Workflow runs for this commit did not complete successfully.".to_owned());
    }
    lines.join("\n").trim().to_owned()
}

// ---------------------------------------------------------------------------------------------
// Structured details (GhToolDetails and its watch sub-objects)
// ---------------------------------------------------------------------------------------------

/// Object with absent (`None`) entries omitted, as JSON `undefined` is.
fn object(entries: Vec<(&str, Option<Value>)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
            .collect::<Map<String, Value>>(),
    )
}

fn opt_text(value: Option<&str>) -> Option<Value> {
    value.map(|text| Value::String(text.to_owned()))
}

fn job_details(job: &Job, observed_ms: i64) -> Value {
    object(vec![
        ("id", Some(json!(job.id))),
        ("name", Some(json!(job.name))),
        ("status", opt_text(job.status.as_deref())),
        ("conclusion", opt_text(job.conclusion.as_deref())),
        (
            "durationSeconds",
            job_duration_seconds(job, observed_ms).map(|seconds| json!(seconds)),
        ),
        ("url", opt_text(job.url.as_deref())),
    ])
}

fn run_details(run: &Run, observed_ms: i64) -> Value {
    object(vec![
        ("id", Some(json!(run.id))),
        ("workflowName", opt_text(run.workflow_name.as_deref())),
        ("displayTitle", opt_text(run.display_title.as_deref())),
        ("status", opt_text(run.status.as_deref())),
        ("conclusion", opt_text(run.conclusion.as_deref())),
        ("branch", opt_text(run.branch.as_deref())),
        ("headSha", opt_text(run.head_sha.as_deref())),
        ("url", opt_text(run.url.as_deref())),
        (
            "jobs",
            Some(Value::Array(
                run.jobs
                    .iter()
                    .map(|job| job_details(job, observed_ms))
                    .collect(),
            )),
        ),
    ])
}

fn failed_log_details(logs: &[FailedLog]) -> Value {
    Value::Array(
        logs.iter()
            .map(|entry| {
                object(vec![
                    ("runId", Some(json!(entry.run.id))),
                    ("workflowName", opt_text(entry.run.workflow_name.as_deref())),
                    ("jobName", Some(json!(entry.job.name))),
                    ("conclusion", opt_text(entry.job.conclusion.as_deref())),
                    ("tail", opt_text(entry.tail.as_deref())),
                    ("available", Some(json!(entry.full.is_some()))),
                ])
            })
            .collect(),
    )
}

fn run_result_details(repo: &str, run: &Run, logs: &[FailedLog], observed_ms: i64) -> Value {
    let failed_names: Vec<Value> = run
        .jobs
        .iter()
        .filter(|job| is_failed_job(job))
        .map(|job| json!(job.name))
        .collect();
    object(vec![
        ("repo", opt_text(Some(repo))),
        ("branch", opt_text(run.branch.as_deref())),
        ("headSha", opt_text(run.head_sha.as_deref())),
        ("runId", Some(json!(run.id))),
        ("runIds", Some(json!([run.id]))),
        ("status", opt_text(run.status.as_deref())),
        ("conclusion", opt_text(run.conclusion.as_deref())),
        ("failedJobs", Some(Value::Array(failed_names))),
        (
            "watch",
            Some(object(vec![
                ("mode", opt_text(Some("run"))),
                ("state", opt_text(Some("completed"))),
                ("repo", opt_text(Some(repo))),
                ("branch", opt_text(run.branch.as_deref())),
                ("headSha", opt_text(run.head_sha.as_deref())),
                ("run", Some(run_details(run, observed_ms))),
                ("failedLogs", Some(failed_log_details(logs))),
            ])),
        ),
    ])
}

fn commit_result_details(
    repo: &str,
    head: &str,
    branch: &str,
    runs: &[Run],
    logs: &[FailedLog],
    poll_count: Option<u64>,
    observed_ms: i64,
) -> Value {
    let all_completed = !runs.is_empty()
        && runs
            .iter()
            .all(|run| run.status.as_deref() == Some("completed"));
    let failed_names: Vec<Value> = runs
        .iter()
        .flat_map(|run| {
            let workflow = run
                .workflow_name
                .as_deref()
                .map_or_else(|| format!("run {}", run.id), str::to_owned);
            run.jobs
                .iter()
                .filter(|job| is_failed_job(job))
                .map(move |job| json!(format!("{workflow}: {}", job.name)))
        })
        .collect();
    let run_ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
    object(vec![
        ("repo", opt_text(Some(repo))),
        ("branch", opt_text(Some(branch))),
        ("headSha", opt_text(Some(head))),
        ("runIds", Some(json!(run_ids))),
        (
            "status",
            opt_text(Some(if all_completed {
                "completed"
            } else {
                "in_progress"
            })),
        ),
        (
            "conclusion",
            opt_text(Some(collection_outcome(runs).name())),
        ),
        ("failedJobs", Some(Value::Array(failed_names))),
        (
            "watch",
            Some(object(vec![
                ("mode", opt_text(Some("commit"))),
                ("state", opt_text(Some("completed"))),
                ("repo", opt_text(Some(repo))),
                ("branch", opt_text(Some(branch))),
                ("headSha", opt_text(Some(head))),
                ("pollCount", poll_count.map(|count| json!(count))),
                (
                    "runs",
                    Some(Value::Array(
                        runs.iter()
                            .map(|run| run_details(run, observed_ms))
                            .collect(),
                    )),
                ),
                ("failedLogs", Some(failed_log_details(logs))),
            ])),
        ),
    ])
}

/// Attach `artifactId` and `sourceUrl` to the details object.
fn finish(
    text: String,
    mut data: Value,
    artifact_id: Option<String>,
    source_url: Option<&str>,
) -> ToolOutput {
    if let Value::Object(map) = &mut data {
        if let Some(id) = artifact_id {
            map.insert("artifactId".to_owned(), Value::String(id));
        }
        if let Some(url) = source_url {
            map.insert("sourceUrl".to_owned(), Value::String(url.to_owned()));
        }
    }
    ToolOutput {
        execution: Default::default(),
        text,
        is_error: false,
        data: Some(data),
    }
}

/// Save the complete text as an artifact; `None` when the workspace has no artifact store.
fn save_artifact(ws: &Workspace, text: &str) -> Result<Option<String>, String> {
    match ws.artifacts() {
        None => Ok(None),
        Some(store) => store
            .retain(text)
            .map(Some)
            .map_err(|error| format!("{error:#}")),
    }
}

/// The bare id of an `artifact://` URI.
fn bare_artifact_id(uri: &str) -> &str {
    uri.strip_prefix("artifact://").unwrap_or(uri)
}

fn append_artifact_reference(text: &mut String, uri: Option<&str>, label: &str) {
    if let Some(uri) = uri {
        *text = format!("{text}\n\n{label}: {uri}");
    }
}

// ---------------------------------------------------------------------------------------------
// Watch loops (executeRunWatch)
// ---------------------------------------------------------------------------------------------

fn watch(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<ToolOutput, String> {
    check_cancel(cancel)?;
    let branch_input = opt_str(args, "branch")?
        .map(str::trim)
        .filter(|branch| !branch.is_empty());
    let explicit_repo = opt_str(args, "repo")?
        .map(str::trim)
        .filter(|repo| !repo.is_empty());
    let run_ref = parse_run_reference(opt_str(args, "run")?)?;
    let repo = resolve_watch_repo(ws, explicit_repo, run_ref.repo.as_deref(), cancel)?;
    let tail = resolve_tail_limit(args)?;
    let started = Instant::now();
    match run_ref.run_id {
        Some(run_id) => watch_run(ws, &repo, run_id, tail, cancel, started, progress),
        None => watch_commit(ws, &repo, branch_input, tail, cancel, started, progress),
    }
}

fn watch_run(
    ws: &Workspace,
    repo: &str,
    run_id: u64,
    tail: usize,
    cancel: &AtomicBool,
    started: Instant,
    progress: &mut dyn FnMut(&str),
) -> Result<ToolOutput, String> {
    let mut poll_count = 0u64;
    let mut failures = 0u32;
    loop {
        check_cancel(cancel)?;
        poll_count += 1;
        let mut run = match fetch_run_snapshot(ws, repo, run_id, cancel) {
            Ok(run) => run,
            Err(err) => {
                handle_poll_error(err, cancel, &mut failures)?;
                continue;
            }
        };
        failures = 0;
        progress(&format_run_snapshot(repo, &run, poll_count, None));

        let mut failed = failed_jobs(&run.jobs);
        let completed = run.status.as_deref() == Some("completed");
        if !failed.is_empty() {
            if !completed {
                let note = failure_note();
                progress(&format_run_snapshot(
                    repo,
                    &run,
                    poll_count,
                    Some(note.as_str()),
                ));
                wait_for(GRACE, cancel)?;
                match fetch_run_snapshot(ws, repo, run_id, cancel) {
                    Ok(refetched) => {
                        // An auto-retry can reset conclusions during the grace window; keep the detected failures then.
                        let again = failed_jobs(&refetched.jobs);
                        if !again.is_empty() {
                            run = refetched;
                            failed = again;
                        }
                    }
                    Err(_) => check_cancel(cancel)?,
                }
            }
            let pairs: Vec<(Run, Job)> = failed.into_iter().map(|job| (run.clone(), job)).collect();
            let logs = fetch_failed_job_logs(ws, repo, pairs, tail, cancel)?;
            return run_failure_output(ws, repo, &run, &logs, tail);
        }
        if completed {
            let text = format_run_result(repo, &run, &[], LogMode::Tail(tail));
            let data = run_result_details(repo, &run, &[], now_ms());
            return Ok(finish(text, data, None, run.url.as_deref()));
        }
        wait_for(current_interval(started), cancel)?;
    }
}

fn run_failure_output(
    ws: &Workspace,
    repo: &str,
    run: &Run,
    logs: &[FailedLog],
    tail: usize,
) -> Result<ToolOutput, String> {
    let full = format_run_result(repo, run, logs, LogMode::Full);
    let artifact = save_artifact(ws, &full)?;
    let mut text = format_run_result(repo, run, logs, LogMode::Tail(tail));
    append_artifact_reference(&mut text, artifact.as_deref(), "Full failed-job logs");
    let artifact_id = artifact
        .as_deref()
        .map(|uri| bare_artifact_id(uri).to_owned());
    let data = run_result_details(repo, run, logs, now_ms());
    Ok(finish(text, data, artifact_id, run.url.as_deref()))
}

fn watch_commit(
    ws: &Workspace,
    repo: &str,
    branch_input: Option<&str>,
    tail: usize,
    cancel: &AtomicBool,
    started: Instant,
    progress: &mut dyn FnMut(&str),
) -> Result<ToolOutput, String> {
    let (branch, head) = match branch_input {
        Some(branch) => {
            let head = resolve_branch_head(ws, repo, branch, cancel)?;
            (branch.to_owned(), head)
        }
        None => {
            // Without a branch or run, the watched commit is the checkout's HEAD, which is only valid when the
            // checkout is the target repo.
            let cwd_repo = resolve_repo_from_cwd(ws, cancel).ok();
            check_cancel(cancel)?;
            if !slug_equals(cwd_repo.as_deref(), repo) {
                return Err(format!(
                    "Cannot infer the watched commit for {repo}: current checkout is {}. Pass `branch` or `run` to scope the watch.",
                    cwd_repo.as_deref().unwrap_or("not a GitHub repository")
                ));
            }
            (
                require_current_git_branch(ws, cancel)?,
                require_current_git_head(ws, cancel)?,
            )
        }
    };

    let mut poll_count = 0u64;
    let mut failures = 0u32;
    let mut settled: Option<String> = None;
    let mut ever_saw_runs = false;
    let mut cache: HashMap<u64, Vec<Job>> = HashMap::new();
    loop {
        check_cancel(cancel)?;
        poll_count += 1;
        let mut runs = match fetch_runs_for_commit(ws, repo, &head, cancel, &mut cache) {
            Ok(runs) => runs,
            Err(err) => {
                handle_poll_error(err, cancel, &mut failures)?;
                continue;
            }
        };
        failures = 0;
        if !runs.is_empty() {
            ever_saw_runs = true;
        }
        progress(&format_commit_snapshot(
            repo, &head, &branch, &runs, poll_count, None,
        ));

        match collection_outcome(&runs) {
            Outcome::Failure => {
                let mut pairs = failed_pairs(&runs);
                let note = failure_note();
                progress(&format_commit_snapshot(
                    repo,
                    &head,
                    &branch,
                    &runs,
                    poll_count,
                    Some(note.as_str()),
                ));
                wait_for(GRACE, cancel)?;
                match fetch_runs_for_commit(ws, repo, &head, cancel, &mut cache) {
                    Ok(refetched) => {
                        let again = failed_pairs(&refetched);
                        if !again.is_empty() {
                            runs = refetched;
                            pairs = again;
                        }
                    }
                    Err(_) => check_cancel(cancel)?,
                }
                let logs = fetch_failed_job_logs(ws, repo, pairs, tail, cancel)?;
                return commit_failure_output(ws, repo, &head, &branch, &runs, &logs, tail);
            }
            Outcome::Success => {
                let signature = run_signature(&runs);
                if settled.as_deref() == Some(signature.as_str()) {
                    let text =
                        format_commit_result(repo, &head, &branch, &runs, &[], LogMode::Tail(tail));
                    let data =
                        commit_result_details(repo, &head, &branch, &runs, &[], None, now_ms());
                    return Ok(finish(text, data, None, None));
                }
                settled = Some(signature);
                // Wait one more interval so runs created by the same push have time to appear.
                let confirm = current_interval(started);
                let note = format!(
                    "All known workflow runs completed successfully. Waiting {}s to ensure no additional runs appear for this commit.",
                    confirm.as_secs()
                );
                progress(&format_commit_snapshot(
                    repo,
                    &head,
                    &branch,
                    &runs,
                    poll_count,
                    Some(note.as_str()),
                ));
                wait_for(confirm, cancel)?;
            }
            Outcome::Pending => {
                settled = None;
                if !ever_saw_runs && started.elapsed() >= NO_RUNS_GIVE_UP {
                    let elapsed = started.elapsed().as_secs_f64().round() as u64;
                    let text = format!(
                        "No workflow runs found for {repo}@{} after {elapsed}s ({poll_count} polls). The commit may not trigger any GitHub Actions workflows, or Actions may be disabled for this repository. Pass `run` to watch a specific run.",
                        short_sha(&head)
                    );
                    let data = commit_result_details(
                        repo,
                        &head,
                        &branch,
                        &runs,
                        &[],
                        Some(poll_count),
                        now_ms(),
                    );
                    return Ok(finish(text, data, None, None));
                }
                wait_for(current_interval(started), cancel)?;
            }
        }
    }
}

fn commit_failure_output(
    ws: &Workspace,
    repo: &str,
    head: &str,
    branch: &str,
    runs: &[Run],
    logs: &[FailedLog],
    tail: usize,
) -> Result<ToolOutput, String> {
    let full = format_commit_result(repo, head, branch, runs, logs, LogMode::Full);
    let artifact = save_artifact(ws, &full)?;
    let mut text = format_commit_result(repo, head, branch, runs, logs, LogMode::Tail(tail));
    append_artifact_reference(&mut text, artifact.as_deref(), "Full failed-job logs");
    let artifact_id = artifact
        .as_deref()
        .map(|uri| bare_artifact_id(uri).to_owned());
    let data = commit_result_details(repo, head, branch, runs, logs, None, now_ms());
    Ok(finish(text, data, artifact_id, None))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
