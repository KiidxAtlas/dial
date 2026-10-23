//! In-memory TTL cache for `github` PR-diff reads.
//!
//! Within the soft TTL an entry is served directly. Past it the read is refetched, and a failed
//! refetch serves the stale entry with a note saying so (never when the call was cancelled). Past
//! the hard TTL the entry is dropped, and the map holds at most [`MAX_ENTRIES`] (oldest evicted). `gh.rs` routing calls
//! `cached` for `pr_diff` and `invalidate_for_op` after `pr_create`, `pr_push` and `pr_checkout`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;

use super::gh_common::parse_repo_ref;
use super::gh_pr_diff::{PrDiffView, parse_pr_url, parse_request};
use crate::tools::ToolOutput;

/// Default soft TTL: entries younger than this are served directly.
const SOFT_TTL: Duration = Duration::from_secs(300);
/// Default hard TTL: entries older than this are dropped.
const HARD_TTL: Duration = Duration::from_secs(604_800);

/// Most entries kept; storing past this evicts the oldest (diffs can be large).
const MAX_ENTRIES: usize = 64;

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
    Stale { output: ToolOutput, stored: Instant },
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
        Lookup::Stale { output, stored }
    }
}

/// Store a successful result; errors are never cached.
fn store_at(key: &CacheKey, output: &ToolOutput, now: Instant) {
    if output.is_error {
        return;
    }
    insert_capped(
        &mut CACHE.lock(),
        key.clone(),
        Entry {
            output: output.clone(),
            stored: now,
        },
    );
}

/// Insert `entry`, then evict the oldest entries while the map exceeds [`MAX_ENTRIES`].
fn insert_capped(cache: &mut HashMap<CacheKey, Entry>, key: CacheKey, entry: Entry) {
    cache.insert(key, entry);
    while cache.len() > MAX_ENTRIES {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.stored)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        cache.remove(&oldest);
    }
}

/// Serve `key` from the cache, or run `fill` and cache its successful result. `cancel` is the
/// call's cancellation flag: a cancelled refresh is reported, never papered over with stale text.
pub(super) fn cached(
    key: &CacheKey,
    cancel: &AtomicBool,
    fill: impl FnOnce() -> Result<ToolOutput, String>,
) -> Result<ToolOutput, String> {
    cached_at(key, Instant::now(), cancel, fill)
}

fn cached_at(
    key: &CacheKey,
    now: Instant,
    cancel: &AtomicBool,
    fill: impl FnOnce() -> Result<ToolOutput, String>,
) -> Result<ToolOutput, String> {
    match lookup_at(key, now) {
        Lookup::Fresh(output) => Ok(output),
        Lookup::Stale { output, stored } => {
            let failure = match fill() {
                Ok(fresh) if !fresh.is_error => {
                    store_at(key, &fresh, now);
                    return Ok(fresh);
                }
                other if cancel.load(Ordering::Acquire) => return other,
                Ok(error) => error.text,
                Err(error) => error,
            };
            let age = now.saturating_duration_since(stored).as_secs();
            let mut stale = output;
            stale.text.push_str(&format!(
                "\n[served from cache, age {age}s; refresh failed: {}]",
                failure.trim()
            ));
            Ok(stale)
        }
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

/// Cache key for a `pr_diff` call, or `None` when the op would reject its arguments (it then runs
/// uncached and reports its own error, which a cached entry must never mask).
pub(super) fn pr_diff_key(checkout: &Path, args: &Value) -> Option<CacheKey> {
    let request = parse_request(args).ok()?;
    let repo = request
        .repo
        .map(|repo| {
            parse_repo_ref(&repo)
                .map(|parsed| parsed.qualified())
                .unwrap_or(repo)
        })
        // The current checkout's repo depends on the workspace, so key those entries by its root.
        .unwrap_or_else(|| checkout.display().to_string());
    let variant = match request.view {
        PrDiffView::Summary => "summary".to_owned(),
        PrDiffView::File(index) => format!("file:{index}"),
        PrDiffView::All => "all".to_owned(),
    };
    Some(CacheKey {
        repo,
        number: request.number,
        variant,
    })
}

#[cfg(test)]
mod tests;
