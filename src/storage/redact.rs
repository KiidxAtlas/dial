//! Credential redaction for transcripts that leave the machine (export, share).
//!
//! Credential patterns (`CREDENTIAL_PATTERNS`, `SENSITIVE_TOKEN_RE`) plus its
//! environment-secret collection. Only anchored vendor shapes, PEM private keys, bearer tokens,
//! URL userinfo passwords and the literal values of secret-named environment variables are
//! replaced: no generic keyword/entropy rules, so identifiers such as `token_expiry_seconds`
//! survive. Rust's `regex` has no look-around, so the look-behind/look-ahead boundaries
//! `(?<![A-Za-z0-9_-])`/`(?![A-Za-z0-9_-])` are checked explicitly around each match.
use regex::Regex;
use serde_json::Value;
use std::{borrow::Cow, sync::LazyLock};

/// Characters that may not touch a vendor token on either side.
fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}
/// `SENSITIVE_TOKEN_RE` also treats `*` as part of a token (partially masked tokens).
fn word_or_star(c: char) -> bool {
    word(c) || c == '*'
}

struct Pattern {
    name: &'static str,
    regex: Regex,
    /// Capture group holding the secret; 0 is the whole match.
    group: usize,
    /// Every match contains one of these; the regex is skipped otherwise.
    prefixes: &'static [&'static str],
    case_insensitive: bool,
    /// Boundary class checked before and after the secret span.
    boundary: Option<fn(char) -> bool>,
}

fn pattern(
    name: &'static str,
    source: &str,
    group: usize,
    prefixes: &'static [&'static str],
    case_insensitive: bool,
    boundary: Option<fn(char) -> bool>,
) -> Pattern {
    Pattern {
        name,
        regex: Regex::new(source).expect("credential pattern compiles"),
        group,
        prefixes,
        case_insensitive,
        boundary,
    }
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    vec![
        // PEM blocks first: their bodies would otherwise be scanned as unrelated tokens.
        pattern(
            "PrivateKey",
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            0,
            &["-----BEGIN "],
            false,
            None,
        ),
        pattern(
            "Credential",
            r"(?i)gh[opusr]_[a-z0-9_*]{36,}|github_pat_[a-z0-9_*]{36,}|glpat-[a-z0-9_*-]{20,}|sk-proj-[a-z0-9_*-]{36,}|sk-ant-[a-z0-9_*-]{36,}|sk-[a-z0-9_*-]{48,}",
            0,
            &[
                "gho_",
                "ghp_",
                "ghu_",
                "ghs_",
                "ghr_",
                "github_pat_",
                "glpat-",
                "sk-",
            ],
            true,
            Some(word_or_star),
        ),
        pattern(
            "AWSAccessKey",
            r"(?:AKIA|ASIA)[A-Z0-9]{16}",
            0,
            &["AKIA", "ASIA"],
            false,
            Some(word),
        ),
        pattern(
            "GoogleAPIKey",
            r"AIza[A-Za-z0-9_-]{30,}",
            0,
            &["AIza"],
            false,
            Some(word),
        ),
        pattern(
            "SlackToken",
            r"xox[abprs]-[A-Za-z0-9-]{10,}",
            0,
            &["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"],
            false,
            Some(word),
        ),
        pattern(
            "NpmToken",
            r"npm_[A-Za-z0-9]{30,}",
            0,
            &["npm_"],
            false,
            Some(word),
        ),
        pattern(
            "StripeKey",
            r"(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{20,}",
            0,
            &["sk_live_", "sk_test_", "rk_live_", "rk_test_"],
            false,
            Some(word),
        ),
        pattern(
            "StripeWebhookSecret",
            r"whsec_[A-Za-z0-9]{20,}",
            0,
            &["whsec_"],
            false,
            Some(word),
        ),
        pattern(
            "HuggingFaceToken",
            r"hf_[A-Za-z0-9]{30,}",
            0,
            &["hf_"],
            false,
            Some(word),
        ),
        pattern(
            "SendGridKey",
            r"SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}",
            0,
            &["SG."],
            false,
            Some(word),
        ),
        pattern(
            "JWT",
            r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            0,
            &["eyJ"],
            false,
            Some(word),
        ),
        // `(?<=\bBearer )token(?![A-Za-z0-9_-])`: the introducer stays, the token is replaced.
        pattern(
            "BearerToken",
            r"(?i)\bBearer ([A-Za-z0-9._~+/=-]{20,})",
            1,
            &["bearer "],
            true,
            None,
        ),
        // scheme://user:PASSWORD@host — only the password is replaced.
        pattern(
            "UrlPassword",
            r"[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+:([^\s/@]+)@",
            1,
            &["://"],
            false,
            None,
        ),
    ]
});

const SECRET_ENV_NAMES: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

/// Literal values of secret-named environment variables, longest first so a value containing
/// another is replaced whole.
static ENV_SECRETS: LazyLock<Vec<String>> =
    LazyLock::new(|| {
        env_secrets(std::env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }))
    });

fn env_secrets(vars: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    let mut values: Vec<String> = vars
        .into_iter()
        .filter(|(name, value)| {
            let upper = name.to_ascii_uppercase();
            SECRET_ENV_NAMES.iter().any(|needle| upper.contains(needle))
                && value.len() >= 8
                && !value.chars().any(char::is_whitespace)
                && !value.starts_with('/')
        })
        .map(|(_, value)| value)
        .collect();
    values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    values.dedup();
    values
}

fn marker(name: &str) -> String {
    format!("[REDACTED:{name}]")
}

fn apply(pattern: &Pattern, text: &str) -> Option<(String, usize)> {
    let present = if pattern.case_insensitive {
        let lower = text.to_ascii_lowercase();
        pattern
            .prefixes
            .iter()
            .any(|prefix| lower.contains(&prefix.to_ascii_lowercase()))
    } else {
        pattern.prefixes.iter().any(|prefix| text.contains(prefix))
    };
    if !present {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut count = 0;
    for captures in pattern.regex.captures_iter(text) {
        let Some(span) = captures.get(pattern.group) else {
            continue;
        };
        if let Some(boundary) = pattern.boundary {
            let before = text[..span.start()].chars().next_back();
            let after = text[span.end()..].chars().next();
            if before.is_some_and(boundary) || after.is_some_and(boundary) {
                continue;
            }
        }
        out.push_str(&text[last..span.start()]);
        out.push_str(&marker(pattern.name));
        last = span.end();
        count += 1;
    }
    if count == 0 {
        return None;
    }
    out.push_str(&text[last..]);
    Some((out, count))
}

fn redact_with(text: &str, env: &[String]) -> (Option<String>, usize) {
    let mut current: Option<String> = None;
    let mut total = 0;
    for value in env {
        let source = current.as_deref().unwrap_or(text);
        if source.contains(value.as_str()) {
            let hits = source.matches(value.as_str()).count();
            current = Some(source.replace(value.as_str(), &marker("EnvSecret")));
            total += hits;
        }
    }
    for pattern in PATTERNS.iter() {
        let source = current.as_deref().unwrap_or(text);
        if let Some((replaced, count)) = apply(pattern, source) {
            current = Some(replaced);
            total += count;
        }
    }
    (current, total)
}

/// Replace every recognized credential; borrowed when nothing matched.
pub fn redact_text(text: &str) -> Cow<'_, str> {
    match redact_with(text, &ENV_SECRETS).0 {
        Some(replaced) => Cow::Owned(replaced),
        None => Cow::Borrowed(text),
    }
}

/// Redacted text and the number of replaced secrets.
pub fn redact_text_counted(text: &str) -> (String, usize) {
    let (replaced, count) = redact_with(text, &ENV_SECRETS);
    (replaced.unwrap_or_else(|| text.to_owned()), count)
}

/// Redact every string in a JSON document in place (keys are structure, not content).
/// Returns how many secrets were replaced.
pub fn redact_value(value: &mut Value) -> usize {
    match value {
        Value::String(text) => {
            let (replaced, count) = redact_with(text, &ENV_SECRETS);
            if let Some(replaced) = replaced {
                *text = replaced;
            }
            count
        }
        Value::Array(items) => items.iter_mut().map(redact_value).sum(),
        Value::Object(map) => map.values_mut().map(redact_value).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn json_values_are_walked_and_counted() {
        let gh = format!("github_pat_{}", "x".repeat(40));
        let mut value = serde_json::json!({"a":[gh.clone(), {"b": format!("say {gh}")}], "n": 3});
        assert_eq!(redact_value(&mut value), 2);
        assert!(!value.to_string().contains(&gh));
        assert_eq!(value["n"], 3);
    }
}
