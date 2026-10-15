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
mod tests {
    use super::*;
    use serde_json::json;

    fn key(repo: &str, number: u64) -> CacheKey {
        CacheKey {
            repo: repo.into(),
            number,
            variant: "summary".into(),
        }
    }

    fn out(text: &str) -> ToolOutput {
        ToolOutput {
            execution: Default::default(),
            text: text.into(),
            is_error: false,
            data: None,
        }
    }

    #[test]
    fn entries_are_fresh_then_stale_then_dropped() {
        let k = key("cache-test/ttl", 1);
        let t0 = Instant::now();
        store_at(&k, &out("diff"), t0);
        assert!(matches!(lookup_at(&k, t0 + SOFT_TTL), Lookup::Fresh(o) if o.text == "diff"));
        assert!(matches!(
            lookup_at(&k, t0 + SOFT_TTL + Duration::from_secs(1)),
            Lookup::Stale(o) if o.text == "diff"
        ));
        assert!(matches!(
            lookup_at(&k, t0 + HARD_TTL + Duration::from_secs(1)),
            Lookup::Miss
        ));
        assert!(matches!(lookup_at(&k, t0), Lookup::Miss));
    }

    #[test]
    fn hit_skips_fill_and_miss_fills_once() {
        let k = key("cache-test/hit", 2);
        let t0 = Instant::now();
        let mut fills = 0;
        let first = cached_at(&k, t0, || {
            fills += 1;
            Ok(out("one"))
        })
        .unwrap();
        assert_eq!(first.text, "one");
        let second = cached_at(&k, t0, || {
            fills += 1;
            Ok(out("two"))
        })
        .unwrap();
        assert_eq!(second.text, "one");
        assert_eq!(fills, 1);
    }

    #[test]
    fn stale_entries_refresh_and_fall_back_when_refresh_fails() {
        let k = key("cache-test/stale", 3);
        let t0 = Instant::now();
        store_at(&k, &out("old"), t0);
        let later = t0 + SOFT_TTL + Duration::from_secs(1);
        let fallback = cached_at(&k, later, || Err("network".into())).unwrap();
        assert_eq!(fallback.text, "old");
        let refreshed = cached_at(&k, later, || Ok(out("new"))).unwrap();
        assert_eq!(refreshed.text, "new");
        assert!(matches!(lookup_at(&k, later), Lookup::Fresh(o) if o.text == "new"));
    }

    #[test]
    fn errors_are_not_cached() {
        let k = key("cache-test/error", 4);
        let t0 = Instant::now();
        let result = cached_at(&k, t0, || Ok(ToolOutput::err("boom"))).unwrap();
        assert!(result.is_error);
        assert!(matches!(lookup_at(&k, t0), Lookup::Miss));
    }

    #[test]
    fn invalidate_by_repo_or_by_number() {
        let now = Instant::now();
        store_at(&key("cache-test/inv-a", 5), &out("a"), now);
        store_at(&key("cache-test/inv-b", 5), &out("b"), now);
        invalidate(Some("cache-test/inv-a"), None);
        assert!(matches!(
            lookup_at(&key("cache-test/inv-a", 5), now),
            Lookup::Miss
        ));
        assert!(matches!(
            lookup_at(&key("cache-test/inv-b", 5), now),
            Lookup::Fresh(_)
        ));

        store_at(&key("cache-test/inv-c", 9_000_011), &out("c"), now);
        store_at(&key("cache-test/inv-c", 9_000_012), &out("d"), now);
        invalidate(None, Some(9_000_011));
        assert!(matches!(
            lookup_at(&key("cache-test/inv-c", 9_000_011), now),
            Lookup::Miss
        ));
        assert!(matches!(
            lookup_at(&key("cache-test/inv-c", 9_000_012), now),
            Lookup::Fresh(_)
        ));
    }

    #[test]
    fn pr_push_invalidates_only_the_named_number() {
        let now = Instant::now();
        store_at(&key("cache-test/push", 9_000_021), &out("x"), now);
        store_at(&key("cache-test/push", 9_000_022), &out("y"), now);
        invalidate_for_op("pr_push", &json!({"pr": "9000021"}));
        assert!(matches!(
            lookup_at(&key("cache-test/push", 9_000_021), now),
            Lookup::Miss
        ));
        assert!(matches!(
            lookup_at(&key("cache-test/push", 9_000_022), now),
            Lookup::Fresh(_)
        ));
        invalidate_for_op("run_watch", &json!({"pr": "9000022"}));
        assert!(matches!(
            lookup_at(&key("cache-test/push", 9_000_022), now),
            Lookup::Fresh(_)
        ));
    }

    #[test]
    fn pr_diff_key_normalizes_repo_host_and_view() {
        assert_eq!(
            pr_diff_key(
                Path::new("/w"),
                &json!({"pr": "42", "repo": "github.com/octo/repo"})
            )
            .unwrap(),
            CacheKey {
                repo: "octo/repo".into(),
                number: 42,
                variant: "summary".into()
            }
        );
        assert_eq!(
            pr_diff_key(
                Path::new("/w"),
                &json!({"pr": 42, "repo": "ghe.example.com/octo/repo", "file": 3})
            )
            .unwrap(),
            CacheKey {
                repo: "ghe.example.com/octo/repo".into(),
                number: 42,
                variant: "file:3".into()
            }
        );
        assert_eq!(
            pr_diff_key(Path::new("/w"), &json!({"pr": "42", "all": true})).unwrap(),
            CacheKey {
                repo: "/w".into(),
                number: 42,
                variant: "all".into()
            }
        );
        assert!(pr_diff_key(Path::new("/w"), &json!({"repo": "octo/repo"})).is_none());
    }
}
