//! Session anonymizer: rewrites a session JSONL file (and optional LLM request
//! JSON) so it can be shared.
//!
//! Design:
//! - Structural fields (ids, parents, timestamps, record types, roles) are kept
//!   when they match a strict shape; anything else is replaced by a placeholder.
//! - Free text and unknown fields are replaced wholesale (default deny) by
//!   `<text:n>` placeholders. Identical values map to identical placeholders
//!   within one `Anonymizer`, so correlation survives.
//! - Object keys outside the session schema are data: they become correlated `<key:n>` placeholders.
//! - Paths map each segment, keep extensions and common structural segments,
//!   and rewrite the home prefix to `~`.
//! - Credentials are redacted first via `redact_text_counted`.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::storage::redact::redact_text_counted;

/// Shown after every anonymized export.
pub const REVIEW_NOTE: &str = "Turn contents and error text are redacted and paths/literals replaced; metadata such as model names is kept — review before sharing.";

/// Path segments that carry no identity and are kept verbatim.
const KEEP_SEGMENTS: &[&str] = &[
    "Users",
    "home",
    "root",
    "tmp",
    "var",
    "etc",
    "usr",
    "opt",
    "Library",
    "Documents",
    "Desktop",
    "dev",
    "projects",
    "src",
    "lib",
    "bin",
    "tests",
    "test",
    "target",
    "node_modules",
    ".config",
    ".cache",
    ".local",
    ".dial",
];

/// Field names the session and request schemas define. Only these keys keep their text; any other
/// object key is data.
const SCHEMA_KEYS: &[&str] = &[
    "type",
    "id",
    "parent",
    "ts",
    "started",
    "version",
    "model",
    "forked_from",
    "cwd",
    "message",
    "title",
    "summary",
    "first_kept",
    "custom_type",
    "role",
    "content",
    "tool_calls",
    "tool_call_id",
    "function",
    "name",
    "arguments",
    "dial_context_request",
    "dial_context_source_entry_id",
    "messages",
    "tools",
    "stream",
    "max_tokens",
    "temperature",
    "tool_choice",
];

/// Host identity used to scrub usernames and hostnames from paths and text.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub home: String,
    pub user: String,
    pub host: String,
}

impl Identity {
    /// Reads `HOME`, `USER`/`USERNAME`, and `HOSTNAME`, falling back to the
    /// `hostname` command for the host.
    pub fn detect() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default();
        let host = std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            })
            .unwrap_or_default();
        Self { home, user, host }
    }
}

/// Result of anonymizing one session JSONL file.
#[derive(Debug, Clone)]
pub struct AnonymizedSession {
    pub bytes: Vec<u8>,
    /// Lines that were not JSON objects and were dropped.
    pub malformed: usize,
    /// Credential matches replaced while anonymizing this session.
    pub redacted: usize,
}

/// Shared anonymizer. Reuse one instance across the files of one export so
/// placeholders correlate.
#[derive(Debug)]
pub struct Anonymizer {
    identity: Identity,
    tokens: HashMap<(&'static str, String), usize>,
    redacted: usize,
}

#[derive(Clone, Copy)]
enum Rule {
    Id,
    Time,
    Label,
    Path,
    Opaque,
}

impl Anonymizer {
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            tokens: HashMap::new(),
            redacted: 0,
        }
    }

    /// Anonymizes a session JSONL file's bytes. The first non-empty line must be
    /// the `session` header. Empty lines are skipped; non-object lines are counted
    /// as malformed and dropped. Every object record is kept, including unknown types.
    pub fn session_jsonl(&mut self, bytes: &[u8]) -> Result<AnonymizedSession> {
        let text = std::str::from_utf8(bytes).context("session file is not valid UTF-8")?;
        let redacted_before = self.redacted;
        let mut out = String::new();
        let mut malformed = 0usize;
        let mut header_seen = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    malformed += 1;
                    continue;
                }
            };
            let Value::Object(map) = value else {
                malformed += 1;
                continue;
            };
            if !header_seen {
                if map.get("type").and_then(Value::as_str) != Some("session") {
                    bail!("is not a valid session file: first record is not a session header");
                }
                header_seen = true;
            }
            let record = self.record(&map);
            out.push_str(&serde_json::to_string(&record)?);
            out.push('\n');
        }
        if !header_seen {
            bail!("is not a valid session file: no session header found");
        }
        Ok(AnonymizedSession {
            bytes: out.into_bytes(),
            malformed,
            redacted: self.redacted - redacted_before,
        })
    }

    /// Anonymizes an arbitrary JSON value (e.g. the LLM request message). An object is read with the
    /// message envelope rules, so its role and tool-call ids survive; everything else is default-deny.
    pub fn value(&mut self, value: &Value) -> Value {
        match value {
            Value::Object(_) => self.message(value),
            _ => self.opaque(value),
        }
    }

    fn record(&mut self, map: &Map<String, Value>) -> Value {
        let ty = map.get("type").and_then(Value::as_str).unwrap_or("");
        let rules: &[(&str, Rule)] = match ty {
            "session" => &[
                ("type", Rule::Label),
                ("id", Rule::Id),
                ("started", Rule::Time),
                ("version", Rule::Label),
                ("model", Rule::Label),
                ("forked_from", Rule::Id),
                ("cwd", Rule::Path),
            ],
            "runtime" | "message" | "meta" | "compaction" | "custom" | "reset_boundary" => &[
                ("type", Rule::Label),
                ("id", Rule::Id),
                ("parent", Rule::Id),
                ("ts", Rule::Time),
            ],
            // Unknown record types keep only the envelope fields; every other key is data.
            _ => &[
                ("type", Rule::Label),
                ("id", Rule::Id),
                ("parent", Rule::Id),
                ("ts", Rule::Time),
            ],
        };
        let mut out = Map::new();
        for (key, value) in map {
            let rule = rules
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, r)| *r)
                .unwrap_or(Rule::Opaque);
            let mapped = match (ty, key.as_str()) {
                ("message", "message") => self.message(value),
                ("meta", "title") | ("compaction", "summary") => self.text_value(value),
                ("compaction", "first_kept") => self.apply(Rule::Id, value),
                ("custom", "custom_type") => self.apply(Rule::Label, value),
                _ => self.apply(rule, value),
            };
            out.insert(self.key(key), mapped);
        }
        Value::Object(out)
    }

    fn message(&mut self, value: &Value) -> Value {
        let Value::Object(map) = value else {
            return self.opaque(value);
        };
        let mut out = Map::new();
        for (key, value) in map {
            let mapped = match key.as_str() {
                "role" => self.apply(Rule::Label, value),
                "tool_call_id" => self.apply(Rule::Id, value),
                "dial_context_source_entry_id" => self.apply(Rule::Id, value),
                "content" if value.is_string() => self.text_value(value),
                "tool_calls" => self.tool_calls(value),
                _ => self.opaque(value),
            };
            out.insert(self.key(key), mapped);
        }
        Value::Object(out)
    }

    fn tool_calls(&mut self, value: &Value) -> Value {
        let Value::Array(items) = value else {
            return self.opaque(value);
        };
        let mapped = items
            .iter()
            .map(|item| {
                let Value::Object(map) = item else {
                    return self.opaque(item);
                };
                let mut out = Map::new();
                for (key, value) in map {
                    let mapped = match key.as_str() {
                        "id" => self.apply(Rule::Id, value),
                        "type" => self.apply(Rule::Label, value),
                        "function" => self.function(value),
                        _ => self.opaque(value),
                    };
                    out.insert(self.key(key), mapped);
                }
                Value::Object(out)
            })
            .collect();
        Value::Array(mapped)
    }

    fn function(&mut self, value: &Value) -> Value {
        let Value::Object(map) = value else {
            return self.opaque(value);
        };
        let mut out = Map::new();
        for (key, value) in map {
            let mapped = match key.as_str() {
                "name" => self.apply(Rule::Label, value),
                "arguments" => self.arguments(value),
                _ => self.opaque(value),
            };
            out.insert(self.key(key), mapped);
        }
        Value::Object(out)
    }

    /// Tool-call arguments are a JSON string. Parse, map by key, and re-serialize.
    fn arguments(&mut self, value: &Value) -> Value {
        let Value::String(raw) = value else {
            return self.opaque(value);
        };
        match serde_json::from_str::<Value>(raw) {
            Ok(parsed) => {
                let mapped = self.tool_args(&parsed);
                Value::String(serde_json::to_string(&mapped).unwrap_or_default())
            }
            Err(_) => self.text_value(value),
        }
    }

    fn tool_args(&mut self, value: &Value) -> Value {
        let Value::Object(map) = value else {
            return self.opaque(value);
        };
        let mut out = Map::new();
        for (key, value) in map {
            let mapped = match (key.as_str(), value) {
                ("command" | "cmd", Value::String(s)) => Value::String(self.command(s)),
                (k, Value::String(_)) if is_path_argument(k) => self.apply(Rule::Path, value),
                _ => self.opaque(value),
            };
            let name = if is_argument_key(key) {
                self.clean_key(key)
            } else {
                self.key(key)
            };
            out.insert(name, mapped);
        }
        Value::Object(out)
    }

    /// Shell command: redact credentials first, then map each word. Only the program name (first
    /// word) and genuine flags keep their text; `--flag=value` keeps the flag and maps the value.
    /// Every other word is a value and becomes a placeholder. Whitespace collapses to single spaces.
    fn command(&mut self, command: &str) -> String {
        let (redacted, n) = redact_text_counted(command);
        self.redacted += n;
        redacted
            .split_whitespace()
            .enumerate()
            .map(|(index, word)| self.command_word(index == 0, word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn command_word(&mut self, program_position: bool, word: &str) -> String {
        if is_flag(word) {
            return word.to_string();
        }
        if let Some((flag, value)) = word.split_once('=')
            && is_flag(flag)
        {
            return format!("{flag}={}", self.value_word(value));
        }
        if program_position && is_label(word) {
            return word.to_string();
        }
        self.value_word(word)
    }

    /// A value word: paths keep their structure, anything else becomes a text placeholder.
    fn value_word(&mut self, word: &str) -> String {
        if word.contains('/') || word.starts_with('~') {
            self.path(word)
        } else {
            self.text_word(word)
        }
    }

    fn apply(&mut self, rule: Rule, value: &Value) -> Value {
        match (rule, value) {
            (Rule::Opaque, _) => self.opaque(value),
            (_, Value::Null) => Value::Null,
            (Rule::Id, Value::String(s)) => {
                let (clean, n) = redact_text_counted(s);
                self.redacted += n;
                if n == 0 && is_ident(s) {
                    Value::String(s.clone())
                } else {
                    Value::String(self.token("id", &clean))
                }
            }
            (Rule::Time, Value::String(s)) => {
                if is_iso_time(s) {
                    Value::String(s.clone())
                } else {
                    Value::String(self.token("time", s))
                }
            }
            (Rule::Label, Value::String(s)) => {
                let (clean, n) = redact_text_counted(s);
                self.redacted += n;
                if n == 0 && is_label(s) {
                    Value::String(s.clone())
                } else {
                    Value::String(self.token("label", &clean))
                }
            }
            (Rule::Path, Value::String(s)) => Value::String(self.path(s)),
            _ => self.opaque(value),
        }
    }

    /// Free text: redact credentials, then replace with a placeholder keyed by the redacted text.
    fn text_value(&mut self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.text(s)),
            _ => self.opaque(value),
        }
    }

    fn text(&mut self, s: &str) -> String {
        if s.is_empty() {
            return String::new();
        }
        let (clean, n) = redact_text_counted(s);
        self.redacted += n;
        self.token("text", &clean)
    }

    fn text_word(&mut self, word: &str) -> String {
        self.token("text", word)
    }

    /// Default-deny: every string is replaced and every key outside the schema becomes a placeholder.
    /// Numbers, bools and nulls are kept. Only the explicit envelope rules keep structural strings,
    /// so a nested `name` or `type` value is data like any other.
    fn opaque(&mut self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.opaque(v)).collect()),
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, value) in map {
                    let mapped = self.opaque(value);
                    out.insert(self.key(key), mapped);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    /// Object keys: schema field names stay (credential-redacted). Any other key is data and becomes
    /// a correlated `<key:n>` placeholder, so path-, email- or name-keyed maps leak nothing.
    fn key(&mut self, key: &str) -> String {
        if SCHEMA_KEYS.contains(&key) {
            return self.clean_key(key);
        }
        let clean = self.clean_key(key);
        self.token("key", &clean)
    }

    /// Credential redaction for a key, keeping its shape.
    fn clean_key(&mut self, key: &str) -> String {
        let (clean, n) = redact_text_counted(key);
        self.redacted += n;
        clean
    }

    /// Path: home prefix becomes `~`; each segment is kept, mapped to a
    /// placeholder, or replaced (usernames and hostnames). Extensions survive.
    fn path(&mut self, path: &str) -> String {
        let (clean, n) = redact_text_counted(path);
        self.redacted += n;
        let mut working = clean.as_str();
        let home = self.identity.home.clone();
        let mut prefix = String::new();
        if !home.is_empty() {
            if working == home {
                return "~".to_string();
            }
            if let Some(rest) = working.strip_prefix(&format!("{home}/")) {
                prefix.push_str("~/");
                working = rest;
            }
        }
        let user = self.identity.user.clone();
        let host = self.identity.host.clone();
        let mapped: Vec<String> = working
            .split('/')
            .map(|seg| {
                if seg.is_empty() || seg == "~" || KEEP_SEGMENTS.contains(&seg) {
                    seg.to_string()
                } else if !user.is_empty() && seg == user {
                    self.token("user", seg)
                } else if !host.is_empty() && seg == host {
                    self.token("host", seg)
                } else if let Some((stem, ext)) = split_extension(seg) {
                    format!("{}.{ext}", self.token("path", stem))
                } else {
                    self.token("path", seg)
                }
            })
            .collect();
        format!("{prefix}{}", mapped.join("/"))
    }

    fn token(&mut self, kind: &'static str, value: &str) -> String {
        if value.is_empty() {
            return String::new();
        }
        let next = self.tokens.len() + 1;
        let n = *self.tokens.entry((kind, value.to_string())).or_insert(next);
        format!("<{kind}:{n}>")
    }
}

/// Keeps the source name only when it has the timestamped `<ts>_<id>.jsonl` shape.
pub fn safe_session_name(src: &Path) -> String {
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let ok = name.ends_with(".jsonl")
        && name
            .strip_suffix(".jsonl")
            .and_then(|stem| stem.rsplit_once('_'))
            .is_some_and(|(ts, id)| {
                !ts.is_empty()
                    && ts
                        .chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, 'T' | 'Z' | '.' | ':' | '-'))
                    && (1..=64).contains(&id.len())
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            });
    if ok {
        name.to_string()
    } else {
        "session.jsonl".to_string()
    }
}

fn is_ident(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_label(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && (1..=64).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_iso_time(s: &str) -> bool {
    s.len() <= 40
        && s.starts_with(|c: char| c.is_ascii_digit())
        && s.contains('T')
        && s.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | 'T' | 'Z' | '+'))
}

/// A genuine flag: one or two leading dashes, then a short alphanumeric name. Bare words are never
/// flags, so a value that merely looks like a word is replaced.
fn is_flag(word: &str) -> bool {
    let body = word.trim_start_matches('-');
    (1..=2).contains(&(word.len() - body.len()))
        && word.len() <= 40
        && body.starts_with(|c: char| c.is_ascii_alphabetic())
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Tool-argument keys whose string values are paths.
fn is_path_argument(key: &str) -> bool {
    key.contains("path") || key == "file"
}

/// Tool-argument keys that keep their structure: the shell command and path arguments.
fn is_argument_key(key: &str) -> bool {
    matches!(key, "command" | "cmd") || is_path_argument(key)
}

/// `name.ext` → (`name`, `ext`) when the extension is short and alphanumeric.
fn split_extension(seg: &str) -> Option<(&str, &str)> {
    let (stem, ext) = seg.rsplit_once('.')?;
    (!stem.is_empty()
        && (1..=8).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric()))
    .then_some((stem, ext))
}

#[cfg(test)]
mod tests;
