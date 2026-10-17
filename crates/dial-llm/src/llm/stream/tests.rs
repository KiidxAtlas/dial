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
fn connection_loss_and_timeouts_are_classified_as_incomplete_streams() {
    struct Fails(ErrorKind);
    impl Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "Peer disconnected"))
        }
    }
    for kind in [
        ErrorKind::UnexpectedEof,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::BrokenPipe,
        ErrorKind::TimedOut,
        ErrorKind::InvalidData,
        ErrorKind::Other,
    ] {
        let error = read_events(Fails(kind), 1024, 1024, far(), |_| Ok(false)).unwrap_err();
        let reconnect = matches!(
            kind,
            ErrorKind::UnexpectedEof
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
        );
        assert_eq!(
            error.downcast_ref::<IncompleteStream>().is_some(),
            reconnect,
            "{kind:?}: {error:#}"
        );
        assert_eq!(
            error.downcast_ref::<StalledStream>().is_some(),
            kind == ErrorKind::TimedOut
        );
    }
}

#[test]
fn sse_framing_tolerates_comments_crlf_and_other_fields() {
    let raw = b": keepalive\r\n\r\nevent: message\r\nid: 7\r\nretry: 10\r\ndata: {\"a\":1}\r\n\r\ndata:{\"b\":2}\n\n: ping\ndata: [DONE]\n\ndata: ignored-after-done\n";
    assert_eq!(events(raw).unwrap(), ["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
}

#[test]
fn multiline_sse_payload_is_one_event() {
    assert_eq!(
        events(b"event: content_block_delta\ndata: {\"type\":\"ping\",\ndata: \"extra\":true}\n\n")
            .unwrap(),
        ["{\"type\":\"ping\",\n\"extra\":true}"]
    );
}

#[test]
fn final_line_without_newline_is_delivered() {
    assert_eq!(events(b"data: {\"a\":1}").unwrap(), ["{\"a\":1}"]);
}

#[test]
fn llama_cpp_prompt_progress_and_timings_become_telemetry() {
    use crate::llm::{InferencePhase, StreamEvent};
    let mut acc = Accumulator::default();
    let mut seen = Vec::new();
    for event in [
        json!({"choices":[{"index":0,"delta":{}}],"prompt_progress":{"total":1000,"cache":600,"processed":800,"time_ms":120}}),
        json!({"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],
            "timings":{"cache_n":600,"prompt_per_second":250.0,"predicted_per_second":42.5}}),
    ] {
        acc.feed_with_events(&event.to_string(), &mut |_| true, &mut |e| {
            seen.push(e.clone());
            true
        })
        .unwrap();
    }
    let StreamEvent::Progress(p) = &seen[0] else {
        panic!("missing prompt progress")
    };
    assert_eq!(p.phase, InferencePhase::Processing);
    assert_eq!(
        (p.processed_tokens, p.prompt_tokens, p.cached_tokens),
        (Some(800), Some(1000), Some(600))
    );
    let reply = acc.finish().unwrap();
    assert_eq!(reply.text, "hi");
    assert_eq!(reply.prefill_tokens_per_second, Some(250.0));
    assert_eq!(reply.decode_tokens_per_second, Some(42.5));
    assert_eq!(reply.cached_prompt_tokens, Some(600));

    // A usage-reported cache count is the authority; timings only fill the gap.
    let mut acc = Accumulator::default();
    let event = json!({"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1000,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":5}},
        "timings":{"cache_n":600}});
    acc.feed(&event.to_string(), &mut |_| true).unwrap();
    assert_eq!(acc.finish().unwrap().cached_prompt_tokens, Some(5));
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
    let (reply, _) = run(&[json!({"error": {"message": "overloaded", "type": "server_error"}})]);
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
