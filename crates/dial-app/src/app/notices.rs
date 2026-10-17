//! Memory and session notices shown in the transcript, and the retry notice bookkeeping.

use crate::llm::catalog;
use serde_json::Value;

/// Where each provider's credential comes from, keyed by base URL: a saved key and/or a named
/// environment variable. Covers discovered built-in endpoints and configured ones; configured wins.
pub(super) fn credential_sources(
    user_dir: &std::path::Path,
    config: &crate::config::types::Config,
    discovered: &[catalog::ProviderStatus],
) -> std::collections::BTreeMap<String, crate::ui::overlay::providers::CredentialSource> {
    let built_in = discovered
        .iter()
        .filter_map(|p| p.base_url.clone())
        .map(|url| (url, None));
    let configured = config
        .providers
        .values()
        .filter(|spec| !spec.base_url.trim().is_empty())
        .map(|spec| (spec.base_url.clone(), spec.api_key_env.clone()));
    let mut sources = std::collections::BTreeMap::new();
    for (url, env_name) in built_in.chain(configured) {
        let saved = catalog::credential_token(user_dir, &url)
            .ok()
            .flatten()
            .is_some();
        let env_name = env_name.or_else(|| catalog::known_key_env(&url).map(str::to_owned));
        let env = env_name.map(|name| crate::ui::overlay::providers::EnvCredential {
            set: std::env::var(&name).is_ok_and(|value| !value.is_empty()),
            name,
        });
        sources.insert(
            url,
            crate::ui::overlay::providers::CredentialSource { saved, env },
        );
    }
    sources
}

/// The `/memory` status in plain words: what Dial remembers (newest first), then the backend, whether
/// it is working, why not, notes from the backend, and what to do next.
pub(super) fn memory_status_text(value: &Value) -> String {
    let backend = value.get("backend").and_then(Value::as_str).unwrap_or("");
    let active = value.get("active").and_then(Value::as_bool).unwrap_or(true);
    let (label, noun) = match backend {
        "off" => ("Off", ""),
        "provenance" => ("Remembered facts", "fact"),
        "local" => ("Rollout summaries (local)", "lesson"),
        "hindsight" => ("Hindsight service", ""),
        "sharpshooter" => ("Project decisions", ""),
        "semantic" => ("Semantic facts", "fact"),
        _ => ("unknown", ""),
    };
    let mut lines = Vec::new();
    if active && !noun.is_empty() {
        lines.extend(remembered_lines(value, noun));
        if let Some(user) = value.get("userFacts").and_then(Value::as_u64) {
            lines.push(format!("User-wide facts (all projects): {user}"));
        }
        lines.push(String::new());
    }
    lines.push(format!("Backend: {label}"));
    lines.push(format!("Active: {}", if active { "yes" } else { "no" }));
    let error = value.get("error").and_then(Value::as_str);
    // An inactive backend's generic message repeats the reason below with a fixed next step.
    if (noun.is_empty() || backend == "off")
        && (active || error.is_none())
        && let Some(message) = value.get("message").and_then(Value::as_str)
    {
        lines.push(message.to_owned());
    }
    if let Some(embedding) = value.get("embedding") {
        let state = embedding.get("state").and_then(Value::as_str).unwrap_or("");
        let line = match (state, embedding.get("model").and_then(Value::as_str)) {
            ("on", Some(model)) => match embedding.get("embedded").and_then(Value::as_u64) {
                Some(embedded) => format!("Embeddings: on ({model}, {embedded} facts embedded)"),
                None => format!("Embeddings: on ({model})"),
            },
            _ => {
                let reason = embedding
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("not configured");
                format!("Embeddings: off — {reason}")
            }
        };
        lines.push(line);
        if state != "on" && !reason_names_next_step(embedding) {
            lines.push(
                "  Recall by meaning: assign the embedding role (Alt+M) or run /setup embeddings."
                    .to_owned(),
            );
        }
    }
    if let Some(probe) = value.get("reachability").and_then(Value::as_str) {
        lines.push(format!("Service probe: {probe}"));
    }
    if let Some(error) = error {
        lines.push(format!("Why: {error}"));
    }
    for note in value
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        lines.push(format!("Note: {note}"));
    }
    if backend == "off" {
        lines.push("Next: choose a backend in /settings → Memory or with /setup memory".to_owned());
    } else if !active {
        lines.push(format!("Next: {}", memory_next_step(error.unwrap_or(""))));
    } else if !noun.is_empty() {
        let clear = if backend == "semantic" {
            "/memory clear forgets project facts (/memory clear user: user-wide)"
        } else {
            "/memory clear forgets project facts"
        };
        lines.push(format!("Next: {clear} · /memory help · /settings → Memory"));
    }
    lines.join("\n")
}

/// Whether a backend's embedding reason already says what to do (it mentions a role or a command).
fn reason_names_next_step(embedding: &Value) -> bool {
    embedding
        .get("reason")
        .and_then(Value::as_str)
        .is_some_and(|reason| reason.contains("Alt+M") || reason.contains("/setup"))
}

/// Where to fix an inactive memory backend: the Roles view when a model role is missing, settings
/// otherwise.
fn memory_next_step(reason: &str) -> &'static str {
    if reason.contains("/roles") || reason.contains(" role") {
        "assign the role in /roles (Alt+M)"
    } else {
        "/settings → Memory, or choose another backend with /setup memory"
    }
}

/// The "memory is paused" notice, due after a startup or rebuild only when the backend or its open
/// error differs from the last one reported (`last` remembers it).
pub(super) fn memory_notice_due(
    last: &mut Option<(String, Option<String>)>,
    backend: &str,
    error: Option<String>,
) -> Option<String> {
    let state = (backend.to_owned(), error);
    if last.as_ref() == Some(&state) {
        return None;
    }
    let notice = state.1.as_deref().map(|reason| {
        if reason.contains("/roles") || reason.contains("/settings") {
            format!("Memory is paused: {reason}.")
        } else {
            format!(
                "Memory is paused: {reason}. Next: {}.",
                memory_next_step(reason)
            )
        }
    });
    *last = Some(state);
    notice
}

/// The remembered items for a backend that stores facts or lessons: a count heading, then one row per
/// item (newest first), or an empty-state line that says how items get recorded.
fn remembered_lines(value: &Value, noun: &str) -> Vec<String> {
    let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
    let truncated = value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if count == 0 {
        let how = if noun == "lesson" {
            "Dial learns from past sessions for this project."
        } else {
            "Dial records facts as sessions run."
        };
        return vec![format!("No remembered {noun}s yet."), how.to_owned()];
    }
    let shown = if truncated {
        format!("{count}+")
    } else {
        count.to_string()
    };
    let plural = if count == 1 && !truncated { "" } else { "s" };
    if value.get("recent").is_none() {
        // Backends that report only a count (semantic) list nothing, so make no "newest first" claim.
        return vec![format!(
            "{shown} remembered {noun}{plural} in this project."
        )];
    }
    let mut lines = vec![format!("{shown} remembered {noun}{plural}, newest first:")];
    for item in value
        .get("recent")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = item.get("content").and_then(Value::as_str).unwrap_or("");
        let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
        let content: String = if content.chars().count() > 110 {
            format!("{}…", content.chars().take(109).collect::<String>())
        } else {
            content
        };
        match item.get("updated_at").and_then(Value::as_i64) {
            Some(then) => lines.push(format!("  {}  {content}", memory_age(then))),
            None => lines.push(format!("  {content}")),
        }
    }
    lines
}

/// Relative age of a unix timestamp in seconds: `just now`, `5m ago`, `2h ago`, `3d ago`.
pub(super) fn memory_age(then: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(then, |d| d.as_secs() as i64);
    match (now - then).max(0) {
        0..=59 => "just now".into(),
        secs @ 60..=3599 => format!("{}m ago", secs / 60),
        secs @ 3600..=86399 => format!("{}h ago", secs / 3600),
        secs => format!("{}d ago", secs / 86400),
    }
}

/// A provider retry notice: `"<reason>: <cause>; retrying in 3.4s (attempt 4/10)"`.
pub(super) struct RetryNotice {
    pub(super) cause: String,
    pub(super) max: String,
}

impl RetryNotice {
    pub(super) fn parse(message: &str) -> Option<Self> {
        let (head, tail) = message.rsplit_once("; retrying in ")?;
        let attempts = tail.rsplit_once("(attempt ")?.1.strip_suffix(')')?;
        let (_, max) = attempts.split_once('/')?;
        let cause = head
            .split_once(": ")
            .map_or(head, |(_, cause)| cause)
            .trim();
        Some(Self {
            cause: cause.to_owned(),
            max: max.trim().to_owned(),
        })
    }
}
