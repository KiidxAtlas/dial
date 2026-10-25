//! Provider-bound secret masking. When `[secrets] enabled`, every secret in the conversation sent
//! to a hosted model is replaced by a placeholder, and placeholders in the model's reply are
//! restored before anything else sees it, so tools always run with the real value and the session
//! journal keeps real values.
//!
//! A placeholder is `$$SECRET_<12 hex>$$`: the first 6 bytes of HMAC-SHA256 over the secret, keyed
//! with a random per-install key in the private user directory. The same secret always gets the
//! same placeholder (stable prompt-cache prefixes, stable across restarts) and a placeholder
//! cannot be dictionary-tested without the key.
//!
//! Secrets come from the shared credential shapes (`dial_core::credentials`), the values of
//! secret-named environment variables (`secrets.env`) and `secrets.values`. Masking is applied to
//! each request's message list; the static tool definitions are not masked. The masking of a
//! request yields a [`Restorer`] that knows exactly the secrets that request contained, which are
//! all the placeholders the model can have seen.
use super::{Chat, Reply};
use crate::config::secrets::{Config, MIN_SECRET_BYTES};
use crate::config::settings_store;
use crate::platform::atomic::{read_private, write_private};
use anyhow::{Context, Result, bail};
use dial_core::credentials::{self, patterns};
use rand::RngCore;
use ring::hmac;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
};

const PREFIX: &str = "$$SECRET_";
const SUFFIX: &str = "$$";
const DIGEST_HEX: usize = 12;
const PLACEHOLDER_LEN: usize = PREFIX.len() + DIGEST_HEX + SUFFIX.len();
const KEY_FILE: &str = "secret-mask.key";
const KEY_BYTES: usize = 32;
/// Strings larger than this are never parsed as nested JSON.
const NESTED_JSON_LIMIT: usize = 4 * 1024 * 1024;

pub struct Masker {
    key: hmac::Key,
    /// Literal secrets, longest first.
    literals: Vec<String>,
}

impl Masker {
    /// `None` when masking is off. Fails (rather than sending raw text) if the install key cannot
    /// be read or created.
    pub fn from_config(config: &Config, user_dir: &Path) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let key = load_key(user_dir)?;
        let env = if config.env {
            credentials::process_env_secrets().to_vec()
        } else {
            Vec::new()
        };
        Ok(Some(Self::new(&key, &config.values, env)))
    }

    fn new(key: &[u8; KEY_BYTES], values: &[String], env: Vec<String>) -> Self {
        let mut literals: Vec<String> = values
            .iter()
            .cloned()
            .chain(env)
            .filter(|value| value.len() >= MIN_SECRET_BYTES && !value.trim().is_empty())
            .collect();
        literals.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        literals.dedup();
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, key),
            literals,
        }
    }

    /// The placeholder for `secret`.
    pub fn placeholder(&self, secret: &str) -> String {
        let tag = hmac::sign(&self.key, secret.as_bytes());
        format!(
            "{PREFIX}{}{SUFFIX}",
            dial_core::hex(&tag.as_ref()[..DIGEST_HEX / 2])
        )
    }

    /// A masked copy of `chat` (tool definitions untouched) and the means to undo the masking in
    /// the reply it produces.
    pub fn mask_chat(&self, chat: &Chat) -> (Chat, Restorer) {
        let mut masked = chat.clone();
        let mut found = BTreeMap::new();
        for message in &mut masked.messages {
            self.mask_value(message, &mut found);
        }
        (masked, Restorer { secrets: found })
    }

    /// Mask every string of `value`, recording each placeholder's secret in `found`.
    pub fn mask_value(&self, value: &mut Value, found: &mut BTreeMap<String, String>) -> bool {
        match value {
            Value::String(text) => self.mask_string(text, found),
            Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
                self.mask_value(item, found) | changed
            }),
            Value::Object(map) => map.iter_mut().fold(false, |changed, (key, item)| {
                (!opaque_key(key) && self.mask_value(item, found)) | changed
            }),
            _ => false,
        }
    }

    fn mask_string(&self, text: &mut String, found: &mut BTreeMap<String, String>) -> bool {
        // A secret inside a JSON document may be written with escapes the plain text would not
        // contain, so documents are masked structurally first.
        if let Some(mut document) = json_container(text)
            && self.mask_value(&mut document, found)
        {
            *text = document.to_string();
            return true;
        }
        let spans = self.spans(text);
        if spans.is_empty() {
            return false;
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for span in spans {
            out.push_str(&text[last..span.start]);
            let secret = &text[span.clone()];
            let placeholder = self.placeholder(secret);
            found
                .entry(placeholder.clone())
                .or_insert_with(|| secret.to_owned());
            out.push_str(&placeholder);
            last = span.end;
        }
        out.push_str(&text[last..]);
        *text = out;
        true
    }

    /// Sorted, non-overlapping byte ranges of every secret in `text`; overlapping or touching
    /// matches become one range so no fragment of a secret survives.
    fn spans(&self, text: &str) -> Vec<Range<usize>> {
        let mut spans: Vec<Range<usize>> = Vec::new();
        for literal in &self.literals {
            spans.extend(
                text.match_indices(literal.as_str())
                    .map(|(at, found)| at..at + found.len()),
            );
        }
        for pattern in patterns() {
            spans.extend(pattern.spans(text));
        }
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.start < last.end => last.end = last.end.max(span.end),
                _ => merged.push(span),
            }
        }
        merged
    }
}

/// Undoes the masking of one request.
pub struct Restorer {
    /// Placeholder to secret.
    secrets: BTreeMap<String, String>,
}

impl Restorer {
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// `text` with known placeholders replaced; `None` when it holds none.
    pub fn restore_str(&self, text: &str) -> Option<String> {
        if self.secrets.is_empty() || !text.contains(PREFIX) {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let (mut last, mut from) = (0, 0);
        while let Some(offset) = text[from..].find(PREFIX) {
            let start = from + offset;
            let end = start + PLACEHOLDER_LEN;
            match text.get(start..end).and_then(|c| self.secrets.get(c)) {
                Some(secret) => {
                    out.push_str(&text[last..start]);
                    out.push_str(secret);
                    last = end;
                    from = end;
                }
                None => from = start + PREFIX.len(),
            }
        }
        if last == 0 && out.is_empty() {
            return None;
        }
        out.push_str(&text[last..]);
        Some(out)
    }

    /// Replace known placeholders in `text` in place.
    pub fn restore_text(&self, text: &mut String) {
        if let Some(restored) = self.restore_str(text) {
            *text = restored;
        }
    }

    /// Restore every string of `value` (a nested JSON document is restored structurally, so a
    /// secret holding quotes or backslashes stays valid JSON).
    pub fn restore_value(&self, value: &mut Value) -> bool {
        if self.secrets.is_empty() {
            return false;
        }
        match value {
            Value::String(text) => {
                if !text.contains(PREFIX) {
                    return false;
                }
                if let Some(mut document) = json_container(text)
                    && self.restore_value(&mut document)
                {
                    *text = document.to_string();
                    return true;
                }
                match self.restore_str(text) {
                    Some(restored) => {
                        *text = restored;
                        true
                    }
                    None => false,
                }
            }
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, item| self.restore_value(item) | changed),
            Value::Object(map) => map.iter_mut().fold(false, |changed, (key, item)| {
                (!opaque_key(key) && self.restore_value(item)) | changed
            }),
            _ => false,
        }
    }

    /// Restore text, reasoning, tool-call arguments and native replay content of a reply.
    pub fn restore_reply(&self, reply: &mut Reply) {
        if self.secrets.is_empty() {
            return;
        }
        for text in [&mut reply.text, &mut reply.reasoning] {
            if let Some(restored) = self.restore_str(text) {
                *text = restored;
            }
        }
        for call in &mut reply.tool_calls {
            self.restore_value(&mut call.arguments);
            if let Some(restored) = self.restore_str(&call.name) {
                call.name = restored;
            }
        }
        for item in &mut reply.provider_content {
            self.restore_value(item);
        }
    }

    /// Run `request` with streamed text restored on its way to `on_text`, then restore the reply.
    /// Text that might be the start of a placeholder is held back until it resolves.
    pub fn run(
        &self,
        on_text: Option<&mut dyn FnMut(&str) -> bool>,
        request: impl FnOnce(Option<&mut dyn FnMut(&str) -> bool>) -> Result<Reply>,
    ) -> Result<Reply> {
        let mut result = match on_text {
            Some(callback) if !self.is_empty() => {
                let mut pending = String::new();
                let mut wrapped = |chunk: &str| {
                    pending.push_str(chunk);
                    let hold = held_suffix(&pending);
                    let ready: String = pending.drain(..pending.len() - hold).collect();
                    let ready = self.restore_str(&ready).unwrap_or(ready);
                    ready.is_empty() || callback(&ready)
                };
                let result = request(Some(&mut wrapped));
                if !pending.is_empty() {
                    let tail = self.restore_str(&pending).unwrap_or(pending);
                    callback(&tail);
                }
                result
            }
            other => request(other),
        };
        if let Ok(reply) = &mut result {
            self.restore_reply(reply);
        }
        result
    }
}

/// Keys whose strings are opaque provider state or harness bookkeeping, never conversation text.
fn opaque_key(key: &str) -> bool {
    matches!(
        key,
        "signature" | "thoughtSignature" | "thought_signature" | "encrypted_content" | "scope"
    ) || (key.starts_with("dial_") && key != "dial_provider_content")
}

fn json_container(text: &str) -> Option<Value> {
    let trimmed = text.trim_start();
    if text.len() > NESTED_JSON_LIMIT || !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| value.is_object() || value.is_array())
}

/// Bytes at the end of `text` that could still become a placeholder.
fn held_suffix(text: &str) -> usize {
    let length = text.len();
    text.char_indices()
        .rev()
        .take_while(|(at, _)| length - at <= PLACEHOLDER_LEN)
        .filter(|(at, c)| *c == '$' && could_become_placeholder(&text[*at..]))
        .last()
        .map_or(0, |(at, _)| length - at)
}

fn could_become_placeholder(partial: &str) -> bool {
    if PREFIX.starts_with(partial) {
        return true;
    }
    let Some(rest) = partial.strip_prefix(PREFIX) else {
        return false;
    };
    let digits = rest
        .bytes()
        .take_while(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        .count();
    let tail = &rest[digits..];
    digits <= DIGEST_HEX && (tail.is_empty() || (digits == DIGEST_HEX && SUFFIX.starts_with(tail)))
}

fn key_path(user_dir: &Path) -> PathBuf {
    user_dir.join(KEY_FILE)
}

/// The install key, created owner-only on first use. Concurrent processes agree on one key.
fn load_key(user_dir: &Path) -> Result<[u8; KEY_BYTES]> {
    let path = key_path(user_dir);
    if let Some(key) = read_key(&path)? {
        return Ok(key);
    }
    let _lock = settings_store::locked(&path)?;
    if let Some(key) = read_key(&path)? {
        return Ok(key);
    }
    let mut key = [0u8; KEY_BYTES];
    rand::rng().fill_bytes(&mut key);
    write_private(&path, dial_core::hex(&key).as_bytes())
        .context("saving the secret-masking key")?;
    Ok(key)
}

fn read_key(path: &Path) -> Result<Option<[u8; KEY_BYTES]>> {
    let Some(bytes) = read_private(path)? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes).unwrap_or_default().trim();
    if text.len() != KEY_BYTES * 2 || !text.is_ascii() {
        bail!(
            "{} is not a valid secret-masking key; delete it to create a new one",
            path.display()
        );
    }
    let mut key = [0u8; KEY_BYTES];
    for (byte, pair) in key.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = std::str::from_utf8(pair)
            .ok()
            .and_then(|digits| u8::from_str_radix(digits, 16).ok())
            .with_context(|| format!("{} is not a valid secret-masking key", path.display()))?;
    }
    Ok(Some(key))
}

#[cfg(test)]
mod tests;
