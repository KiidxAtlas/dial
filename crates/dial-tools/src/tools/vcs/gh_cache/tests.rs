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
