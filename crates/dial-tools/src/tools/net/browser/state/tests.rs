use serde_json::json;

use super::*;

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

#[test]
fn cookie_names_must_be_rfc6265_tokens() {
    assert!(validate_cookie_name("session_id").is_ok());
    assert!(validate_cookie_name("a b").is_err());
    assert!(validate_cookie_name("a;b").is_err());
    assert!(validate_cookie_name("").is_err());
}

#[test]
fn set_cookies_requires_exactly_one_of_url_or_domain() {
    let state = dir();
    let both = json!({"cookies": [{"name": "a", "value": "1", "url": "https://x.test", "domain": "x.test"}]});
    let neither = json!({"cookies": [{"name": "a", "value": "1"}]});
    assert!(prepare("set_cookies", &both, state.path()).is_err());
    assert!(prepare("set_cookies", &neither, state.path()).is_err());
}

#[test]
fn set_cookies_accepts_domain_with_defaulted_path_and_canonical_same_site() {
    let state = dir();
    let args = json!({"cookies": [{"name": "a", "value": "1", "domain": ".Example.com", "sameSite": "lax"}]});
    let prepared = prepare("set_cookies", &args, state.path())
        .expect("valid cookie")
        .expect("driver fields");
    let cookie = &prepared["cookies"][0];
    assert_eq!(cookie["domain"], "example.com");
    assert_eq!(cookie["path"], "/");
    assert_eq!(cookie["sameSite"], "Lax");
}

#[test]
fn set_cookies_rejects_unknown_same_site_and_oversized_batches() {
    let state = dir();
    let bad_same_site = json!({"cookies": [{"name": "a", "value": "1", "url": "https://x.test", "sameSite": "maybe"}]});
    assert!(prepare("set_cookies", &bad_same_site, state.path()).is_err());
    let cookies: Vec<_> = (0..=MAX_COOKIE_BATCH)
        .map(|i| json!({"name": format!("c{i}"), "value": "1", "url": "https://x.test"}))
        .collect();
    assert!(prepare("set_cookies", &json!({"cookies": cookies}), state.path()).is_err());
}

#[test]
fn cookie_values_are_redacted_unless_requested() {
    let mut result = json!({"cookies": [
        {"name": "a", "value": "secret", "domain": "example.com"},
        {"name": "b", "value": "other", "domain": "elsewhere.test"},
    ]});
    finish(
        "cookies",
        &json!({"domain": "shop.example.com"}),
        dir().path(),
        &mut result,
    )
    .expect("finish");
    let cookies = result["cookies"].as_array().expect("cookies");
    assert_eq!(
        cookies.len(),
        1,
        "domain filter keeps parent-domain cookies only"
    );
    assert!(cookies[0].get("value").is_none());
    assert_eq!(result["count"], 1);
}

#[test]
fn cookie_values_are_returned_only_with_include_values() {
    let mut result = json!({"cookies": [{"name": "a", "value": "secret", "domain": "x.test"}]});
    finish(
        "cookies",
        &json!({"include_values": true}),
        dir().path(),
        &mut result,
    )
    .expect("finish");
    assert_eq!(result["cookies"][0]["value"], "secret");
}

#[test]
fn cookie_listing_is_truncated_and_reports_total() {
    let cookies: Vec<_> = (0..MAX_COOKIES_RETURNED + 5)
        .map(|i| json!({"name": format!("c{i}"), "domain": "x.test"}))
        .collect();
    let mut result = json!({"cookies": cookies});
    finish("cookies", &json!({}), dir().path(), &mut result).expect("finish");
    assert_eq!(
        result["cookies"].as_array().map(Vec::len),
        Some(MAX_COOKIES_RETURNED)
    );
    assert_eq!(result["count"], MAX_COOKIES_RETURNED + 5);
    assert_eq!(result["truncated"], true);
}

#[test]
fn storage_set_needs_key_and_string_value() {
    let state = dir();
    assert!(
        prepare(
            "storage",
            &json!({"area": "local", "operation": "set", "key": "k"}),
            state.path()
        )
        .is_err()
    );
    assert!(
        prepare(
            "storage",
            &json!({"area": "local", "operation": "set", "key": "k", "value": 3}),
            state.path()
        )
        .is_err()
    );
    assert!(
        prepare(
            "storage",
            &json!({"area": "session", "operation": "set", "key": "k", "value": "v"}),
            state.path()
        )
        .is_ok()
    );
}

#[test]
fn storage_clear_rejects_key_and_get_rejects_value() {
    let state = dir();
    assert!(
        prepare(
            "storage",
            &json!({"area": "local", "operation": "clear", "key": "k"}),
            state.path()
        )
        .is_err()
    );
    assert!(
        prepare(
            "storage",
            &json!({"area": "local", "operation": "get", "value": "v"}),
            state.path()
        )
        .is_err()
    );
    assert!(
        prepare(
            "storage",
            &json!({"area": "cookie", "operation": "get"}),
            state.path()
        )
        .is_err()
    );
}

#[test]
fn oversized_storage_output_is_refused_instead_of_truncated() {
    let big = "x".repeat(MAX_OUTPUT_CHARS + 1);
    let mut result = json!({"entries": {"k": big}});
    let error = finish(
        "storage",
        &json!({"area": "local", "operation": "get"}),
        dir().path(),
        &mut result,
    )
    .expect_err("oversized entries");
    assert!(error.contains("key"));
}

#[test]
fn clipboard_read_truncates_and_marks_output() {
    let mut result = json!({"text": "y".repeat(MAX_OUTPUT_CHARS + 10)});
    finish("clipboard_read", &json!({}), dir().path(), &mut result).expect("finish");
    assert_eq!(
        result["text"].as_str().map(|t| t.chars().count()),
        Some(MAX_OUTPUT_CHARS)
    );
    assert_eq!(result["truncated"], true);
}

#[test]
fn clipboard_write_rejects_oversized_text() {
    let text = "z".repeat(MAX_CLIPBOARD_TEXT_CHARS + 1);
    assert!(prepare("clipboard_write", &json!({"text": text}), dir().path()).is_err());
}

#[test]
fn state_names_cannot_escape_the_states_directory() {
    let state = dir();
    assert!(prepare("save_state", &json!({"name": "../escape"}), state.path()).is_err());
    assert!(prepare("save_state", &json!({}), state.path()).is_err());
}

#[test]
fn save_then_load_round_trips_cookies_and_local_storage_only() {
    let state = dir();
    let mut saved = json!({"storage_state": {
        "cookies": [{"name": "sid", "value": "abc", "domain": "x.test", "path": "/", "expires": -1,
                     "httpOnly": true, "secure": false, "sameSite": "Lax"}],
        "origins": [{"origin": "https://x.test",
                     "localStorage": [{"name": "theme", "value": "dark"}]}],
    }});
    finish(
        "save_state",
        &json!({"name": "work"}),
        state.path(),
        &mut saved,
    )
    .expect("save");
    assert_eq!(saved, json!({"name": "work", "cookies": 1, "origins": 1}));

    let loaded = prepare("load_state", &json!({"name": "work"}), state.path())
        .expect("load")
        .expect("driver fields");
    let restored = &loaded["loaded_state"];
    assert_eq!(restored["cookies"][0]["name"], "sid");
    assert_eq!(restored["origins"][0]["origin"], "https://x.test");
    assert_eq!(restored["origins"][0]["localStorage"][0]["value"], "dark");
}

#[test]
fn load_state_fails_for_missing_state() {
    let state = dir();
    let error =
        prepare("load_state", &json!({"name": "absent"}), state.path()).expect_err("missing");
    assert!(error.contains("absent"));
}

#[test]
fn loaded_origins_keep_no_session_storage() {
    let stored = json!({
        "cookies": [],
        "origins": [{"origin": "https://x.test", "sessionStorage": [{"name": "s", "value": "v"}]}],
    });
    let parsed = parse_loaded_state(&stored).expect("valid state");
    assert!(parsed["origins"][0].get("sessionStorage").is_none());
    assert_eq!(parsed["origins"][0]["localStorage"], json!([]));
}
