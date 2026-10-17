use super::*;

fn material<'a>(user: &'a str) -> CacheKeyMaterial<'a> {
    CacheKeyMaterial {
        operation: "primary",
        model: "provider/model",
        api_mode: "chat",
        tool_name: "",
        system_prompt: "system",
        user_prompt: user,
        reasoning_effort: None,
    }
}

fn open_temp(ttl_days: f64) -> (tempfile::TempDir, ConventionalCache) {
    let dir = tempfile::tempdir().unwrap();
    let cache =
        ConventionalCache::open(&dir.path().join("cache/commit-inference.db"), ttl_days).unwrap();
    (dir, cache)
}

fn entry<'a>(key: &'a str, text: &'a str) -> CacheEntry<'a> {
    CacheEntry {
        key,
        model: "provider/model",
        operation: "primary",
        request: "{}",
        text,
        finish_reason: "stop",
        cost_usd: None,
    }
}

#[test]
fn key_is_stable_and_sensitive_to_each_field() {
    let base = key_for(&material("diff a"));
    assert_eq!(base, key_for(&material("diff a")));
    assert_ne!(base, key_for(&material("diff b")));
    let mut other = material("diff a");
    other.model = "provider/other";
    assert_ne!(base, key_for(&other));
    other = material("diff a");
    other.operation = "fast";
    assert_ne!(base, key_for(&other));
    other = material("diff a");
    other.system_prompt = "system 2";
    assert_ne!(base, key_for(&other));
}

#[test]
fn empty_reasoning_effort_matches_absent() {
    let absent = key_for(&material("x"));
    let mut empty = material("x");
    empty.reasoning_effort = Some("");
    assert_eq!(absent, key_for(&empty));
    let mut high = material("x");
    high.reasoning_effort = Some("high");
    assert_ne!(absent, key_for(&high));
}

#[test]
fn put_then_get_round_trips_and_misses_unknown_keys() {
    let (_dir, cache) = open_temp(14.0);
    assert!(cache.get("k1").is_none());
    cache.put(&entry("k1", "feat: add thing"));
    let hit = cache.get("k1").unwrap();
    assert_eq!(hit.text, "feat: add thing");
    assert_eq!(hit.finish_reason, "stop");
    assert!(cache.get("k2").is_none());
}

#[test]
fn expired_rows_are_dropped_on_read() {
    let (_dir, cache) = open_temp(1.0);
    cache.put(&entry("old", "text"));
    cache
        .conn
        .lock()
        .execute(
            "UPDATE responses SET created_at = created_at - 200000 WHERE key = 'old'",
            [],
        )
        .unwrap();
    assert!(cache.get("old").is_none());
    let remaining: i64 = cache
        .conn
        .lock()
        .query_row("SELECT COUNT(*) FROM responses", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn zero_ttl_never_expires() {
    let (_dir, cache) = open_temp(0.0);
    cache.put(&entry("k", "text"));
    cache
        .conn
        .lock()
        .execute("UPDATE responses SET created_at = 1 WHERE key = 'k'", [])
        .unwrap();
    assert_eq!(cache.get("k").unwrap().text, "text");
}

#[test]
fn rows_from_another_schema_version_are_ignored() {
    let (_dir, cache) = open_temp(14.0);
    cache.put(&entry("k", "text"));
    cache
        .conn
        .lock()
        .execute(
            "UPDATE responses SET schema_version = 2 WHERE key = 'k'",
            [],
        )
        .unwrap();
    assert!(cache.get("k").is_none());
}

#[test]
fn failures_are_capped_to_newest_rows() {
    let (_dir, cache) = open_temp(14.0);
    for index in 0..(MAX_FAILURES + 6) {
        let key = format!("k{index}");
        cache.put_failure(&FailureEntry {
            key: &key,
            model: "provider/model",
            operation: "primary",
            request: "{}",
            response: "",
            error: "boom",
        });
    }
    let count: i64 = cache
        .conn
        .lock()
        .query_row("SELECT COUNT(*) FROM failures", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, MAX_FAILURES);
}

#[test]
fn usage_rows_record_reported_counts_and_cost() {
    let (_dir, cache) = open_temp(14.0);
    cache.record_usage(&UsageRecord {
        model: "provider/model",
        operation: "primary",
        input_tokens: Some(10),
        output_tokens: None,
        cache_read_tokens: Some(3),
        cache_write_tokens: None,
        cost_usd: Some(0.0042),
    });
    let (input, output): (Option<i64>, Option<i64>) = cache
        .conn
        .lock()
        .query_row("SELECT input_tokens, output_tokens FROM usage", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(input, Some(10));
    assert_eq!(output, None);
    let cost: Option<f64> = cache
        .conn
        .lock()
        .query_row("SELECT cost_usd FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cost, Some(0.0042));
}

#[test]
fn path_resolution_prefers_override_then_user_dir() {
    let user = Path::new("/home/u/.dial");
    assert_eq!(
        resolve_path(Some(OsString::from("/tmp/c.db")), user),
        Some(PathBuf::from("/tmp/c.db"))
    );
    assert_eq!(
        resolve_path(Some(OsString::new()), user),
        Some(user.join("cache/commit-inference.db"))
    );
    assert_eq!(resolve_path(None, Path::new("")), None);
}
