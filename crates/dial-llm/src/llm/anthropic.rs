//! Anthropic native Messages SSE accumulation. Signed thinking blocks survive tool replay.
use super::{Interrupted, Reply, StreamEvent};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const BASE_URL: &str = "https://api.anthropic.com/v1";

/// A structurally unfinished generation, not a completed reply or executable call.
/// Static text only: safe to preserve through provider error redaction.
#[derive(Debug)]
pub(super) struct IncompleteStream;
impl std::fmt::Display for IncompleteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Anthropic stream ended before blocks or stop reason completed")
    }
}
impl std::error::Error for IncompleteStream {}

/// Only events that advance the generated reply renew the idle deadline.
/// Usage updates, keepalives, unknown extensions and empty deltas are not output.
/// Lifecycle events are bounded by the accumulator's duplicate/block checks.
pub(super) fn advances_reply(data: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(data) else {
        // The accumulator still reports malformed input; do not hide it as a stall.
        return false;
    };
    match event["type"].as_str() {
        Some("message_start" | "content_block_start" | "content_block_stop" | "message_stop") => {
            true
        }
        Some("content_block_delta") => {
            let delta = &event["delta"];
            let field = match delta["type"].as_str() {
                Some("text_delta") => "text",
                Some("thinking_delta") => "thinking",
                Some("signature_delta") => "signature",
                Some("input_json_delta") => "partial_json",
                _ => return false,
            };
            delta[field].as_str().is_some_and(|piece| !piece.is_empty())
        }
        _ => false,
    }
}

/// Documented fallback when older Models API responses omit max_input_tokens.
/// Live catalog values and explicit operator limits take precedence.
pub(crate) fn documented_context_window(model: &str) -> Option<u32> {
    // https://platform.claude.com/docs/en/models/overview (verified 2026-10-06).
    for (id, window) in [
        ("claude-opus-5-5", 1_000_000),
        ("claude-opus-5", 1_000_000),
        ("claude-sonnet-5-5", 1_000_000),
        ("claude-sonnet-5", 1_000_000),
        ("claude-fable-5-1", 1_000_000),
        ("claude-haiku-4-5", 200_000),
    ] {
        if model == id
            || model.strip_prefix(id).is_some_and(|suffix| {
                suffix.len() == 9
                    && suffix.starts_with('-')
                    && suffix[1..].bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Some(window);
        }
    }
    None
}

pub(crate) fn subscription_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat01-")
}

fn claude_sdk_headers() -> [(&'static str, String); 8] {
    let arch = match std::env::consts::ARCH {
        "x86_64" | "x64" => "x64".to_owned(),
        "aarch64" | "arm64" => "arm64".to_owned(),
        "x86" | "i686" => "x86".to_owned(),
        other => format!("other::{other}"),
    };
    let os = match std::env::consts::OS {
        "macos" => "MacOS".to_owned(),
        "windows" => "Windows".to_owned(),
        "linux" => "Linux".to_owned(),
        "freebsd" => "FreeBSD".to_owned(),
        other => format!("Other::{other}"),
    };
    [
        ("X-Stainless-Arch", arch),
        ("X-Stainless-Lang", "js".into()),
        ("X-Stainless-OS", os),
        ("X-Stainless-Package-Version", "0.112.1".into()),
        ("X-Stainless-Retry-Count", "0".into()),
        ("X-Stainless-Runtime", "node".into()),
        ("X-Stainless-Runtime-Version", "v26.3.0".into()),
        ("X-Stainless-Timeout", "600".into()),
    ]
}
/// Claude OAuth requests use Anthropic's native CLI authentication headers.
/// Never forward subscription credentials to a proxy or custom endpoint.
pub(crate) fn auth_headers(url: &str, token: &str) -> Result<Vec<(&'static str, String)>> {
    ensure!(
        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
        "invalid Anthropic credential"
    );
    let mut headers = vec![("anthropic-version", "2023-06-01".into())];
    if subscription_token(token) {
        let uri: ureq::http::Uri = url
            .parse()
            .context("invalid Anthropic subscription endpoint")?;
        ensure!(
            uri.scheme_str() == Some("https")
                && uri
                    .authority()
                    .is_some_and(|a| a.as_str() == "api.anthropic.com")
                && matches!(uri.path(), "/v1" | "/v1/models" | "/v1/messages")
                && uri.query().is_none(),
            "Anthropic subscription credentials require the official endpoint"
        );
        headers.push(("Authorization", format!("Bearer {token}")));
        headers.extend(claude_sdk_headers());
        let beta = if uri.path() == "/v1/models" {
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15"
        } else {
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,effort-2025-11-24,fallback-credit-2026-06-01"
        };
        headers.push(("anthropic-beta", beta.into()));
        headers.push(("User-Agent", "claude-cli/2.1.280 (external, cli)".into()));
        headers.push(("anthropic-dangerous-direct-browser-access", "true".into()));
        headers.push(("x-app", "cli".into()));
        headers.push(("Connection", "keep-alive".into()));
    } else {
        headers.push(("x-api-key", token.into()));
    }
    Ok(headers)
}

#[cfg(test)]
mod auth_tests;

#[derive(Default)]
pub(super) struct Accumulator {
    blocks: BTreeMap<u64, Value>,
    arguments: BTreeMap<u64, String>,
    open: std::collections::BTreeSet<u64>,
    usage: Value,
    stop_reason: Option<String>,
    started: bool,
    stopped: bool,
    claude_code_oauth: bool,
}
impl Accumulator {
    pub(super) fn for_claude_code() -> Self {
        Self {
            claude_code_oauth: true,
            ..Default::default()
        }
    }
    pub(super) fn feed(
        &mut self,
        data: &str,
        on_text: &mut dyn FnMut(&str) -> bool,
        on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    ) -> Result<bool> {
        let event: Value = serde_json::from_str(data).context("invalid Anthropic stream JSON")?;
        match event["type"].as_str().unwrap_or_default() {
            "ping" => {}
            "error" => {
                // These are the streamed equivalents of retryable HTTP errors.
                // Never retain upstream messages, which may contain credentials.
                let kind = event.pointer("/error/type").and_then(Value::as_str);
                let transient = match kind {
                    Some("overloaded_error") => {
                        Some("Anthropic stream overloaded before completion")
                    }
                    Some("api_error") => {
                        Some("Anthropic stream failed with an internal server error")
                    }
                    Some("rate_limit_error") => {
                        Some("Anthropic stream rate limited before completion")
                    }
                    _ => None,
                };
                if let Some(message) = transient {
                    return Err(super::stream::IncompleteStream(message).into());
                }
                bail!(
                    "Anthropic reported a stream error ({})",
                    kind.filter(|s| s.len() <= 80
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                        .unwrap_or("unknown")
                );
            }
            "message_start" => {
                ensure!(!self.started, "duplicate Anthropic message_start");
                self.started = true;
                self.usage = event["message"]["usage"].clone();
                super::usage::capture(super::provider::anthropic_usage_snapshot(&self.usage));
            }
            "content_block_start" => {
                ensure!(
                    self.started && !self.stopped,
                    "Anthropic block before message_start"
                );
                let index = event["index"]
                    .as_u64()
                    .context("Anthropic block has no index")?;
                ensure!(
                    self.blocks.len() < 128 && !self.blocks.contains_key(&index),
                    "duplicate or excessive Anthropic blocks"
                );
                let block = event["content_block"].clone();
                ensure!(
                    block.is_object(),
                    "Anthropic content block is not an object"
                );
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    && !on_text(text)
                {
                    return Err(Interrupted.into());
                }
                if let Some(text) = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    && !on_event(&StreamEvent::Reasoning(text.to_owned()))
                {
                    return Err(Interrupted.into());
                }
                self.blocks.insert(index, block);
                self.open.insert(index);
            }
            "content_block_delta" => {
                let index = event["index"]
                    .as_u64()
                    .context("Anthropic delta has no index")?;
                ensure!(
                    self.open.contains(&index),
                    "Anthropic delta for closed block"
                );
                let block = self
                    .blocks
                    .get_mut(&index)
                    .context("Anthropic delta for unknown block")?;
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" | "thinking_delta" | "signature_delta" => {
                        let (field, kind) = match delta["type"].as_str().unwrap() {
                            "text_delta" => ("text", "text"),
                            "thinking_delta" => ("thinking", "thinking"),
                            _ => ("signature", "thinking"),
                        };
                        ensure!(
                            block["type"] == kind,
                            "Anthropic delta type conflicts with block"
                        );
                        let text = delta[field]
                            .as_str()
                            .context("Anthropic text delta is not a string")?;
                        if block[field].is_null() {
                            block[field] = json!("");
                        }
                        match &mut block[field] {
                            Value::String(buffer) => buffer.push_str(text),
                            _ => bail!("invalid Anthropic block text"),
                        }
                        if field == "text" && !text.is_empty() && !on_text(text) {
                            return Err(Interrupted.into());
                        }
                        if field == "thinking"
                            && !text.is_empty()
                            && !on_event(&StreamEvent::Reasoning(text.to_owned()))
                        {
                            return Err(Interrupted.into());
                        }
                    }
                    "input_json_delta" => {
                        ensure!(
                            matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use")),
                            "Anthropic input delta outside tool block"
                        );
                        let piece = delta["partial_json"]
                            .as_str()
                            .context("invalid Anthropic tool JSON delta")?;
                        self.arguments.entry(index).or_default().push_str(piece);
                        let mut name = block["name"].as_str().unwrap_or_default().to_owned();
                        if self.claude_code_oauth
                            && let Some(stripped) = name.strip_prefix('_')
                        {
                            name = stripped.to_owned();
                        }
                        if block["type"] == "tool_use"
                            && !on_event(&StreamEvent::ToolArguments {
                                index,
                                id: block["id"].as_str().unwrap_or_default().to_owned(),
                                name,
                                delta: piece.to_owned(),
                            })
                        {
                            return Err(Interrupted.into());
                        }
                    }
                    // Unknown delta types (e.g. `citations_delta`) add no reply content; skip them.
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event["index"]
                    .as_u64()
                    .context("Anthropic block_stop has no index")?;
                ensure!(
                    self.open.remove(&index),
                    "Anthropic stop for unknown or closed block"
                );
                let block = &self.blocks[&index];
                if block["type"] == "tool_use" {
                    let name = block["name"].as_str().unwrap_or_default();
                    let name = if self.claude_code_oauth {
                        name.strip_prefix('_').unwrap_or(name)
                    } else {
                        name
                    };
                    let arguments = self
                        .arguments
                        .get(&index)
                        .cloned()
                        .unwrap_or_else(|| block["input"].to_string());
                    if !on_event(&StreamEvent::ToolCallComplete {
                        index,
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name: name.to_owned(),
                        arguments,
                    }) {
                        return Err(Interrupted.into());
                    }
                }
                if let Some(raw) = self.arguments.remove(&index) {
                    // No-argument tools stream a single empty `partial_json`; that means `{}`.
                    self.blocks.get_mut(&index).unwrap()["input"] = if raw.trim().is_empty() {
                        json!({})
                    } else {
                        super::json_repair::parse_arguments(&raw).unwrap_or(Value::Null)
                    };
                }
            }
            "message_delta" => {
                ensure!(self.started, "Anthropic message delta before start");
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(fields) = event["usage"].as_object() {
                    if !self.usage.is_object() {
                        self.usage = json!({});
                    }
                    for (key, value) in fields {
                        if key == "cache_creation"
                            && let Some(incoming) = value.as_object()
                            && let Some(existing) = self.usage[key].as_object_mut()
                        {
                            // A delta may repeat only one TTL bucket; keep the other from message_start.
                            existing.extend(incoming.clone());
                        } else if !value.is_null() {
                            self.usage[key] = value.clone();
                        }
                    }
                }
                super::usage::capture(super::provider::anthropic_usage_snapshot(&self.usage));
            }
            "message_stop" => {
                if !(self.started && self.open.is_empty() && self.stop_reason.is_some()) {
                    return Err(IncompleteStream.into());
                }
                self.stopped = true;
                return Ok(true);
            }
            // Unknown event types are future extensions; they must not abort the reply.
            _ => {}
        }
        Ok(false)
    }
    pub(super) fn finish(self) -> Result<Reply> {
        if !self.stopped {
            return Err(IncompleteStream.into());
        }
        super::provider::anthropic_reply(
            &json!({"content":self.blocks.into_values().collect::<Vec<_>>(),"stop_reason":self.stop_reason,"usage":self.usage}),
        )
    }
}

#[cfg(test)]
mod tests;
