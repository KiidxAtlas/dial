//! Server-sent-event reading and chat-completion chunk accumulation.
//!
//! [`read_events`] turns a byte stream into `data:` payloads under hard bounds
//! (total bytes, bytes per line, silence). [`Accumulator`] folds
//! `chat.completion.chunk` payloads into a [`Reply`].

use super::cancel::Patience;
use super::errors::{self, Classified};
use super::{Interrupted, Reply, excerpt, parse_tool_calls};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

/// Longest accepted SSE line. One chunk carries at most one tool call's
/// arguments (`mlx_lm.server` sends each call whole), so this bounds those.
pub(super) const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Most distinct tool calls accepted in one reply.
const MAX_TOOL_CALLS: usize = 128;
/// Drain optional usage/[DONE] briefly without letting a completed reply stall.
const POST_FINISH_GRACE: Duration = Duration::from_millis(2500);

/// A connection closed, stalled, or failed transiently before a complete reply.
/// Partial calls stay inside the failed accumulator and can never be executed.
#[derive(Debug)]
pub(super) struct IncompleteStream(pub &'static str);

impl std::fmt::Display for IncompleteStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for IncompleteStream {}

/// Providers choose whether an idle timeout is safe to retry.
#[derive(Debug)]
pub(super) struct StalledStream;

impl std::fmt::Display for StalledStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "chat stream was silent until the idle limit; the server may be stuck or overloaded",
        )
    }
}

impl std::error::Error for StalledStream {}

/// Read SSE from `reader`, handing every `data:` payload to `on_data`, until it
/// returns `Ok(true)` (stream finished) or the reader reaches EOF. Comments,
/// keep-alives and other fields (`event:`, `id:`, `retry:`) are skipped. Multiple
/// `data:` lines in one event are joined with newlines, per the SSE protocol.
///
/// Time is bounded by silence, never by the length of the reply: the clock restarts on every `data:` event and
/// only on those, so keep-alive comments or a trickle of bytes that never completes an event cannot hold the
/// connection open, while a long answer that keeps producing events can take as long as it needs.
#[cfg(test)]
pub(super) fn read_events(
    reader: impl Read,
    total_limit: usize,
    line_limit: usize,
    patience: Patience,
    on_data: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    read_events_with_progress(reader, total_limit, line_limit, patience, |_| true, on_data)
}

/// Like read_events, but protocol heartbeats need not reset the idle clock.
pub(super) fn read_events_with_progress(
    reader: impl Read,
    total_limit: usize,
    line_limit: usize,
    patience: Patience,
    mut is_progress: impl FnMut(&str) -> bool,
    mut on_data: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    read_events_impl(
        reader,
        total_limit,
        line_limit,
        patience,
        false,
        |data, _| is_progress(data),
        |data, _| on_data(data),
    )
}

/// Preserve SSE event names for protocols whose JSON payload may omit its type discriminator.
pub(super) fn read_named_events_with_progress(
    reader: impl Read,
    total_limit: usize,
    line_limit: usize,
    patience: Patience,
    is_progress: impl FnMut(&str, Option<&str>) -> bool,
    on_data: impl FnMut(&str, Option<&str>) -> Result<bool>,
) -> Result<()> {
    read_events_impl(
        reader,
        total_limit,
        line_limit,
        patience,
        true,
        is_progress,
        on_data,
    )
}

fn read_events_impl(
    mut reader: impl Read,
    total_limit: usize,
    line_limit: usize,
    patience: Patience,
    capture_names: bool,
    mut is_progress: impl FnMut(&str, Option<&str>) -> bool,
    mut on_data: impl FnMut(&str, Option<&str>) -> Result<bool>,
) -> Result<()> {
    let mut chunk = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut pending = String::new();
    let mut event_name = String::new();
    let mut total = 0usize;
    let mut on_data = |data: &str, name: Option<&str>| {
        if is_progress(data, name) {
            patience.mark_event();
        }
        on_data(data, name)
    };
    let stalled = || {
        patience
            .remaining()
            .is_some_and(|remaining| remaining.is_zero())
    };
    let stall_error = || StalledStream;
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                if stalled()
                    || error.kind() == ErrorKind::TimedOut
                    || error.to_string().to_ascii_lowercase().contains("timeout")
                {
                    return Err(stall_error().into());
                }
                if matches!(
                    error.kind(),
                    ErrorKind::UnexpectedEof
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) {
                    return Err(IncompleteStream(
                        "reading chat stream: connection disconnected before the model finished",
                    )
                    .into());
                }
                return Err(error).context("reading chat stream");
            }
        };
        total += count;
        ensure!(
            total <= total_limit,
            "chat stream exceeds {total_limit} bytes"
        );
        for &byte in &chunk[..count] {
            if byte != b'\n' {
                line.push(byte);
                ensure!(
                    line.len() <= line_limit,
                    "chat stream line exceeds {line_limit} bytes"
                );
            } else {
                let done = dispatch_named(
                    &line,
                    &mut pending,
                    &mut event_name,
                    capture_names,
                    &mut on_data,
                )?;
                line.clear();
                if done {
                    return Ok(());
                }
            }
        }
        // Checked after the bytes were processed so a final chunk that
        // finishes the stream is never discarded for arriving late.
        if stalled() {
            return Err(stall_error().into());
        }
    }
    if !line.is_empty() {
        dispatch_named(
            &line,
            &mut pending,
            &mut event_name,
            capture_names,
            &mut on_data,
        )?;
    }
    if !pending.is_empty() {
        on_data(
            &pending,
            (!event_name.is_empty()).then_some(event_name.as_str()),
        )?;
    }
    Ok(())
}

fn dispatch_named(
    line: &[u8],
    pending: &mut String,
    name: &mut String,
    capture_names: bool,
    on_data: &mut impl FnMut(&str, Option<&str>) -> Result<bool>,
) -> Result<bool> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if let Some(value) = line.strip_prefix(b"event:") {
        if capture_names {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            name.clear();
            name.push_str(
                std::str::from_utf8(value).context("chat stream event name is not valid UTF-8")?,
            );
        }
        return Ok(false);
    }
    let done = dispatch(line, pending, &mut |data| {
        on_data(data, (!name.is_empty()).then_some(name.as_str()))
    })?;
    if line.is_empty() {
        name.clear();
    }
    Ok(done)
}

fn dispatch(
    line: &[u8],
    pending: &mut String,
    on_data: &mut impl FnMut(&str) -> Result<bool>,
) -> Result<bool> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        if pending.is_empty() {
            return Ok(false);
        }
        let done = on_data(pending)?;
        pending.clear();
        return Ok(done);
    }
    let Some(rest) = line.strip_prefix(b"data:") else {
        return Ok(false);
    };
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    let payload = std::str::from_utf8(rest).context("chat stream event is not valid UTF-8")?;
    if !pending.is_empty() {
        pending.push('\n');
    }
    pending.push_str(payload);
    Ok(false)
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(super) struct Accumulator {
    text: String,
    calls: BTreeMap<u64, PartialCall>,
    finish_reason: Option<String>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    done: bool,
    reasoning: String,
    reasoning_tokens: Option<u64>,
    cached_prompt_tokens: Option<u64>,
    patience: Option<Patience>,
    prefill_tps: Option<f64>,
    decode_tps: Option<f64>,
    timing_cache_n: Option<u64>,
}

impl Accumulator {
    pub(super) fn with_patience(patience: Patience) -> Self {
        Self {
            patience: Some(patience),
            ..Self::default()
        }
    }

    /// Fold one `data:` payload in. Returns true on `[DONE]`. `on_text`
    /// returning false aborts with [`Interrupted`].
    #[cfg(test)]
    pub(super) fn feed(
        &mut self,
        data: &str,
        on_text: &mut dyn FnMut(&str) -> bool,
    ) -> Result<bool> {
        self.feed_with_events(data, on_text, &mut |_| true)
    }

    pub(super) fn feed_with_events(
        &mut self,
        data: &str,
        on_text: &mut dyn FnMut(&str) -> bool,
        on_event: &mut dyn FnMut(&super::StreamEvent) -> bool,
    ) -> Result<bool> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(false);
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        let event: Value =
            serde_json::from_str(data).context("chat stream event is not valid JSON")?;
        if let Some(error) = event.get("error").filter(|e| !e.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            let message = message.unwrap_or_else(|| error.to_string());
            let kinds: Vec<&str> = ["type", "code"]
                .iter()
                .filter_map(|key| error.get(*key)?.as_str())
                .collect();
            let class = errors::classify_stream_error(&kinds, &message);
            return Err(Classified::new(
                class,
                format!(
                    "model server reported a stream error: {}",
                    excerpt(message.as_bytes(), message.len() > 512)
                ),
            )
            .into());
        }
        let mut progress_seen = false;
        if let Some(raw) = event.get("dial_progress")
            && let Ok(mut progress) = serde_json::from_value::<super::ModelProgress>(raw.clone())
        {
            progress.normalize();
            progress_seen = true;
            // Dial's MLX runtime reports its generation rate on generating progress packets.
            if progress.phase == super::InferencePhase::Generating {
                self.decode_tps = progress.tokens_per_second.or(self.decode_tps);
            }
            if !on_event(&super::StreamEvent::Progress(progress)) {
                return Err(Interrupted.into());
            }
        }
        // llama.cpp `return_progress`: prompt-processing progress, outside the usage chunk.
        if let Some(pp) = event.get("prompt_progress").filter(|p| p.is_object()) {
            let mut progress = super::ModelProgress::new(super::InferencePhase::Processing);
            progress.processed_tokens = pp.get("processed").and_then(Value::as_u64);
            progress.prompt_tokens = pp.get("total").and_then(Value::as_u64);
            progress.cached_tokens = pp.get("cache").and_then(Value::as_u64);
            progress.normalize();
            progress_seen = true;
            if !on_event(&super::StreamEvent::Progress(progress)) {
                return Err(Interrupted.into());
            }
        }
        // llama.cpp `timings` (final chunk): measured prompt and generation rates, reused prompt tokens.
        if let Some(timings) = event.get("timings").filter(|t| t.is_object()) {
            let rate = |key: &str| {
                timings
                    .get(key)
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite() && *n > 0.0)
            };
            self.prefill_tps = rate("prompt_per_second").or(self.prefill_tps);
            self.decode_tps = rate("predicted_per_second").or(self.decode_tps);
            self.timing_cache_n = timings
                .get("cache_n")
                .and_then(Value::as_u64)
                .or(self.timing_cache_n);
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            progress_seen = true;
            self.prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .or(self.prompt_tokens);
            self.completion_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .or(self.completion_tokens);
            self.reasoning_tokens = usage
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .or(self.reasoning_tokens);
            self.cached_prompt_tokens = usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .or(self.cached_prompt_tokens);
            if self.completion_tokens.is_some() {
                let mut progress = super::ModelProgress::new(super::InferencePhase::Generating);
                progress.prompt_tokens = self.prompt_tokens;
                progress.output_tokens = self.completion_tokens;
                progress.tokens_per_second = event
                    .pointer("/timings/predicted_per_second")
                    .and_then(Value::as_f64);
                progress.normalize();
                if !on_event(&super::StreamEvent::Progress(progress)) {
                    return Err(Interrupted.into());
                }
            }
        }
        // `choices` is empty in the trailing usage chunk. Only choice 0 is used.
        let Some(choice) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| {
                choices
                    .iter()
                    .find(|c| c.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
            })
        else {
            if progress_seen {
                self.mark_progress();
            }
            return Ok(false);
        };
        if let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) {
            if let Some(piece) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    delta
                        .get("reasoning")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                })
            {
                progress_seen = true;
                self.reasoning.push_str(piece);
                if !on_event(&super::StreamEvent::Reasoning(piece.to_owned())) {
                    return Err(Interrupted.into());
                }
            }
            match delta.get("content") {
                Some(Value::String(piece)) if !piece.is_empty() => {
                    progress_seen = true;
                    self.text.push_str(piece);
                    if !on_text(piece) {
                        return Err(Interrupted.into());
                    }
                }
                Some(Value::String(_) | Value::Null) | None => {}
                Some(_) => bail!("chat stream content delta is not a string"),
            }
            if let Some(calls) = delta.get("tool_calls") {
                match calls {
                    Value::Array(calls) => {
                        progress_seen |= !calls.is_empty();
                        for call in calls {
                            self.add_tool_delta(call)?;
                            let (index, slot) = self.slot(call)?;
                            if let Some(piece) =
                                call.pointer("/function/arguments").and_then(Value::as_str)
                                && !on_event(&super::StreamEvent::ToolArguments {
                                    index,
                                    id: slot.id.clone(),
                                    name: slot.name.clone(),
                                    delta: piece.to_owned(),
                                })
                            {
                                return Err(Interrupted.into());
                            }
                        }
                    }
                    Value::Null => {}
                    _ => bail!("chat stream tool_calls is not an array"),
                }
            }
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            self.finish_reason = Some(reason.to_string());
            progress_seen = true;
            if let Some(patience) = &self.patience {
                patience.finish_within(POST_FINISH_GRACE);
            }
        }
        if progress_seen {
            self.mark_progress();
        }
        Ok(false)
    }

    fn mark_progress(&self) {
        if let Some(patience) = &self.patience {
            patience.mark_event();
        }
    }

    /// Calls are keyed by `index`. A delta without one continues the call with
    /// the same `id`, else the most recent call, else starts a new one.
    fn slot(&mut self, delta: &Value) -> Result<(u64, &mut PartialCall)> {
        let id = delta
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let key = match delta.get("index").and_then(Value::as_u64) {
            Some(index) => index,
            None => {
                let found = id
                    .and_then(|id| {
                        self.calls
                            .iter()
                            .find(|(_, call)| call.id == id)
                            .map(|(key, _)| *key)
                    })
                    .or_else(|| {
                        if id.is_none() {
                            self.calls.keys().next_back().copied()
                        } else {
                            None
                        }
                    });
                match found {
                    Some(key) => key,
                    None => match self.calls.keys().next_back() {
                        Some(last) => last
                            .checked_add(1)
                            .context("chat stream tool call index overflows")?,
                        None => 0,
                    },
                }
            }
        };
        ensure!(
            self.calls.contains_key(&key) || self.calls.len() < MAX_TOOL_CALLS,
            "chat stream carries more than {MAX_TOOL_CALLS} tool calls"
        );
        Ok((key, self.calls.entry(key).or_default()))
    }

    fn add_tool_delta(&mut self, delta: &Value) -> Result<()> {
        ensure!(
            delta.is_object(),
            "chat stream tool call delta is not an object"
        );
        let (_, slot) = self.slot(delta)?;
        if let Some(id) = delta.get("id").and_then(Value::as_str)
            && slot.id.is_empty()
        {
            slot.id = id.to_string();
        }
        let Some(function) = delta.get("function") else {
            return Ok(());
        };
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            // Names normally arrive once; tolerate pieces, ignore exact repeats.
            if slot.name.is_empty() {
                slot.name = name.to_string();
            } else if slot.name != name {
                slot.name.push_str(name);
            }
        }
        match function.get("arguments") {
            Some(Value::String(piece)) => slot.arguments.push_str(piece),
            // Some servers send an already-parsed object in one piece.
            Some(object @ Value::Object(_)) if slot.arguments.is_empty() => {
                slot.arguments = object.to_string()
            }
            _ => {}
        }
        Ok(())
    }

    /// The finished reply. A stream that ended without a `finish_reason` or
    /// `[DONE]` was cut off; its partial text is not returned.
    pub(super) fn is_complete(&self) -> bool {
        self.finish_reason.is_some() || self.done
    }

    pub(super) fn finish(self) -> Result<Reply> {
        if !self.is_complete() {
            return Err(IncompleteStream(
                "chat stream ended before the model finished (connection closed early)",
            )
            .into());
        }
        let calls: Vec<Value> = self
            .calls
            .into_values()
            .map(|call| {
                let mut function = Map::new();
                function.insert("name".into(), Value::String(call.name));
                function.insert("arguments".into(), Value::String(call.arguments));
                json!({ "id": call.id, "type": "function", "function": Value::Object(function) })
            })
            .collect();
        Ok(Reply {
            text: self.text,
            tool_calls: parse_tool_calls(&calls)?,
            finish_reason: self.finish_reason.unwrap_or_default(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            reasoning: self.reasoning,
            reasoning_tokens: self.reasoning_tokens,
            cached_prompt_tokens: self.cached_prompt_tokens.or(self.timing_cache_n),
            prefill_tokens_per_second: self.prefill_tps,
            decode_tokens_per_second: self.decode_tps,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests;
