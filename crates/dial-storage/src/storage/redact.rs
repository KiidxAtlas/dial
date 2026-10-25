//! Credential redaction for transcripts that leave the machine (export, share).
//!
//! The credential shapes and environment-secret collection live in `dial_core::credentials`
//! (shared with provider-bound masking). Only anchored vendor shapes, PEM private keys, bearer
//! tokens, URL userinfo passwords and the literal values of secret-named environment variables
//! are replaced: no generic keyword/entropy rules, so identifiers such as `token_expiry_seconds`
//! survive.
use dial_core::credentials::{Pattern, patterns, process_env_secrets};
use serde_json::Value;
use std::borrow::Cow;

fn marker(name: &str) -> String {
    format!("[REDACTED:{name}]")
}

fn apply(pattern: &Pattern, text: &str) -> Option<(String, usize)> {
    let spans = pattern.spans(text);
    if spans.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for span in &spans {
        out.push_str(&text[last..span.start]);
        out.push_str(&marker(pattern.name));
        last = span.end;
    }
    out.push_str(&text[last..]);
    Some((out, spans.len()))
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
    for pattern in patterns() {
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
    match redact_with(text, process_env_secrets()).0 {
        Some(replaced) => Cow::Owned(replaced),
        None => Cow::Borrowed(text),
    }
}

/// Redacted text and the number of replaced secrets.
pub fn redact_text_counted(text: &str) -> (String, usize) {
    let (replaced, count) = redact_with(text, process_env_secrets());
    (replaced.unwrap_or_else(|| text.to_owned()), count)
}

/// Redact every string in a JSON document in place (keys are structure, not content).
/// Returns how many secrets were replaced.
pub fn redact_value(value: &mut Value) -> usize {
    match value {
        Value::String(text) => {
            let (replaced, count) = redact_with(text, process_env_secrets());
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
mod tests;
