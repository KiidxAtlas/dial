//! In-memory TTL cache for `github` PR-diff reads.
//!
//! Within the soft TTL an entry is served directly. Past it the read is refetched, and a failed
//! refetch serves the stale entry. Past the hard TTL the entry is dropped. `gh.rs` routing calls
//! `cached` for `pr_diff` and `invalidate_for_op` after `pr_create`, `pr_push` and `pr_checkout`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;

use super::ToolOutput;
use super::gh_common::parse_repo_ref;
use super::gh_pr_diff::{PrDiffView, parse_pr_url};

/// Default soft TTL: entries younger than this are served directly.
const SOFT_TTL: Duration = Duration::from_secs(300);
/// Default hard TTL: entries older than this are dropped.
const HARD_TTL: Duration = Duration::from_secs(604_800);

/// Identity of one cached PR-diff view, keyed on (repo, kind, number). This cache only stores
/// `pr-diff` rows, so `variant` (`summary`, `file:<index>` or `all`) takes the kind's place
/// alongside the repo and number.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CacheKey {
    /// Normalized `owner/repo` or `host/owner/repo`; the workspace root for the current checkout.
    pub repo: String,
    pub number: u64,
    pub variant: String,
}

struct Entry {
    output: ToolOutput,
    stored: Instant,
}

static CACHE: LazyLock<Mutex<HashMap<CacheKey, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cache state for one key at a given instant.
enum Lookup {
    Fresh(ToolOutput),
    Stale(ToolOutput),
    Miss,
}

fn lookup_at(key: &CacheKey, now: Instant) -> Lookup {
    let mut cache = CACHE.lock();
    let Some(stored) = cache.get(key).map(|entry| entry.stored) else {
        return Lookup::Miss;
    };
    let age = now.saturating_duration_since(stored);
    if age > HARD_TTL {
        cache.remove(key);
        return Lookup::Miss;
    }
    let output = cache[key].output.clone();
    if age <= SOFT_TTL {
        Lookup::Fresh(output)
    } else {
        Lookup::Stale(output)
    }
}

/// Store a successful result; errors are never cached.
fn store_at(key: &CacheKey, output: &ToolOutput, now: Instant) {
    if output.is_error {
        return;
    }
    CACHE.lock().insert(
        key.clone(),
        Entry {
            output: output.clone(),
            stored: now,
        },
    );
}

/// Serve `key` from the cache, or run `fill` and cache its successful result.
pub(super) fn cached(
    key: &CacheKey,
    fill: impl FnOnce() -> Result<ToolOutput, String>,
) -> Result<ToolOutput, String> {
    cached_at(key, Instant::now(), fill)
}

fn cached_at(
    key: &CacheKey,
    now: Instant,
    fill: impl FnOnce() -> Result<ToolOutput, String>,
) -> Result<ToolOutput, String> {
    match lookup_at(key, now) {
        Lookup::Fresh(output) => Ok(output),
        Lookup::Stale(stale) => match fill() {
            Ok(fresh) if !fresh.is_error => {
                store_at(key, &fresh, now);
                Ok(fresh)
            }
            _ => Ok(stale),
        },
        Lookup::Miss => {
            let fresh = fill()?;
            store_at(key, &fresh, now);
            Ok(fresh)
        }
    }
}

/// Drop entries matching `repo` and `number`; `None` matches every repo or every number.
pub(super) fn invalidate(repo: Option<&str>, number: Option<u64>) {
    CACHE.lock().retain(|key, _| {
        !(repo.is_none_or(|repo| key.repo == repo) && number.is_none_or(|n| key.number == n))
    });
}

/// Invalidate after a mutating `github` op. `pr_create` can add a PR to any list, and an unnamed
/// PR for `pr_push` or `pr_checkout` can refer to any diff, so those clear every entry. Named PR
/// numbers invalidate only their diffs, in every repo.
pub(super) fn invalidate_for_op(op: &str, args: &Value) {
    if op == "pr_create" {
        return invalidate(None, None);
    }
    if !matches!(op, "pr_push" | "pr_checkout") {
        return;
    }
    let numbers: Option<Vec<u64>> = match &args["pr"] {
        Value::String(text) => pr_number(text).map(|n| vec![n]),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().and_then(pr_number))
            .collect(),
        _ => None,
    };
    match numbers {
        Some(numbers) if !numbers.is_empty() => {
            for number in numbers {
                invalidate(None, Some(number));
            }
        }
        _ => invalidate(None, None),
    }
}

/// PR number from a bare number or a PR URL.
fn pr_number(text: &str) -> Option<u64> {
    let text = text.trim();
    text.parse()
        .ok()
        .or_else(|| parse_pr_url(text).map(|target| target.number))
}

/// Cache key for a `pr_diff` call, or `None` when `pr` is missing or unreadable (the op then runs
/// uncached and reports its own error).
pub(super) fn pr_diff_key(checkout: &Path, args: &Value) -> Option<CacheKey> {
    let pr = match &args["pr"] {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    let (number, url_repo, url_view) = match pr.parse::<u64>() {
        Ok(number) => (number, None, None),
        Err(_) => {
            let target = parse_pr_url(&pr)?;
            (target.number, target.repo, Some(target.view))
        }
    };
    let repo_arg = args["repo"]
        .as_str()
        .map(str::trim)
        .filter(|repo| !repo.is_empty());
    let repo = url_repo
        .or_else(|| repo_arg.map(str::to_owned))
        .map(|repo| {
            parse_repo_ref(&repo)
                .map(|parsed| parsed.qualified())
                .unwrap_or(repo)
        })
        .unwrap_or_default();
    // The current checkout's repo depends on the workspace, so key those entries by its root.
    let repo = if repo.is_empty() {
        checkout.display().to_string()
    } else {
        repo
    };
    let view = if args["all"].as_bool() == Some(true) {
        PrDiffView::All
    } else if let Some(index) = args["file"].as_u64() {
        PrDiffView::File(index as usize)
    } else {
        url_view.unwrap_or(PrDiffView::Summary)
    };
    let variant = match view {
        PrDiffView::Summary => "summary".to_owned(),
        PrDiffView::File(index) => format!("file:{index}"),
        PrDiffView::All => "all".to_owned(),
    };
    Some(CacheKey {
        repo,
        number,
        variant,
    })
}

#[cfg(test)]
mod tests;
