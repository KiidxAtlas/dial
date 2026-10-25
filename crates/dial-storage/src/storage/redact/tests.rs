use super::*;
use dial_core::credentials::env_secrets;

fn redact(text: &str) -> (String, usize) {
    let (replaced, count) = redact_with(text, &[]);
    (replaced.unwrap_or_else(|| text.to_owned()), count)
}

#[test]
fn vendor_tokens_are_replaced_and_identifiers_survive() {
    let gh = format!("ghp_{}", "a1B2".repeat(9));
    let aws = "AKIAABCDEFGHIJKLMNOP";
    let text = format!("token={gh} key {aws} token_expiry_seconds=3600 sk-short");
    let (out, count) = redact(&text);
    assert_eq!(count, 2, "{out}");
    assert!(!out.contains(&gh) && !out.contains(aws));
    assert!(out.contains("[REDACTED:Credential]") && out.contains("[REDACTED:AWSAccessKey]"));
    assert!(out.contains("token_expiry_seconds=3600 sk-short"));
}

#[test]
fn boundaries_reject_embedded_shapes() {
    // A longer word containing an AWS-shaped run is not a key (lookbehind/lookahead).
    let (out, count) = redact("xAKIAABCDEFGHIJKLMNOP AKIAABCDEFGHIJKLMNOPQ");
    assert_eq!(count, 0, "{out}");
}

#[test]
fn bearer_pem_url_and_jwt_are_replaced() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop";
    let text = format!(
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345\n-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAK\n-----END RSA PRIVATE KEY-----\nhttps://alice:hunter2pass@example.com/x {jwt}"
    );
    let (out, count) = redact(&text);
    assert_eq!(count, 4, "{out}");
    assert!(out.contains("Bearer [REDACTED:BearerToken]"));
    assert!(out.contains("[REDACTED:PrivateKey]") && !out.contains("MIIBOgIBAAJBAK"));
    assert!(out.contains("https://alice:[REDACTED:UrlPassword]@example.com/x"));
    assert!(!out.contains(jwt));
}

#[test]
fn secret_named_environment_values_are_replaced_literally() {
    let env = env_secrets([
        ("MY_API_KEY".to_owned(), "zq9-literal-value".to_owned()),
        ("HOME".to_owned(), "/Users/someone".to_owned()),
        ("DB_PASSWORD".to_owned(), "short".to_owned()),
        ("AUTH_PATH".to_owned(), "/etc/secret/path".to_owned()),
    ]);
    assert_eq!(env, vec!["zq9-literal-value".to_owned()]);
    let (out, count) = redact_with("echo zq9-literal-value short", &env);
    assert_eq!(count, 1);
    assert_eq!(out.unwrap(), "echo [REDACTED:EnvSecret] short");
}

#[test]
fn multiline_environment_secrets_are_replaced_as_whole_values() {
    let secret = "literal secret spans\nmultiple lines of prose";
    let env = env_secrets([
        ("AUTH_TOKEN".to_owned(), secret.to_owned()),
        ("EMPTY_SECRET".to_owned(), "         \n".to_owned()),
    ]);
    assert_eq!(env, vec![secret.to_owned()]);
    let (out, count) = redact_with(&format!("Before\n{secret}\nAfter"), &env);
    assert_eq!(count, 1);
    assert_eq!(out.unwrap(), "Before\n[REDACTED:EnvSecret]\nAfter");
}

#[test]
fn json_values_are_walked_and_counted() {
    let gh = format!("github_pat_{}", "x".repeat(40));
    let mut value = serde_json::json!({"a":[gh.clone(), {"b": format!("say {gh}")}], "n": 3});
    assert_eq!(redact_value(&mut value), 2);
    assert!(!value.to_string().contains(&gh));
    assert_eq!(value["n"], 3);
}
