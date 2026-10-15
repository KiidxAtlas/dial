//! GitHub search ops of the `github` tool: `search_issues`, `search_prs`, `search_code`,
//! `search_commits` and `search_repos`.
//!
//! Queries go to the REST `/search/<endpoint>` API through `gh api`, not `gh search`, which quotes
//! multi-token positional queries and returns no hits. Relative date bounds are
//! resolved against the clock passed into `run_at`, so tests pin `now`.

use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::gh_common::{GH_TIMEOUT, clip, default_gh_host, gh, gh_json, strings, valid_repo};
use super::{ToolOutput, Workspace, opt_str};

const SEARCH_LIMIT_DEFAULT: u64 = 10;
const SEARCH_LIMIT_MAX: u64 = 50;
const TEXT_MATCH_ACCEPT: &str = "Accept: application/vnd.github.text-match+json";
const DAY_MS: i128 = 86_400_000;
/// JavaScript's `Date` range (±8.64e15 ms); outside it `toISOString` throws.
const DATE_RANGE_MS: i128 = 8_640_000_000_000_000;
/// Counts past this cannot land inside `DATE_RANGE_MS`; rejecting them keeps the i128 math exact.
const MAX_DATE_COUNT: u128 = 100_000_000_000;
/// Scope qualifiers that mark an explicit repo/org/user, matched at token starts with an optional `-`.
const SCOPE_PREFIXES: [&str; 4] = ["repo:", "org:", "user:", "owner:"];

/// Cache of successful cwd → `owner/repo` lookups; failures are not cached.
static CURRENT_REPOS: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Run one search op. `op` is one of `search_issues`, `search_prs`, `search_code`, `search_commits`, `search_repos`.
pub(super) fn run(
    op: &str,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    run_at(op, ws, args, cancel, now_ms()?)
}

fn run_at(
    op: &str,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    now: i64,
) -> Result<ToolOutput, String> {
    let text = match op {
        "search_issues" => search_issues(ws, args, cancel, now, false)?,
        "search_prs" => search_issues(ws, args, cancel, now, true)?,
        "search_code" => search_code(ws, args, cancel)?,
        "search_commits" => search_commits(ws, args, cancel, now)?,
        "search_repos" => search_repos(ws, args, cancel, now)?,
        other => return Err(format!("unknown search op {other:?}")),
    };
    Ok(ToolOutput::ok(clip(ws, text)))
}

fn now_ms() -> Result<i64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
    i64::try_from(elapsed.as_millis()).map_err(|_| "system clock is out of range".to_owned())
}

fn search_issues(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    now: i64,
    pull_requests: bool,
) -> Result<String, String> {
    let limit = search_limit(args)?;
    let field = date_field(if pull_requests { "prs" } else { "issues" }, args)?;
    let date = date_qualifier(field, opt_str(args, "since")?, opt_str(args, "until")?, now)?;
    let display = compose_query(&[opt_str(args, "query")?, date.as_deref()])?;
    let repo = resolve_scope(ws, normalized_repo(args)?, Some(display.as_str()), cancel)?;
    let scope = SearchScope::new(repo.as_deref())?;
    let kind_filter = if pull_requests { "is:pr" } else { "is:issue" };
    let api_query = compose_query(&[
        Some(display.as_str()),
        scope.qualifier.as_deref(),
        Some(kind_filter),
    ])?;
    let items: Vec<ApiIssue> = search(
        ws,
        "issues",
        &api_query,
        limit,
        scope.host.as_deref(),
        &[],
        cancel,
    )?;
    let kind = if pull_requests {
        "pull requests"
    } else {
        "issues"
    };
    Ok(format_issue_results(
        kind,
        &display,
        repo.as_deref(),
        &items,
    ))
}

fn search_code(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<String, String> {
    let query = opt_str(args, "query")?
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or_else(|| "query must not be empty".to_owned())?;
    if nonblank(opt_str(args, "since")?) || nonblank(opt_str(args, "until")?) {
        return Err(
            "search_code does not support since/until; GitHub code search has no date qualifier."
                .to_owned(),
        );
    }
    let limit = search_limit(args)?;
    let repo = resolve_scope(ws, normalized_repo(args)?, Some(query), cancel)?;
    let scope = SearchScope::new(repo.as_deref())?;
    let api_query = compose_query(&[Some(query), scope.qualifier.as_deref()])?;
    let items: Vec<ApiCodeItem> = search(
        ws,
        "code",
        &api_query,
        limit,
        scope.host.as_deref(),
        &[TEXT_MATCH_ACCEPT],
        cancel,
    )?;
    Ok(format_code_results(query, repo.as_deref(), &items))
}

fn search_commits(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    now: i64,
) -> Result<String, String> {
    let limit = search_limit(args)?;
    let field = date_field("commits", args)?;
    let date = date_qualifier(field, opt_str(args, "since")?, opt_str(args, "until")?, now)?;
    let display = compose_query(&[opt_str(args, "query")?, date.as_deref()])?;
    let repo = resolve_scope(ws, normalized_repo(args)?, Some(display.as_str()), cancel)?;
    let scope = SearchScope::new(repo.as_deref())?;
    let api_query = compose_query(&[Some(display.as_str()), scope.qualifier.as_deref()])?;
    let items: Vec<ApiCommitItem> = search(
        ws,
        "commits",
        &api_query,
        limit,
        scope.host.as_deref(),
        &[],
        cancel,
    )?;
    Ok(format_commit_results(&display, repo.as_deref(), &items))
}

fn search_repos(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    now: i64,
) -> Result<String, String> {
    let limit = search_limit(args)?;
    let field = date_field("repos", args)?;
    let date = date_qualifier(field, opt_str(args, "since")?, opt_str(args, "until")?, now)?;
    let query = compose_query(&[opt_str(args, "query")?, date.as_deref()])?;
    let items: Vec<ApiRepoItem> = search(ws, "repositories", &query, limit, None, &[], cancel)?;
    Ok(format_repo_results(&query, &items))
}

/// `limit` default 10, positive, floored and capped at 50.
fn search_limit(args: &Value) -> Result<u64, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(SEARCH_LIMIT_DEFAULT),
        Some(value) => {
            let limit = value
                .as_f64()
                .filter(|limit| *limit > 0.0)
                .ok_or_else(|| "limit must be a positive number".to_owned())?;
            Ok((limit.floor().max(1.0) as u64).min(SEARCH_LIMIT_MAX))
        }
    }
}

/// The qualifier field for a search command.
fn date_field(command: &str, args: &Value) -> Result<&'static str, String> {
    if command == "commits" {
        return Ok("committer-date");
    }
    let requested = match opt_str(args, "dateField")? {
        None | Some("created") => "created",
        Some("updated") => "updated",
        Some(other) => {
            return Err(format!(
                "dateField must be \"created\" or \"updated\", got {other:?}"
            ));
        }
    };
    Ok(if command == "repos" && requested == "updated" {
        "pushed"
    } else {
        requested
    })
}

/// `field:<since>..<until>`, `field:>=<since>` or `field:<=<until>`; `None` when neither bound is set
/// An empty bound counts as unset, as in the JS truthiness check.
fn date_qualifier(
    field: &str,
    since: Option<&str>,
    until: Option<&str>,
    now: i64,
) -> Result<Option<String>, String> {
    let since = since
        .filter(|bound| !bound.is_empty())
        .map(|bound| parse_date_bound(bound, now))
        .transpose()?;
    let until = until
        .filter(|bound| !bound.is_empty())
        .map(|bound| parse_date_bound(bound, now))
        .transpose()?;
    Ok(match (since, until) {
        (Some(since), Some(until)) => Some(format!("{field}:{since}..{until}")),
        (Some(since), None) => Some(format!("{field}:>={since}")),
        (None, Some(until)) => Some(format!("{field}:<={until}")),
        (None, None) => None,
    })
}

/// Resolve a bound to `YYYY-MM-DD` (relative durations, ISO dates) or `YYYY-MM-DDTHH:MM:SSZ` (datetimes).
fn parse_date_bound(raw: &str, now: i64) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("date bound must not be empty".to_owned());
    }
    if let Some((count, unit)) = relative_bound(trimmed) {
        return relative_ms(count, unit, now)
            .and_then(format_date)
            .ok_or_else(|| invalid_bound(raw));
    }
    if is_iso_date(trimmed) {
        return Ok(trimmed.to_owned());
    }
    parse_iso_datetime(trimmed)
        .and_then(format_datetime)
        .ok_or_else(|| invalid_bound(raw))
}

fn invalid_bound(raw: &str) -> String {
    format!(
        "invalid date bound: {raw}. Expected a relative duration like \"3d\", \"12h\", \"2w\", an ISO date \"YYYY-MM-DD\", or an ISO datetime."
    )
}

/// `^(\d+)\s*(m|h|d|w|mo|y)$` with case-insensitive units.
fn relative_bound(text: &str) -> Option<(u128, &'static str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let count: u128 = text[..digits].parse().ok()?;
    let unit = match text[digits..].trim_start().to_ascii_lowercase().as_str() {
        "m" => "m",
        "h" => "h",
        "d" => "d",
        "w" => "w",
        "mo" => "mo",
        "y" => "y",
        _ => return None,
    };
    Some((count, unit))
}

/// `now` minus the relative duration, in ms. Months and years follow JS `setUTCMonth` /
/// `setUTCFullYear`: the time of day is kept and an out-of-range day rolls forward (Mar 31 − 1 month → Mar 3).
fn relative_ms(count: u128, unit: &str, now: i64) -> Option<i128> {
    if count > MAX_DATE_COUNT {
        return None;
    }
    let now = i128::from(now);
    let count = count as i128;
    let fixed = match unit {
        "m" => Some(60_000),
        "h" => Some(3_600_000),
        "d" => Some(DAY_MS),
        "w" => Some(7 * DAY_MS),
        _ => None,
    };
    if let Some(ms) = fixed {
        return Some(now - count * ms);
    }
    let days = now.div_euclid(DAY_MS) as i64;
    let (year, month, day) = civil_from_days(days);
    let count = count as i64;
    let (year, month) = if unit == "mo" {
        let total = year * 12 + (month - 1) - count;
        (total.div_euclid(12), total.rem_euclid(12) + 1)
    } else {
        (year - count, month)
    };
    let first = days_from_civil(year, month, 1);
    Some(i128::from(first + day - 1) * DAY_MS + now.rem_euclid(DAY_MS))
}

fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

fn format_date(ms: i128) -> Option<String> {
    let (year, month, day) = civil_from_ms(ms)?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Seconds precision, as GitHub search accepts (strips the `.sss` from `toISOString`).
fn format_datetime(ms: i128) -> Option<String> {
    if ms.abs() > DATE_RANGE_MS {
        return None;
    }
    let seconds = ms.div_euclid(1000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400) as i64);
    let second_of_day = seconds.rem_euclid(86_400) as i64;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    ))
}

fn civil_from_ms(ms: i128) -> Option<(i64, i64, i64)> {
    if ms.abs() > DATE_RANGE_MS {
        return None;
    }
    let (year, month, day) = civil_from_days(ms.div_euclid(DAY_MS) as i64);
    (0..=9999).contains(&year).then_some((year, month, day))
}

/// ISO 8601 subset of JS `Date.parse`: `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, optionally followed by `T`
/// HH:MM[:SS[.fraction]] and `Z` or `±HH:MM`. Date-only forms are UTC; date-times without an offset are
/// local time, as in JS.
fn parse_iso_datetime(text: &str) -> Option<i128> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let year = take_digits(bytes, &mut pos, 4)?;
    let (mut month, mut day) = (1, 1);
    if eat(bytes, &mut pos, b'-') {
        month = take_digits(bytes, &mut pos, 2)?;
        if eat(bytes, &mut pos, b'-') {
            day = take_digits(bytes, &mut pos, 2)?;
        }
    }
    let (mut hour, mut minute, mut second) = (0, 0, 0);
    let mut offset_minutes: Option<i64> = None;
    let has_time = eat(bytes, &mut pos, b'T');
    if has_time {
        hour = take_digits(bytes, &mut pos, 2)?;
        if !eat(bytes, &mut pos, b':') {
            return None;
        }
        minute = take_digits(bytes, &mut pos, 2)?;
        if eat(bytes, &mut pos, b':') {
            second = take_digits(bytes, &mut pos, 2)?;
            if eat(bytes, &mut pos, b'.') {
                let start = pos;
                while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
                    pos += 1;
                }
                if pos == start {
                    return None;
                }
            }
        }
        if eat(bytes, &mut pos, b'Z') {
            offset_minutes = Some(0);
        } else if let Some(&sign) = bytes.get(pos).filter(|byte| matches!(**byte, b'+' | b'-')) {
            pos += 1;
            let hours = take_digits(bytes, &mut pos, 2)?;
            if !eat(bytes, &mut pos, b':') {
                return None;
            }
            let minutes = take_digits(bytes, &mut pos, 2)?;
            let total = hours * 60 + minutes;
            offset_minutes = Some(if sign == b'-' { -total } else { total });
        }
    }
    if pos != bytes.len() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, 1) + day - 1;
    let wall =
        i128::from(days) * DAY_MS + i128::from(hour * 3_600_000 + minute * 60_000 + second * 1000);
    Some(match offset_minutes {
        Some(minutes) => wall - i128::from(minutes) * 60_000,
        None if has_time => local_to_utc(wall),
        None => wall,
    })
}

/// Map a wall-clock time in the local zone to UTC, refining the offset once across DST edges.
fn local_to_utc(wall: i128) -> i128 {
    let first = wall - local_offset_ms(wall);
    wall - local_offset_ms(first)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn local_offset_ms(utc_ms: i128) -> i128 {
    let time = utc_ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm` is plain old data, zero-initialised, and `localtime_r` only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference live locals; a null return means no conversion and is handled below.
    let converted = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if converted {
        i128::from(tm.tm_gmtoff as i64) * 1000
    } else {
        0
    }
}

/// Other platforms lack a portable `tm_gmtoff`, so datetimes without an offset are read as UTC there.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn local_offset_ms(_utc_ms: i128) -> i128 {
    0
}

fn take_digits(bytes: &[u8], pos: &mut usize, count: usize) -> Option<i64> {
    let digits = bytes.get(*pos..*pos + count)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *pos += count;
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn eat(bytes: &[u8], pos: &mut usize, byte: u8) -> bool {
    if bytes.get(*pos) == Some(&byte) {
        *pos += 1;
        true
    } else {
        false
    }
}

/// Proleptic Gregorian date → days since 1970-01-01 (H. Hinnant). `day` may overflow the month.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Days since 1970-01-01 → (year, month, day), inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// True when a query token already names a repo/org/user/owner scope.
fn has_scope_qualifier(query: &str) -> bool {
    query.split_whitespace().any(|token| {
        let token = token.strip_prefix('-').unwrap_or(token).as_bytes();
        SCOPE_PREFIXES.iter().any(|prefix| {
            let prefix = prefix.as_bytes();
            token.len() > prefix.len() && token[..prefix.len()].eq_ignore_ascii_case(prefix)
        })
    })
}

/// Explicit `repo` wins; a query that already names a scope gets none; otherwise the checkout's repo,
/// silently absent when it cannot be resolved.
fn resolve_scope(
    ws: &Workspace,
    repo: Option<&str>,
    query: Option<&str>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    if let Some(repo) = repo {
        return Ok(Some(repo.to_owned()));
    }
    if query.is_some_and(has_scope_qualifier) {
        return Ok(None);
    }
    Ok(current_repo(ws, cancel))
}

/// The checkout's `[HOST/]OWNER/REPO`, from the remote URL so an enterprise host survives
/// Any failure yields `None`.
fn current_repo(ws: &Workspace, cancel: &AtomicBool) -> Option<String> {
    let key = ws.root().to_path_buf();
    if let Some(repo) = CURRENT_REPOS.lock().get(&key) {
        return Some(repo.clone());
    }
    let args = strings(&["repo", "view", "--json", "url", "-q", ".url"]);
    let url = gh(ws, &args, GH_TIMEOUT, cancel).ok()?;
    let repo = repo_from_url(url.trim())?;
    CURRENT_REPOS.lock().insert(key, repo.clone());
    Some(repo)
}

/// `https://HOST/OWNER/REPO[...]` → identity. The host is dropped only when it is the default `gh` host.
fn repo_from_url(value: &str) -> Option<String> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let mut parts = rest.splitn(3, '/');
    let host = parts
        .next()
        .filter(|host| !host.is_empty())?
        .to_ascii_lowercase();
    let owner = parts.next().filter(|owner| !owner.is_empty())?;
    let name = parts
        .next()?
        .split(['/', '?', '#'])
        .next()
        .filter(|name| !name.is_empty())?;
    let slug = format!("{owner}/{name}");
    Some(if host == default_gh_host().to_ascii_lowercase() {
        slug
    } else {
        format!("{host}/{slug}")
    })
}

/// `…/repos/OWNER/REPO` at the end of an API URL → `OWNER/REPO`.
fn repo_from_repository_url(value: &str) -> Option<String> {
    let mut parts = value.rsplitn(3, '/');
    let name = parts.next().filter(|name| !name.is_empty())?;
    let owner = parts.next().filter(|owner| !owner.is_empty())?;
    if !parts.next()?.ends_with("/repos") {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// The `repo:` qualifier and the `--hostname` for a resolved scope.
struct SearchScope {
    qualifier: Option<String>,
    host: Option<String>,
}

impl SearchScope {
    fn new(repo: Option<&str>) -> Result<Self, String> {
        let Some(repo) = repo else {
            return Ok(Self {
                qualifier: None,
                host: None,
            });
        };
        let (host, slug) = split_repo_ref(repo);
        let slug = valid_repo(slug)?;
        Ok(Self {
            qualifier: Some(format!("repo:{slug}")),
            host: host.map(str::to_owned),
        })
    }
}

/// Split `[HOST/]OWNER/REPO`; any other shape is taken whole as the slug.
fn split_repo_ref(repo: &str) -> (Option<&str>, &str) {
    let Some(first) = repo.find('/') else {
        return (None, repo);
    };
    let rest = &repo[first + 1..];
    match rest.find('/') {
        Some(second) if !rest[second + 1..].contains('/') => (Some(&repo[..first]), rest),
        _ => (None, repo),
    }
}

fn normalized_repo(args: &Value) -> Result<Option<&str>, String> {
    Ok(opt_str(args, "repo")?
        .map(str::trim)
        .filter(|repo| !repo.is_empty()))
}

fn nonblank(value: Option<&str>) -> bool {
    value.is_some_and(|text| !text.trim().is_empty())
}

/// Join the non-empty parts with spaces; an empty result is an error.
fn compose_query(parts: &[Option<&str>]) -> Result<String, String> {
    let cleaned: Vec<&str> = parts
        .iter()
        .flatten()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect();
    if cleaned.is_empty() {
        return Err("query is required (or pass since/until to filter by date)".to_owned());
    }
    Ok(cleaned.join(" "))
}

/// The `gh api` argv for `/search/<endpoint>`.
fn api_search_args(
    endpoint: &str,
    query: &str,
    limit: u64,
    host: Option<&str>,
    headers: &[&str],
) -> Vec<String> {
    let mut args = vec!["api".to_owned()];
    if let Some(host) = host {
        args.push("--hostname".to_owned());
        args.push(host.to_owned());
    }
    args.extend(strings(&["-X", "GET"]));
    args.push(format!("/search/{endpoint}"));
    args.push("-f".to_owned());
    args.push(format!("q={query}"));
    args.push("-F".to_owned());
    args.push(format!("per_page={limit}"));
    for header in headers {
        args.push("-H".to_owned());
        args.push((*header).to_owned());
    }
    args
}

#[derive(Deserialize)]
struct SearchResponse<T> {
    items: Option<Vec<T>>,
}

/// Run a search through `gh api` and return its `items`, or an empty list when absent.
fn search<T: DeserializeOwned>(
    ws: &Workspace,
    endpoint: &str,
    query: &str,
    limit: u64,
    host: Option<&str>,
    headers: &[&str],
    cancel: &AtomicBool,
) -> Result<Vec<T>, String> {
    let args = api_search_args(endpoint, query, limit, host, headers);
    let value = gh_json(ws, &args, GH_TIMEOUT, cancel)?;
    let response: SearchResponse<T> = serde_json::from_value(value)
        .map_err(|error| format!("unexpected response from /search/{endpoint}: {error}"))?;
    Ok(response.items.unwrap_or_default())
}

#[derive(Deserialize)]
struct ApiUser {
    login: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ApiLabel {
    name: Option<String>,
}

#[derive(Deserialize)]
struct ApiPullRequestRef {
    merged_at: Option<String>,
}

#[derive(Deserialize)]
struct ApiIssue {
    number: Option<i64>,
    title: Option<String>,
    state: Option<String>,
    user: Option<ApiUser>,
    labels: Option<Vec<ApiLabel>>,
    created_at: Option<String>,
    updated_at: Option<String>,
    html_url: Option<String>,
    repository_url: Option<String>,
    pull_request: Option<ApiPullRequestRef>,
}

#[derive(Deserialize)]
struct ApiFullRepo {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct ApiTextMatch {
    fragment: Option<String>,
}

#[derive(Deserialize)]
struct ApiCodeItem {
    path: Option<String>,
    sha: Option<String>,
    html_url: Option<String>,
    repository: Option<ApiFullRepo>,
    text_matches: Option<Vec<ApiTextMatch>>,
}

#[derive(Deserialize)]
struct ApiGitActor {
    name: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
struct ApiCommitDetail {
    author: Option<ApiGitActor>,
    committer: Option<ApiGitActor>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct ApiCommitItem {
    sha: Option<String>,
    html_url: Option<String>,
    author: Option<ApiUser>,
    commit: Option<ApiCommitDetail>,
    repository: Option<ApiFullRepo>,
}

#[derive(Deserialize)]
struct ApiRepoItem {
    full_name: Option<String>,
    description: Option<String>,
    language: Option<String>,
    stargazers_count: Option<i64>,
    forks_count: Option<i64>,
    open_issues_count: Option<i64>,
    archived: Option<bool>,
    fork: Option<bool>,
    visibility: Option<String>,
    updated_at: Option<String>,
    html_url: Option<String>,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}

/// Normalizes text: unify line endings, expand tabs, trim.
fn normalize_text(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .trim()
        .to_owned()
}

/// First line of the normalized text; `None` only when the input is absent.
fn first_line(value: Option<&str>) -> Option<String> {
    value.map(|text| {
        normalize_text(text)
            .split('\n')
            .next()
            .unwrap_or("")
            .to_owned()
    })
}

/// `@login`, else the display name.
fn format_author(user: Option<&ApiUser>) -> Option<String> {
    let user = user?;
    if let Some(login) = non_empty(user.login.as_deref()) {
        return Some(format!("@{login}"));
    }
    non_empty(user.name.as_deref()).map(str::to_owned)
}

/// Comma-separated label names.
fn format_labels(labels: &[ApiLabel]) -> Option<String> {
    let names: Vec<&str> = labels
        .iter()
        .filter_map(|label| non_empty(label.name.as_deref()))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// First 12 characters of a SHA.
fn format_short_sha(value: Option<&str>) -> Option<String> {
    non_empty(value).map(|sha| sha.chars().take(12).collect())
}

/// A `Label: value` line; absent and empty values are skipped.
fn push_line<T: Display>(lines: &mut Vec<String>, label: &str, value: Option<T>) {
    if let Some(value) = value {
        let text = value.to_string();
        if !text.is_empty() {
            lines.push(format!("{label}: {text}"));
        }
    }
}

fn finish(lines: Vec<String>) -> String {
    lines.join("\n").trim().to_owned()
}

fn format_issue_results(kind: &str, query: &str, repo: Option<&str>, items: &[ApiIssue]) -> String {
    let mut lines = vec![
        format!("# GitHub {kind} search"),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Repository", repo);
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push(format!("No {kind} found."));
        return finish(lines);
    }
    for item in items {
        lines.push(String::new());
        let number = item
            .number
            .map_or_else(|| "?".to_owned(), |number| number.to_string());
        lines.push(format!(
            "- #{number} {}",
            item.title.as_deref().unwrap_or("Untitled")
        ));
        let merged = non_empty(
            item.pull_request
                .as_ref()
                .and_then(|pr| pr.merged_at.as_deref()),
        )
        .is_some();
        let state = if merged {
            Some("merged")
        } else {
            item.state.as_deref()
        };
        let repo = item
            .repository_url
            .as_deref()
            .and_then(repo_from_repository_url);
        let labels = format_labels(item.labels.as_deref().unwrap_or_default());
        push_line(&mut lines, "  Repo", repo.as_deref());
        push_line(&mut lines, "  State", state);
        push_line(
            &mut lines,
            "  Author",
            format_author(item.user.as_ref()).as_deref(),
        );
        push_line(&mut lines, "  Labels", labels.as_deref());
        push_line(&mut lines, "  Created", item.created_at.as_deref());
        push_line(&mut lines, "  Updated", item.updated_at.as_deref());
        push_line(&mut lines, "  URL", item.html_url.as_deref());
    }
    finish(lines)
}

fn format_code_results(query: &str, repo: Option<&str>, items: &[ApiCodeItem]) -> String {
    let mut lines = vec![
        "# GitHub code search".to_owned(),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Repository", repo);
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push("No code matches found.".to_owned());
        return finish(lines);
    }
    for item in items {
        lines.push(String::new());
        lines.push(format!(
            "- {}",
            item.path.as_deref().unwrap_or("(unknown path)")
        ));
        let commit = format_short_sha(item.sha.as_deref());
        push_line(
            &mut lines,
            "  Repo",
            item.repository
                .as_ref()
                .and_then(|r| r.full_name.as_deref()),
        );
        push_line(&mut lines, "  Commit", commit.as_deref());
        push_line(&mut lines, "  URL", item.html_url.as_deref());
        let fragment = item
            .text_matches
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find_map(|found| non_empty(found.fragment.as_deref()));
        if let Some(fragment) = fragment {
            let line = first_line(Some(fragment));
            push_line(&mut lines, "  Match", line.as_deref());
        }
    }
    finish(lines)
}

fn format_commit_results(query: &str, repo: Option<&str>, items: &[ApiCommitItem]) -> String {
    let mut lines = vec![
        "# GitHub commits search".to_owned(),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Repository", repo);
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push("No commits found.".to_owned());
        return finish(lines);
    }
    for item in items {
        lines.push(String::new());
        let sha =
            format_short_sha(item.sha.as_deref()).unwrap_or_else(|| "(unknown sha)".to_owned());
        let subject = first_line(item.commit.as_ref().and_then(|c| c.message.as_deref()))
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| "(no commit message)".to_owned());
        lines.push(format!("- {sha} {subject}"));
        let detail = item.commit.as_ref();
        let commit_author = detail.and_then(|c| c.author.as_ref());
        let author = format_author(item.author.as_ref())
            .or_else(|| commit_author.and_then(|a| a.name.clone()));
        let date = commit_author.and_then(|a| a.date.as_deref()).or_else(|| {
            detail
                .and_then(|c| c.committer.as_ref())
                .and_then(|a| a.date.as_deref())
        });
        push_line(
            &mut lines,
            "  Repo",
            item.repository
                .as_ref()
                .and_then(|r| r.full_name.as_deref()),
        );
        push_line(&mut lines, "  Author", author.as_deref());
        push_line(&mut lines, "  Date", date);
        push_line(&mut lines, "  URL", item.html_url.as_deref());
    }
    finish(lines)
}

fn format_repo_results(query: &str, items: &[ApiRepoItem]) -> String {
    let mut lines = vec![
        "# GitHub repositories search".to_owned(),
        String::new(),
        format!("Query: {query}"),
    ];
    push_line(&mut lines, "Results", Some(items.len()));
    if items.is_empty() {
        lines.push(String::new());
        lines.push("No repositories found.".to_owned());
        return finish(lines);
    }
    for item in items {
        lines.push(String::new());
        lines.push(format!(
            "- {}",
            item.full_name.as_deref().unwrap_or("(unknown repository)")
        ));
        let description = first_line(item.description.as_deref()).filter(|line| !line.is_empty());
        push_line(&mut lines, "  Description", description.as_deref());
        push_line(&mut lines, "  Language", item.language.as_deref());
        push_line(&mut lines, "  Stars", item.stargazers_count);
        push_line(&mut lines, "  Forks", item.forks_count);
        push_line(&mut lines, "  Open issues", item.open_issues_count);
        push_line(&mut lines, "  Visibility", item.visibility.as_deref());
        push_line(&mut lines, "  Archived", item.archived);
        push_line(&mut lines, "  Fork", item.fork);
        push_line(&mut lines, "  Updated", item.updated_at.as_deref());
        push_line(&mut lines, "  URL", item.html_url.as_deref());
    }
    finish(lines)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use serde_json::{Value, json};

    use super::*;
    use crate::tools::gh_common::test_support::FakeGh;
    use crate::tools::testkit;

    /// 2026-05-10T12:00:00Z
    const NOW: i64 = 1_778_414_400_000;

    /// Noon UTC on the given date, in ms.
    fn noon(year: i64, month: i64, day: i64) -> i64 {
        (days_from_civil(year, month, day) * 86_400 + 43_200) * 1_000
    }

    fn run_with(op: &str, args: Value) -> Result<ToolOutput, String> {
        let (_dir, ws) = testkit::workspace();
        run_at(op, &ws, &args, &AtomicBool::new(false), NOW)
    }

    fn assert_lines(text: &str, expected: &[&str]) {
        for line in expected {
            assert!(
                text.lines().any(|found| found == *line),
                "missing {line:?} in:\n{text}"
            );
        }
    }

    const ISSUES_JSON: &str = r#"{"total_count":1,"incomplete_results":false,"items":[{"number":42,"title":"Crash on save","state":"closed","user":{"login":"octo","name":null},"labels":[{"name":"bug"},{"name":"p1"}],"created_at":"2026-05-01T09:00:00Z","updated_at":"2026-05-02T09:00:00Z","html_url":"https://github.com/acme/widgets/pull/42","repository_url":"https://api.github.com/repos/acme/widgets","pull_request":{"merged_at":"2026-05-02T09:00:00Z"}}]}"#;

    const CODE_JSON: &str = r#"{"total_count":1,"items":[{"name":"main.rs","path":"src/main.rs","sha":"0123456789abcdef0123","html_url":"https://github.com/acme/widgets/blob/main/src/main.rs","repository":{"full_name":"acme/widgets"},"text_matches":[{"fragment":"\n    fn main() {\r\n","property":"content"}]}]}"#;

    const COMMITS_JSON: &str = r#"{"items":[{"sha":"0123456789abcdef","node_id":"C_1","html_url":"https://github.com/acme/widgets/commit/0123","author":{"login":"octo"},"committer":null,"commit":{"author":{"name":"Octo Cat","email":"o@example.com","date":"2026-05-04T10:00:00Z"},"committer":{"name":"Octo Cat","date":"2026-05-05T10:00:00Z"},"message":"fix: crash on save\n\nlong body"},"repository":{"full_name":"acme/widgets"}}]}"#;

    const REPOS_JSON: &str = r#"{"items":[{"full_name":"acme/widgets","description":"A widget\nsecond line","language":"Rust","stargazers_count":12,"forks_count":3,"open_issues_count":1,"archived":false,"fork":false,"private":false,"visibility":"public","updated_at":"2026-05-08T00:00:00Z","created_at":"2024-01-01T00:00:00Z","html_url":"https://github.com/acme/widgets","owner":{"login":"acme"}}]}"#;

    #[test]
    fn limit_defaults_floors_and_caps() {
        assert_eq!(search_limit(&json!({})).unwrap(), 10);
        assert_eq!(search_limit(&json!({"limit": 500})).unwrap(), 50);
        assert_eq!(search_limit(&json!({"limit": 2.9})).unwrap(), 2);
        assert_eq!(
            search_limit(&json!({"limit": 0})).unwrap_err(),
            "limit must be a positive number"
        );
        assert!(search_limit(&json!({"limit": -1})).is_err());
        assert!(search_limit(&json!({"limit": "5"})).is_err());
    }

    #[test]
    fn relative_bounds_subtract_fixed_units() {
        assert_eq!(parse_date_bound("3d", NOW).unwrap(), "2026-05-07");
        assert_eq!(parse_date_bound("12h", NOW).unwrap(), "2026-05-10");
        assert_eq!(parse_date_bound("30m", NOW).unwrap(), "2026-05-10");
        assert_eq!(parse_date_bound("2w", NOW).unwrap(), "2026-04-26");
        assert_eq!(parse_date_bound(" 3 D ", NOW).unwrap(), "2026-05-07");
    }

    #[test]
    fn relative_month_and_year_roll_like_js_setutc() {
        assert_eq!(parse_date_bound("3mo", NOW).unwrap(), "2026-02-10");
        // setUTCMonth(1) on Mar 31 gives Feb 31, which rolls to Mar 3 in a non-leap year.
        assert_eq!(
            parse_date_bound("1mo", noon(2026, 3, 31)).unwrap(),
            "2026-03-03"
        );
        // setUTCFullYear(2027) on Feb 29 gives Feb 29 2027, which rolls to Mar 1.
        assert_eq!(
            parse_date_bound("1y", noon(2028, 2, 29)).unwrap(),
            "2027-03-01"
        );
    }

    #[test]
    fn absolute_bounds_normalise_to_github_literals() {
        assert_eq!(parse_date_bound("2026-05-09", NOW).unwrap(), "2026-05-09");
        assert_eq!(
            parse_date_bound("2026-05-09T10:00:00.123Z", NOW).unwrap(),
            "2026-05-09T10:00:00Z"
        );
        assert_eq!(
            parse_date_bound("2026-05-09T12:00:00+02:00", NOW).unwrap(),
            "2026-05-09T10:00:00Z"
        );
        assert_eq!(
            parse_date_bound("2026-05-09T10:30-05:00", NOW).unwrap(),
            "2026-05-09T15:30:00Z"
        );
        assert_eq!(
            parse_date_bound("2026-05", NOW).unwrap(),
            "2026-05-01T00:00:00Z"
        );
    }

    #[test]
    fn invalid_bounds_are_errors() {
        assert_eq!(
            parse_date_bound("  ", NOW).unwrap_err(),
            "date bound must not be empty"
        );
        let error = parse_date_bound("soon", NOW).unwrap_err();
        assert!(
            error.starts_with("invalid date bound: soon. Expected"),
            "{error}"
        );
        assert!(parse_date_bound("2026-05-09T25:00Z", NOW).is_err());
        assert!(parse_date_bound("99999999999999999999d", NOW).is_err());
    }

    #[test]
    fn date_qualifier_forms() {
        assert_eq!(
            date_qualifier("created", Some("3d"), Some("1d"), NOW)
                .unwrap()
                .as_deref(),
            Some("created:2026-05-07..2026-05-09")
        );
        assert_eq!(
            date_qualifier("created", Some("3d"), None, NOW)
                .unwrap()
                .as_deref(),
            Some("created:>=2026-05-07")
        );
        assert_eq!(
            date_qualifier("created", None, Some("2026-01-31"), NOW)
                .unwrap()
                .as_deref(),
            Some("created:<=2026-01-31")
        );
        assert_eq!(
            date_qualifier("created", Some(""), None, NOW).unwrap(),
            None
        );
        assert_eq!(date_qualifier("created", None, None, NOW).unwrap(), None);
    }

    #[test]
    fn date_field_per_command() {
        let none = json!({});
        let updated = json!({"dateField": "updated"});
        assert_eq!(date_field("issues", &none).unwrap(), "created");
        assert_eq!(date_field("prs", &updated).unwrap(), "updated");
        assert_eq!(date_field("repos", &updated).unwrap(), "pushed");
        assert_eq!(
            date_field("repos", &json!({"dateField": "created"})).unwrap(),
            "created"
        );
        assert_eq!(date_field("commits", &updated).unwrap(), "committer-date");
        assert!(date_field("issues", &json!({"dateField": "closed"})).is_err());
    }

    #[test]
    fn compose_query_drops_blanks_and_requires_content() {
        assert_eq!(
            compose_query(&[Some(" a "), None, Some(""), Some("b")]).unwrap(),
            "a b"
        );
        assert_eq!(
            compose_query(&[None, Some("  ")]).unwrap_err(),
            "query is required (or pass since/until to filter by date)"
        );
    }

    #[test]
    fn repo_scope_helpers() {
        assert_eq!(split_repo_ref("acme/widgets"), (None, "acme/widgets"));
        assert_eq!(
            split_repo_ref("ghe.example.com/acme/widgets"),
            (Some("ghe.example.com"), "acme/widgets")
        );
        assert_eq!(split_repo_ref("a/b/c/d"), (None, "a/b/c/d"));
        assert!(has_scope_qualifier("fix org:acme"));
        assert!(has_scope_qualifier("-repo:acme/widgets bug"));
        assert!(has_scope_qualifier("OWNER:acme"));
        assert!(!has_scope_qualifier("reporter:octo repo:"));
        assert!(!has_scope_qualifier("docs-repo:x"));
        assert_eq!(
            repo_from_url("https://github.com/acme/widgets/pull/3").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            repo_from_url("https://ghe.example.com/acme/widgets.git").as_deref(),
            Some("ghe.example.com/acme/widgets.git")
        );
        assert_eq!(repo_from_url("not a url"), None);
        assert_eq!(
            repo_from_repository_url("https://api.github.com/repos/acme/widgets").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            repo_from_repository_url("https://ghe.example.com/api/v3/repos/acme/widgets")
                .as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            repo_from_repository_url("https://x.example/repos/a/b/c"),
            None
        );
    }

    #[test]
    fn civil_round_trip_and_fixed_now() {
        assert_eq!(civil_from_days(days_from_civil(2026, 5, 10)), (2026, 5, 10));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(format_date(i128::from(NOW)).as_deref(), Some("2026-05-10"));
    }

    #[test]
    fn api_argv_shape() {
        assert_eq!(
            api_search_args("issues", "fix repo:acme/widgets", 5, None, &[]),
            strings(&[
                "api",
                "-X",
                "GET",
                "/search/issues",
                "-f",
                "q=fix repo:acme/widgets",
                "-F",
                "per_page=5"
            ])
        );
        assert_eq!(
            api_search_args(
                "code",
                "q",
                3,
                Some("ghe.example.com"),
                &[TEXT_MATCH_ACCEPT]
            ),
            strings(&[
                "api",
                "--hostname",
                "ghe.example.com",
                "-X",
                "GET",
                "/search/code",
                "-f",
                "q=q",
                "-F",
                "per_page=3",
                "-H",
                TEXT_MATCH_ACCEPT,
            ])
        );
    }

    #[test]
    fn unknown_op_is_an_error() {
        assert!(run_with("search_nothing", json!({"query": "x"})).is_err());
    }

    #[test]
    fn search_issues_builds_argv_and_formats_results() {
        let fake = FakeGh::new(ISSUES_JSON, "", 0);
        let out = run_with(
            "search_issues",
            json!({"query": "fix bug", "since": "3d", "repo": "acme/widgets", "limit": 5}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            fake.calls(),
            vec![strings(&[
                "api",
                "-X",
                "GET",
                "/search/issues",
                "-f",
                "q=fix bug created:>=2026-05-07 repo:acme/widgets is:issue",
                "-F",
                "per_page=5",
            ])]
        );
        assert_lines(
            &out.text,
            &[
                "# GitHub issues search",
                "Query: fix bug created:>=2026-05-07",
                "Repository: acme/widgets",
                "Results: 1",
                "- #42 Crash on save",
                "  Repo: acme/widgets",
                "  State: merged",
                "  Author: @octo",
                "  Labels: bug, p1",
                "  Created: 2026-05-01T09:00:00Z",
                "  Updated: 2026-05-02T09:00:00Z",
                "  URL: https://github.com/acme/widgets/pull/42",
            ],
        );
    }

    #[test]
    fn search_issues_without_repo_falls_back_to_checkout_or_no_scope() {
        // `gh repo view` answers with non-URL JSON, so no scope is added and the search still runs.
        let fake = FakeGh::new(r#"{"items":[]}"#, "", 0);
        let out = run_with("search_issues", json!({"query": "fix"})).unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][0], "repo");
        assert_eq!(
            calls[1],
            strings(&[
                "api",
                "-X",
                "GET",
                "/search/issues",
                "-f",
                "q=fix is:issue",
                "-F",
                "per_page=10"
            ])
        );
        assert_lines(&out.text, &["Query: fix", "No issues found."]);
        assert!(!out.text.contains("Repository:"));
    }

    #[test]
    fn search_prs_uses_pr_filter_and_updated_field() {
        let fake = FakeGh::new(r#"{"items":[]}"#, "", 0);
        let out = run_with(
            "search_prs",
            json!({"query": "org:acme label:bug", "dateField": "updated", "until": "2026-01-31"}),
        )
        .unwrap();
        assert_eq!(
            fake.calls(),
            vec![strings(&[
                "api",
                "-X",
                "GET",
                "/search/issues",
                "-f",
                "q=org:acme label:bug updated:<=2026-01-31 is:pr",
                "-F",
                "per_page=10",
            ])]
        );
        assert_lines(
            &out.text,
            &[
                "# GitHub pull requests search",
                "Query: org:acme label:bug updated:<=2026-01-31",
                "Results: 0",
                "No pull requests found.",
            ],
        );
    }

    #[test]
    fn search_prs_renders_pull_request_items() {
        let fake = FakeGh::new(ISSUES_JSON, "", 0);
        let out = run_with(
            "search_prs",
            json!({"query": "org:acme", "repo": "acme/widgets"}),
        )
        .unwrap();
        assert_eq!(fake.last_args().last().unwrap(), "per_page=10");
        assert!(
            fake.last_args()
                .contains(&"q=org:acme repo:acme/widgets is:pr".to_owned())
        );
        assert_lines(
            &out.text,
            &[
                "# GitHub pull requests search",
                "Repository: acme/widgets",
                "- #42 Crash on save",
            ],
        );
    }

    #[test]
    fn search_code_passes_text_match_header_and_formats_fragments() {
        let fake = FakeGh::new(CODE_JSON, "", 0);
        let out = run_with(
            "search_code",
            json!({"query": "fn main", "repo": "acme/widgets", "limit": 3}),
        )
        .unwrap();
        assert_eq!(
            fake.calls(),
            vec![strings(&[
                "api",
                "-X",
                "GET",
                "/search/code",
                "-f",
                "q=fn main repo:acme/widgets",
                "-F",
                "per_page=3",
                "-H",
                "Accept: application/vnd.github.text-match+json",
            ])]
        );
        assert_lines(
            &out.text,
            &[
                "# GitHub code search",
                "Query: fn main",
                "- src/main.rs",
                "  Repo: acme/widgets",
                "  Commit: 0123456789ab",
                "  URL: https://github.com/acme/widgets/blob/main/src/main.rs",
                "  Match: fn main() {",
            ],
        );
    }

    #[test]
    fn search_code_enterprise_repo_moves_host_to_hostname_flag() {
        let fake = FakeGh::new(r#"{"items":[]}"#, "", 0);
        let out = run_with(
            "search_code",
            json!({"query": "fn main", "repo": "ghe.example.com/acme/widgets"}),
        )
        .unwrap();
        assert_eq!(
            fake.calls(),
            vec![strings(&[
                "api",
                "--hostname",
                "ghe.example.com",
                "-X",
                "GET",
                "/search/code",
                "-f",
                "q=fn main repo:acme/widgets",
                "-F",
                "per_page=10",
                "-H",
                "Accept: application/vnd.github.text-match+json",
            ])]
        );
        assert_lines(
            &out.text,
            &[
                "Repository: ghe.example.com/acme/widgets",
                "No code matches found.",
            ],
        );
    }

    #[test]
    fn search_code_rejects_dates_and_empty_query() {
        assert!(
            run_with("search_code", json!({"query": "x", "since": "3d"}))
                .unwrap_err()
                .contains("since/until")
        );
        assert_eq!(
            run_with("search_code", json!({})).unwrap_err(),
            "query must not be empty"
        );
        assert_eq!(
            run_with("search_code", json!({"query": "   "})).unwrap_err(),
            "query must not be empty"
        );
    }

    #[test]
    fn search_commits_uses_committer_date_and_formats_commits() {
        let fake = FakeGh::new(COMMITS_JSON, "", 0);
        let out = run_with(
            "search_commits",
            json!({"query": "crash", "since": "2026-05-01", "repo": "acme/widgets", "dateField": "updated"}),
        )
        .unwrap();
        assert_eq!(
            fake.calls(),
            vec![strings(&[
                "api",
                "-X",
                "GET",
                "/search/commits",
                "-f",
                "q=crash committer-date:>=2026-05-01 repo:acme/widgets",
                "-F",
                "per_page=10",
            ])]
        );
        assert_lines(
            &out.text,
            &[
                "# GitHub commits search",
                "Query: crash committer-date:>=2026-05-01",
                "Repository: acme/widgets",
                "Results: 1",
                "- 0123456789ab fix: crash on save",
                "  Repo: acme/widgets",
                "  Author: @octo",
                "  Date: 2026-05-04T10:00:00Z",
                "  URL: https://github.com/acme/widgets/commit/0123",
            ],
        );
    }

    #[test]
    fn search_repos_maps_updated_to_pushed_and_formats_repos() {
        let fake = FakeGh::new(REPOS_JSON, "", 0);
        let out = run_with(
            "search_repos",
            json!({"query": "rust cli", "since": "1w", "dateField": "updated", "limit": 500}),
        )
        .unwrap();
        assert_eq!(
            fake.calls(),
            vec![strings(&[
                "api",
                "-X",
                "GET",
                "/search/repositories",
                "-f",
                "q=rust cli pushed:>=2026-05-03",
                "-F",
                "per_page=50",
            ])]
        );
        assert_lines(
            &out.text,
            &[
                "# GitHub repositories search",
                "Query: rust cli pushed:>=2026-05-03",
                "Results: 1",
                "- acme/widgets",
                "  Description: A widget",
                "  Language: Rust",
                "  Stars: 12",
                "  Forks: 3",
                "  Open issues: 1",
                "  Visibility: public",
                "  Archived: false",
                "  Fork: false",
                "  Updated: 2026-05-08T00:00:00Z",
                "  URL: https://github.com/acme/widgets",
            ],
        );
        assert!(!out.text.contains("Repository:"));
    }

    #[test]
    fn search_failure_is_an_error_not_a_panic() {
        let _fake = FakeGh::new("", "HTTP 422: validation failed", 1);
        assert!(run_with("search_repos", json!({"query": "x"})).is_err());
    }
}
