use super::*;
use serde_json::json;

#[test]
fn listing_exposes_only_identity_kind_status_and_expiry() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path();
    std::fs::write(
        dir.join("credentials.json"),
        serde_json::to_vec(&json!({
            "https://api.example.test/private-secret?key=query-secret": "api-secret"
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("oauth-copilot.json"),
        serde_json::to_vec(&json!({
            "access": "access-secret", "refresh": "refresh-secret",
            "email": "person@example.test", "expires_at_ms": 1000
        }))
        .unwrap(),
    )
    .unwrap();
    for (file, value) in [
        (
            "chatgpt-accounts.json",
            json!({"accounts": {
                "account": {"email": "chat@example.test", "tokens": null}
            }}),
        ),
        (
            "anthropic-subscription.json",
            json!({"tokens": {
                "access_token": "subscription-secret", "expires_at": 1
            }}),
        ),
    ] {
        std::fs::write(dir.join(file), serde_json::to_vec(&value).unwrap()).unwrap();
    }
    // Inspect the ordinary provider records independently of a user's shared subscription override.
    let rows = list_in(dir, dir).unwrap();
    let api = rows.iter().find(|row| row.kind == "api key").unwrap();
    assert_eq!(api.label, "api.example.test");
    let oauth = rows.iter().find(|row| row.provider == "copilot").unwrap();
    assert_eq!(oauth.label, "person@example.test");
    assert_eq!(oauth.expires_at, Some(1));
    assert_eq!(oauth.status, "expired (refresh on use)");
    for (provider, status) in [
        ("chatgpt", "signed out"),
        ("anthropic", "expired (refresh on use)"),
    ] {
        let row = rows.iter().find(|row| row.provider == provider).unwrap();
        assert_eq!(row.kind, "subscription");
        assert_eq!(row.status, status);
    }
    let displayed = format!("{rows:?}");
    for secret in [
        "private-secret",
        "query-secret",
        "api-secret",
        "access-secret",
        "refresh-secret",
        "subscription-secret",
    ] {
        assert!(!displayed.contains(secret));
    }
}

#[test]
fn migrated_sign_out_never_resurrects_legacy_grants() {
    let home = tempfile::tempdir().unwrap();
    let legacy = home.path().join("oauth-copilot.json");
    std::fs::write(
        &legacy,
        serde_json::to_vec(&json!({
            "access": "not-a-real-token", "expires_at_ms": 1000
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(super::super::oauth::signed_in(home.path(), "copilot"));
    super::super::oauth::sign_out(home.path(), "copilot").unwrap();
    assert!(legacy.exists());
    assert!(!super::super::oauth::signed_in(home.path(), "copilot"));
    assert!(!super::super::oauth::signed_in(home.path(), "copilot"));
}
