//! Server-sent-event reading and chat-completion chunk accumulation.
//!
//! [`read_events`] turns a byte stream into `data:` payloads under hard bounds
//! (total bytes, bytes per line, silence). [`Accumulator`] folds
//! `chat.completion.chunk` payloads into a [`Reply`].

use super::cancel::Patience;
use super::{Interrupted, Reply, excerpt, parse_tool_calls};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};
#[cfg(test)]
use std::time::Instant;

/// Longest accepted SSE line. One chunk carries at most one tool call's
/// arguments (`mlx_lm.server` sends each call whole), so this bounds those.
pub(super) const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Most distinct tool calls accepted in one reply.
const MAX_TOOL_CALLS: usize = 128;

/// Read SSE from `reader`, handing every `data:` payload to `on_data`, until it
/// returns `Ok(true)` (stream finished) or the reader reaches EOF. Comments,
/// keep-alives and other fields (`event:`, `id:`, `retry:`) are skipped. Multiple
/// `data:` lines in one event are joined with newlines, per the SSE protocol.
///
/// Time is bounded by silence, never by the length of the reply: the clock restarts on every `data:` event and
/// only on those, so keep-alive comments or a trickle of bytes that never completes an event cannot hold the
/// connection open, while a long answer that keeps producing events can take as long as it needs.
pub(super) fn read_events(
    mut reader: impl Read,
    total_limit: usize,
    line_limit: usize,
    patience: Patience,
    mut on_data: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    let mut chunk = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut pending = String::new();
    let mut total = 0usize;
    let mut on_data = |data: &str| {
        patience.mark_event();
        on_data(data)
    };
    // The longest this stream may currently be silent, or `None` while waiting for the first event on a local server.
    let limit = || {
        if patience.first_event_seen() {
            Some(patience.idle)
        } else {
            patience.first_event
        }
    };
    let stalled = || {
        patience
            .remaining()
            .is_some_and(|remaining| remaining.is_zero())
    };
    let stall_message = || {
        format!(
            "chat stream was silent for {:?} (the idle limit); the server may be stuck or overloaded",
            limit().unwrap_or(patience.idle)
        )
    };
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                if stalled() || error.to_string().to_ascii_lowercase().contains("timeout") {
                    bail!("{}", stall_message());
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
                let done = dispatch(&line, &mut pending, &mut on_data)?;
                line.clear();
                if done {
                    return Ok(());
                }
            }
        }
        // Checked after the bytes were processed so a final chunk that
        // finishes the stream is never discarded for arriving late.
        ensure!(!stalled(), "{}", stall_message());
    }
    if !line.is_empty() {
        dispatch(&line, &mut pending, &mut on_data)?;
    }
    if !pending.is_empty() {
        on_data(&pending)?;
    }
    Ok(())
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
}

impl Accumulator {
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
            bail!(
                "model server reported a stream error: {}",
                excerpt(message.as_bytes(), message.len() > 512)
            );
        }
        if let Some(raw) = event.get("dial_progress") {
            if let Ok(mut progress) = serde_json::from_value::<super::ModelProgress>(raw.clone()) {
                progress.normalize();
                if !on_event(&super::StreamEvent::Progress(progress)) {
                    return Err(Interrupted.into());
                }
            }
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
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
            return Ok(false);
        };
        if let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) {
            if let Some(piece) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.reasoning.push_str(piece);
                if !on_event(&super::StreamEvent::Reasoning(piece.to_owned())) {
                    return Err(Interrupted.into());
                }
            }
            match delta.get("content") {
                Some(Value::String(piece)) if !piece.is_empty() => {
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
                        for call in calls {
                            self.add_tool_delta(call)?;
                            let slot = self.slot(call)?;
                            if let Some(piece) =
                                call.pointer("/function/arguments").and_then(Value::as_str)
                                && !on_event(&super::StreamEvent::ToolArguments {
                                    index: call["index"].as_u64().unwrap_or(0),
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
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        Ok(false)
    }

    /// Calls are keyed by `index`. A delta without one continues the call with
    /// the same `id`, else the most recent call, else starts a new one.
    fn slot(&mut self, delta: &Value) -> Result<&mut PartialCall> {
        let id = delta
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let key = match delta.get("index").and_then(Value::as_u64) {
            Some(index) => index,
            None => id
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
                })
                .unwrap_or_else(|| self.calls.keys().next_back().map_or(0, |last| last + 1)),
        };
        ensure!(
            self.calls.contains_key(&key) || self.calls.len() < MAX_TOOL_CALLS,
            "chat stream carries more than {MAX_TOOL_CALLS} tool calls"
        );
        Ok(self.calls.entry(key).or_default())
    }

    fn add_tool_delta(&mut self, delta: &Value) -> Result<()> {
        ensure!(
            delta.is_object(),
            "chat stream tool call delta is not an object"
        );
        let slot = self.slot(delta)?;
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
    pub(super) fn finish(self) -> Result<Reply> {
        ensure!(
            self.finish_reason.is_some() || self.done,
            "chat stream ended before the model finished (connection closed early)"
        );
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
            cached_prompt_tokens: self.cached_prompt_tokens,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    fn far() -> Patience {
        Patience::new(None, Duration::from_secs(60))
    }

    fn events(raw: &[u8]) -> Result<Vec<String>> {
        let mut seen = Vec::new();
        read_events(Cursor::new(raw.to_vec()), 1 << 20, 1 << 16, far(), |data| {
            seen.push(data.to_string());
            Ok(data == "[DONE]")
        })?;
        Ok(seen)
    }

    #[test]
    fn sse_framing_tolerates_comments_crlf_and_other_fields() {
        let raw = b": keepalive\r\n\r\nevent: message\r\nid: 7\r\nretry: 10\r\ndata: {\"a\":1}\r\n\r\ndata:{\"b\":2}\n\n: ping\ndata: [DONE]\n\ndata: ignored-after-done\n";
        assert_eq!(events(raw).unwrap(), ["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
    }

    #[test]
    fn multiline_sse_payload_is_one_event() {
        assert_eq!(
            events(
                b"event: content_block_delta\ndata: {\"type\":\"ping\",\ndata: \"extra\":true}\n\n"
            )
            .unwrap(),
            ["{\"type\":\"ping\",\n\"extra\":true}"]
        );
    }

    #[test]
    fn final_line_without_newline_is_delivered() {
        assert_eq!(events(b"data: {\"a\":1}").unwrap(), ["{\"a\":1}"]);
    }

    #[test]
    fn live_progress_is_cancellable_and_never_becomes_answer_or_billed_usage() {
        use crate::llm::{InferencePhase, StreamEvent};
        let mut acc = Accumulator::default();
        let mut seen = Vec::new();
        for event in [
            json!({"dial_progress":{"phase":"processing","processed_tokens":100,"prompt_tokens":80}}),
            json!({"dial_progress":{"phase":"generating","output_tokens":12,"tokens_per_second":24}}),
            json!({"choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}),
        ] {
            acc.feed_with_events(&event.to_string(), &mut |_| true, &mut |e| {
                seen.push(e.clone());
                true
            })
            .unwrap();
        }
        let StreamEvent::Progress(p) = &seen[0] else {
            panic!("missing processing progress")
        };
        assert_eq!(p.phase, InferencePhase::Processing);
        assert_eq!(p.processed_tokens, Some(80));
        let reply = acc.finish().unwrap();
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.completion_tokens, None);
        assert!(reply.tool_calls.is_empty());
        let mut acc = Accumulator::default();
        let err = acc
            .feed_with_events(
                &json!({"dial_progress":{"phase":"generating","output_tokens":1}}).to_string(),
                &mut |_| true,
                &mut |_| false,
            )
            .unwrap_err();
        assert!(err.downcast_ref::<Interrupted>().is_some());
    }

    #[test]
    fn line_and_total_caps() {
        let long = format!("data: {}\n", "x".repeat(100));
        let error = read_events(Cursor::new(long.into_bytes()), 1 << 20, 50, far(), |_| {
            Ok(false)
        })
        .unwrap_err();
        assert!(error.to_string().contains("line exceeds"), "{error}");
        let many = "data: 0123456789\n".repeat(100);
        let error = read_events(Cursor::new(many.into_bytes()), 500, 50, far(), |_| {
            Ok(false)
        })
        .unwrap_err();
        assert!(error.to_string().contains("stream exceeds"), "{error}");
    }

    #[test]
    fn silence_is_bounded_but_a_long_steady_stream_is_not() {
        // Bytes that never complete an event are not progress: a keep-alive trickle cannot hold the stream open.
        struct Drip;
        impl Read for Drip {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(30));
                buf[0] = b':';
                Ok(1)
            }
        }
        let patience = Patience::new(Some(Duration::from_millis(200)), Duration::from_millis(200));
        let started = Instant::now();
        let error = read_events(Drip, 1 << 20, 1 << 10, patience, |_| Ok(false)).unwrap_err();
        assert!(error.to_string().contains("silent"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));

        // A reply that takes far longer than the idle limit overall but never goes quiet for that long is fine,
        // and so is a cold start before the first event when the first-event wait is unbounded (a local server).
        struct Steady {
            delay_first: Duration,
            sent: usize,
        }
        impl Read for Steady {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(if self.sent == 0 {
                    self.delay_first
                } else {
                    Duration::from_millis(40)
                });
                self.sent += 1;
                let line: &[u8] = if self.sent > 12 {
                    b"data: [DONE]\n\n"
                } else {
                    b"data: tick\n\n"
                };
                buf[..line.len()].copy_from_slice(line);
                Ok(line.len())
            }
        }
        let local = Patience::new(None, Duration::from_millis(150));
        let started = Instant::now();
        let mut ticks = 0;
        read_events(
            Steady {
                delay_first: Duration::from_millis(400),
                sent: 0,
            },
            1 << 20,
            1 << 10,
            local,
            |data| {
                ticks += 1;
                Ok(data == "[DONE]")
            },
        )
        .unwrap();
        assert_eq!(ticks, 13);
        assert!(
            started.elapsed() > Duration::from_millis(600),
            "the reply outlived the idle limit several times over"
        );
    }

    fn run(chunks: &[Value]) -> (Result<Reply>, String) {
        let mut acc = Accumulator::default();
        let mut streamed = String::new();
        let mut sink = |s: &str| {
            streamed.push_str(s);
            true
        };
        for chunk in chunks {
            if let Err(e) = acc.feed(&chunk.to_string(), &mut sink) {
                return (Err(e), streamed);
            }
        }
        (acc.finish(), streamed)
    }

    fn delta(content: &str) -> Value {
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}]})
    }

    #[test]
    fn text_deltas_usage_and_finish() {
        let (reply, streamed) = run(&[
            delta("Hel"),
            delta(""),
            delta("lo"),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}}),
        ]);
        let reply = reply.unwrap();
        assert_eq!(streamed, "Hello");
        assert_eq!(reply.text, "Hello");
        assert_eq!(reply.finish_reason, "stop");
        assert_eq!(
            (reply.prompt_tokens, reply.completion_tokens),
            (Some(9), Some(2))
        );
    }

    #[test]
    fn tool_call_split_across_deltas_by_index() {
        let part = |index: u64, id: Option<&str>, name: Option<&str>, args: &str| {
            let mut function = json!({"arguments": args});
            if let Some(name) = name {
                function["name"] = json!(name);
            }
            let mut call = json!({"index": index, "function": function});
            if let Some(id) = id {
                call["id"] = json!(id);
                call["type"] = json!("function");
            }
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]})
        };
        let (reply, streamed) = run(&[
            part(0, Some("c0"), Some("read"), ""),
            part(1, Some("c1"), Some("bash"), "{\"cm"),
            part(0, None, None, "{\"path\": \"a"),
            part(1, None, None, "d\": \"ls\"}"),
            part(0, None, None, ".txt\"}"),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]);
        let reply = reply.unwrap();
        assert_eq!(streamed, "");
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(
            (
                reply.tool_calls[0].id.as_str(),
                reply.tool_calls[0].name.as_str()
            ),
            ("c0", "read")
        );
        assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a.txt"}));
        assert_eq!(reply.tool_calls[1].arguments, json!({"cmd": "ls"}));
    }

    #[test]
    fn whole_call_in_one_delta_like_mlx_lm() {
        // Shape emitted by mlx_lm.server 0.32: complete call, string arguments, index set.
        let call = json!({"function": {"name": "read_file", "arguments": "{\"path\": \"notes.txt\"}"},
                          "type": "function", "id": "uuid-1", "index": 0});
        let (reply, _) = run(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [call]}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": "tool_calls"}]}),
        ]);
        let reply = reply.unwrap();
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert_eq!(reply.tool_calls[0].arguments, json!({"path": "notes.txt"}));
    }

    #[test]
    fn deltas_without_index_continue_the_previous_call() {
        let (reply, _) = run(&[
            json!({"choices": [{"delta": {"tool_calls": [{"id": "a", "function": {"name": "x", "arguments": "{\"k\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": " 1}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"id": "b", "function": {"name": "y", "arguments": "{}"}}]}}]}),
            json!({"choices": [{"finish_reason": "tool_calls"}]}),
        ]);
        let calls = reply.unwrap().tool_calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].name.as_str(), calls[0].arguments.clone()),
            ("x", json!({"k": 1}))
        );
        assert_eq!(calls[1].id, "b");
    }

    #[test]
    fn truncated_stream_discards_partial_text() {
        let (reply, streamed) = run(&[delta("par"), delta("tial")]);
        assert_eq!(streamed, "partial", "text was already shown to the user");
        let error = reply.unwrap_err().to_string();
        assert!(error.contains("ended before"), "{error}");
    }

    #[test]
    fn done_without_finish_reason_is_accepted() {
        let mut acc = Accumulator::default();
        let mut sink = |_: &str| true;
        acc.feed(&delta("ok").to_string(), &mut sink).unwrap();
        assert!(acc.feed("[DONE]", &mut sink).unwrap());
        assert_eq!(acc.finish().unwrap().text, "ok");
    }

    #[test]
    fn only_choice_zero_is_used_and_reasoning_is_separate_from_visible_text() {
        let (reply, streamed) = run(&[
            json!({"choices": [
                {"index": 1, "delta": {"content": "other"}},
                {"index": 0, "delta": {"content": "mine", "reasoning": "thinking..."}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]}),
        ]);
        assert_eq!(streamed, "mine");
        let reply = reply.unwrap();
        assert_eq!(reply.finish_reason, "length");
        assert_eq!(reply.reasoning, "thinking...");
    }

    #[test]
    fn callback_false_interrupts_with_typed_error() {
        let mut acc = Accumulator::default();
        let error = acc
            .feed(&delta("x").to_string(), &mut |_| false)
            .unwrap_err();
        assert!(error.downcast_ref::<Interrupted>().is_some());
    }

    #[test]
    fn in_stream_error_and_garbage_are_errors() {
        let (reply, _) =
            run(&[json!({"error": {"message": "overloaded", "type": "server_error"}})]);
        assert!(reply.unwrap_err().to_string().contains("overloaded"));
        let mut acc = Accumulator::default();
        assert!(acc.feed("{not json", &mut |_| true).is_err());
        assert!(
            acc.feed("{\"choices\":[{\"delta\":{\"content\":5}}]}", &mut |_| true)
                .is_err()
        );
    }

    #[test]
    fn tool_call_count_is_bounded() {
        let mut acc = Accumulator::default();
        let mut error = None;
        for index in 0..=MAX_TOOL_CALLS as u64 {
            let chunk = json!({"choices": [{"delta": {"tool_calls": [{"index": index, "function": {"name": "n"}}]}}]});
            if let Err(e) = acc.feed(&chunk.to_string(), &mut |_| true) {
                error = Some(e);
            }
        }
        assert!(error.is_some_and(|e| e.to_string().contains("more than")));
    }

    #[test]
    fn name_repeats_are_idempotent_and_pieces_concatenate() {
        let (reply, _) = run(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "z", "function": {"name": "re"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "ad", "arguments": "{}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "read", "arguments": ""}}]}}]}),
            json!({"choices": [{"finish_reason": "tool_calls"}]}),
        ]);
        assert_eq!(reply.unwrap().tool_calls[0].name, "read");
    }
}
