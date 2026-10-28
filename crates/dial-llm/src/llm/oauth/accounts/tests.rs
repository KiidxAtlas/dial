use super::*;
use crate::llm::key_pool::Blocks;
use std::time::Duration;

#[test]
fn first_insert_becomes_active_and_remove_moves_it() {
    let mut store: Store<u8> = Store::default();
    store.insert("a".into(), 1);
    store.insert("b".into(), 2);
    assert_eq!(store.active_key(), Some("a"));
    assert_eq!(store.remove("a"), Some(1));
    assert_eq!(store.active_key(), Some("b"));
    assert_eq!(store.remove("b"), Some(2));
    assert!(store.is_empty());
    assert_eq!(store.active_key(), None);
}

#[test]
fn select_requires_an_existing_account() {
    let mut store: Store<u8> = Store::default();
    store.insert("a".into(), 1);
    store.insert("b".into(), 2);
    assert!(store.select("b"));
    assert_eq!(store.active_key(), Some("b"));
    assert!(!store.select("missing"));
    assert_eq!(store.active_key(), Some("b"));
}

#[test]
fn stale_active_falls_back_to_first_account() {
    let store: Store<u8> = Store {
        active: Some("gone".into()),
        accounts: BTreeMap::from([("z".into(), 1), ("m".into(), 2)]),
    };
    assert_eq!(store.active_key(), Some("m"));
    assert_eq!(store.active(), Some(&2));
}

#[test]
fn legacy_single_grant_loads_as_default_account() {
    let store: Store<serde_json::Value> =
        parse(br#"{"access":"token","email":"a@example.com"}"#).unwrap();
    assert_eq!(store.active_key(), Some(LEGACY_KEY));
    assert_eq!(store.accounts.len(), 1);
    assert_eq!(store.accounts[LEGACY_KEY]["email"], "a@example.com");
}

#[test]
fn multi_account_document_parses_unchanged() {
    let bytes = br#"{"active":"work","accounts":{"home":{"v":1},"work":{"v":2}}}"#;
    let store: Store<serde_json::Value> = parse(bytes).unwrap();
    assert_eq!(store.active_key(), Some("work"));
    assert_eq!(store.accounts.len(), 2);
}

#[test]
fn unknown_fields_are_rejected_in_multi_account_document() {
    let bytes = br#"{"active":null,"accounts":{},"extra":1}"#;
    assert!(parse::<serde_json::Value>(bytes).is_err());
}

#[test]
fn rotation_is_round_robin_and_skips_rejected_accounts() {
    let store: Store<u8> = Store {
        active: None,
        accounts: BTreeMap::from([("a".into(), 1), ("b".into(), 2), ("c".into(), 3)]),
    };
    assert_eq!(
        next_account(&store, Some("a"), |_| false).as_deref(),
        Some("b")
    );
    assert_eq!(
        next_account(&store, Some("c"), |_| false).as_deref(),
        Some("a")
    );
    assert_eq!(
        next_account(&store, Some("a"), |key| key == "b").as_deref(),
        Some("c")
    );
    assert_eq!(
        next_account(&store, Some("a"), |key| key != "a").as_deref(),
        Some("a")
    );
    assert_eq!(next_account(&store, Some("a"), |_| true), None);
    assert_eq!(next_account(&store, None, |_| false).as_deref(), Some("a"));
}

#[test]
fn rate_limit_blocks_the_account_for_its_cooldown() {
    let blocks = Blocks::default();
    let now = SystemTime::now();
    let class = ErrorClass::RateLimited {
        retry_after: Some(Duration::from_secs(30)),
    };
    assert!(block_account(&blocks, "scope", "a", &class, None, now));
    assert!(is_blocked(&blocks, "scope", "a"));
    assert!(!is_blocked(&blocks, "scope", "b"));
    assert!(!is_blocked(&blocks, "other", "a"));
}

#[test]
fn rejected_credentials_cool_down_and_transient_failures_do_not() {
    let blocks = Blocks::default();
    let now = SystemTime::now();
    assert!(block_account(
        &blocks,
        "scope",
        "a",
        &ErrorClass::Auth,
        None,
        now
    ));
    assert!(is_blocked(&blocks, "scope", "a"));
    assert!(!block_account(
        &blocks,
        "scope",
        "b",
        &ErrorClass::Transient,
        None,
        now
    ));
    assert!(!is_blocked(&blocks, "scope", "b"));
}

#[test]
fn refresh_lock_path_is_per_account_and_hides_the_key() {
    let dir = Path::new("/tmp/dial-user");
    let a = refresh_lock_path(dir, "copilot", "alice@example.com");
    let b = refresh_lock_path(dir, "copilot", "bob@example.com");
    assert_ne!(a, b);
    assert_eq!(a, refresh_lock_path(dir, "copilot", "alice@example.com"));
    assert!(!a.to_string_lossy().contains("alice"));
    assert!(a.starts_with(dir));
}
