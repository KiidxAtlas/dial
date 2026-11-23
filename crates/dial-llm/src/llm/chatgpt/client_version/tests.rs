use super::*;
use serde_json::json;

#[test]
fn newest_takes_the_highest_stable_release_and_never_goes_below_bundled() {
    assert_eq!(newest([None, None]), BUNDLED);
    assert_eq!(newest([Some("0.1.0")]), BUNDLED);
    assert_eq!(newest([Some("0.169.0"), Some("0.160.2")]), "0.169.0");
    assert_eq!(newest([Some("0.159.10")]), "0.159.10");
    assert_eq!(
        newest([Some("1.0.0-alpha.1"), Some("9.9"), Some("x.y.z")]),
        BUNDLED
    );
}

#[test]
fn parse_accepts_only_plain_semver_releases() {
    assert_eq!(parse("0.169.0"), Some((0, 169, 0)));
    assert_eq!(parse("10.2.33"), Some((10, 2, 33)));
    for bad in [
        "",
        "0.169",
        "0.169.0.1",
        "0.169.0-beta",
        "v0.169.0",
        "0..1",
        "0.1.+1",
    ] {
        assert_eq!(parse(bad), None, "{bad}");
    }
}

#[test]
fn checks_are_due_daily_and_after_clock_corrections() {
    assert!(due(CHECK_INTERVAL_SECS, 0));
    assert!(!due(CHECK_INTERVAL_SECS - 1, 0));
    assert!(due(10, 20));
}

#[test]
fn refresh_fetches_once_a_day_caches_and_prefers_newer_local_codex() {
    let dir = tempfile::tempdir().unwrap();
    let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
        200,
        &json!({"name":"@openai/codex","version":"0.170.1"}),
    )]);
    let url = format!("{}/@openai/codex/latest", server.url);
    assert_eq!(refresh(dir.path(), None, &url), "0.170.1");
    assert_eq!(server.requests().len(), 1);
    // Cached: no second request inside the interval, and a newer local Codex wins.
    let codex = json!({"client_version":"0.171.0"});
    assert_eq!(refresh(dir.path(), Some(&codex), &url), "0.171.0");
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        read_stamp(&cache_path(dir.path())).version.as_deref(),
        Some("0.170.1")
    );
}

#[test]
fn failed_or_invalid_registry_answers_keep_the_previous_release() {
    let dir = tempfile::tempdir().unwrap();
    let stale = Stamp {
        checked_at: 0,
        version: Some("0.165.0".into()),
    };
    write_private(
        &cache_path(dir.path()),
        &serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
        200,
        &json!({"version":"1.0.0-rc.1"}),
    )]);
    let url = format!("{}/latest", server.url);
    assert_eq!(refresh(dir.path(), None, &url), "0.165.0");
    let stamp = read_stamp(&cache_path(dir.path()));
    assert_eq!(stamp.version.as_deref(), Some("0.165.0"));
    assert!(stamp.checked_at > 0);
}

#[test]
fn only_a_codex_cache_from_a_newer_client_counts_as_newer() {
    let cache = |version: &str| json!({"client_version": version});
    assert!(codex_is_newer(Some(&cache("0.172.0")), "0.171.0"));
    assert!(codex_is_newer(Some(&cache("0.172.0-alpha.3")), "0.171.0"));
    for same_or_older in ["0.171.0", "0.170.9", "0.171.0-alpha.1", "junk"] {
        assert!(
            !codex_is_newer(Some(&cache(same_or_older)), "0.171.0"),
            "{same_or_older}"
        );
    }
    assert!(!codex_is_newer(Some(&json!({})), "0.171.0"));
    assert!(!codex_is_newer(None, "0.171.0"));
}
