use super::fake::{self, Script, header, request_line};
use super::*;
use std::time::Instant;

fn chat_request(tools: Vec<Value>) -> Chat {
    Chat {
        messages: vec![json!({"role": "user", "content": "SECRET-PROMPT"})],
        tools,
        temperature: 0.0,
        max_tokens: 32,
        seed: None,
    }
}

fn a_tool() -> Vec<Value> {
    vec![
        json!({"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}),
    ]
}

fn sse(events: &[Value]) -> String {
    let mut out: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn delta(content: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]})
}

fn stop() -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
}

fn collect(
    url: &str,
    auth: Option<&str>,
    req: &Chat,
    timeout: u64,
) -> (Result<Reply>, Vec<String>) {
    let mut pieces = Vec::new();
    let mut on_text = |piece: &str| {
        pieces.push(piece.to_string());
        true
    };
    let result = chat_stream(url, auth, SERVER_MODEL, req, timeout, &mut on_text);
    (result, pieces)
}

#[test]
fn completions_url_rules() {
    for (base, want) in [
        (
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/v1/chat/completions",
        ),
        (
            "http://127.0.0.1:8080/",
            "http://127.0.0.1:8080/v1/chat/completions",
        ),
        (
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://api.openai.com/v1/",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://h/openai/v1beta",
            "https://h/openai/v1beta/chat/completions",
        ),
        (
            "https://h/v1/chat/completions",
            "https://h/v1/chat/completions",
        ),
    ] {
        assert_eq!(completions_url(base), want, "{base}");
    }
}

#[test]
fn streams_text_with_keepalives_and_usage() {
    let body = format!(
        ": keepalive\n\n{}",
        sse(&[
            json!({"choices": [{"delta": {"role": "assistant"}}]}),
            delta("Hel"),
            delta("lo"),
            stop(),
            json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}),
        ])
    );
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, pieces) = collect(&server.url, None, &chat_request(vec![]), 10);
    let reply = reply.unwrap();
    assert_eq!(pieces, ["Hel", "lo"]);
    assert_eq!(reply.text, "Hello");
    assert_eq!(reply.finish_reason, "stop");
    assert_eq!(
        (reply.prompt_tokens, reply.completion_tokens),
        (Some(12), Some(3))
    );
    let request = server.requests().remove(0);
    assert_eq!(request_line(&request), "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(
        header(&request, "accept").as_deref(),
        Some("text/event-stream")
    );
    let sent = fake::body(&request);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(sent.get("tools"), Some(&json!([])));
}

#[test]
fn streams_tool_call_deltas() {
    let call = |index: u64, id: Option<&str>, name: Option<&str>, args: &str| {
        let mut function = json!({"arguments": args});
        if let Some(name) = name {
            function["name"] = json!(name);
        }
        let mut call = json!({"index": index, "function": function});
        if let Some(id) = id {
            call["id"] = json!(id);
        }
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]})
    };
    let body = sse(&[
        call(0, Some("t0"), Some("read_file"), ""),
        call(0, None, None, "{\"path\":"),
        call(0, None, None, " \"a.txt\"}"),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ]);
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, pieces) = collect(&server.url, None, &chat_request(a_tool()), 10);
    let reply = reply.unwrap();
    assert!(pieces.is_empty());
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "t0");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a.txt"}));
}

#[test]
fn truncated_stream_is_an_error() {
    let body = format!("data: {}\n\ndata: {}\n\n", delta("par"), delta("tial"));
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, pieces) = collect(&server.url, None, &chat_request(vec![]), 10);
    let error = reply.unwrap_err().to_string();
    assert!(error.contains("ended before"), "{error}");
    assert_eq!(pieces.concat(), "partial");
}

#[test]
fn finish_reason_without_done_is_complete() {
    let body = format!("data: {}\n\ndata: {}\n\n", delta("ok"), stop());
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 10);
    assert_eq!(reply.unwrap().text, "ok");
}

#[test]
fn in_stream_error_event_is_reported() {
    let body = format!(
        "data: {}\n\n",
        json!({"error": {"message": "context length exceeded"}})
    );
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 10);
    assert!(
        reply
            .unwrap_err()
            .to_string()
            .contains("context length exceeded")
    );
}

#[test]
fn oversized_stream_is_rejected() {
    let event = format!("data: {}\n\n", delta(&"z".repeat(60_000)));
    let body = event.repeat(MAX_STREAM_BYTES / event.len() + 2);
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 20);
    let error = reply.unwrap_err().to_string();
    assert!(error.contains("stream exceeds"), "{error}");
}

#[test]
fn oversized_line_is_rejected() {
    let body = format!(
        "data: {}\n\n",
        delta(&"z".repeat(stream::MAX_LINE_BYTES + 10))
    );
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 20);
    let error = reply.unwrap_err().to_string();
    assert!(error.contains("line exceeds"), "{error}");
}

#[test]
fn keepalive_comments_do_not_keep_a_started_reply_alive() {
    // Every read succeeds quickly; only the idle limit stops this. Comments are not progress once a reply began.
    let first = format!("data: {}\n\n", delta("Hel"));
    let server = fake::serve(vec![Script::DripAfter {
        first,
        pause: Duration::from_millis(40),
        count: 500,
    }]);
    let started = Instant::now();
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 1);
    let error = reply.unwrap_err().to_string();
    assert!(error.contains("silent"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn silence_after_the_first_event_hits_the_idle_limit() {
    let first = format!("data: {}\n\n", delta("Hel"));
    let server = fake::serve(vec![Script::StallAfter {
        first,
        silence: Duration::from_secs(6),
    }]);
    let started = Instant::now();
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 1);
    let error = reply.unwrap_err().to_string();
    assert!(error.contains("silent"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_local_cold_start_longer_than_the_idle_limit_still_completes() {
    // A model server reading a long prompt sends its headers, then nothing, for longer than the idle limit.
    // That is not a stall: the first-event wait is unbounded on a local server, and the reply that follows is
    // judged by silence, not by the total time the call took.
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        delta("ok"),
        stop(),
        "[DONE]"
    );
    cancel::Patience::with_test_idle_limit(Duration::from_millis(200));
    let server = fake::serve(vec![Script::ColdStart {
        delay: Duration::from_millis(400),
        body,
    }]);
    let started = Instant::now();
    let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 1);
    assert_eq!(reply.unwrap().text, "ok");
    assert!(
        started.elapsed() > Duration::from_millis(350),
        "the idle limit was 150ms, so this call outlived it"
    );
}

#[test]
fn callback_false_interrupts_and_drops_the_connection() {
    let pieces: Vec<String> = (0..50)
        .map(|i| format!("data: {}\n\n", delta(&format!("w{i} "))))
        .collect();
    let server = fake::serve(vec![Script::SseSlow {
        pieces,
        pause: Duration::from_millis(100),
    }]);
    let mut seen = Vec::new();
    let started = Instant::now();
    let mut on_text = |piece: &str| {
        seen.push(piece.to_string());
        seen.len() < 2
    };
    let error = chat_stream(
        &server.url,
        None,
        SERVER_MODEL,
        &chat_request(vec![]),
        30,
        &mut on_text,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    assert_eq!(error.to_string(), "interrupted");
    assert_eq!(seen.len(), 2, "no deltas after the refusal");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn server_ignoring_stream_flag_still_works() {
    let body = json!({"choices": [{"finish_reason": "stop", "message": {"content": "whole"}}],
                      "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
    let server = fake::serve(vec![fake::json(200, &body)]);
    let (reply, pieces) = collect(&server.url, None, &chat_request(vec![]), 10);
    assert_eq!(reply.unwrap().text, "whole");
    assert_eq!(pieces, ["whole"]);
}

#[test]
fn unstreamed_reply_routes_leaked_think_blocks_out_of_visible_text() {
    let body = json!({"choices": [{"finish_reason": "stop", "message": {"content": "<think>plan it</think>Answer"}}],
                      "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
    let server = fake::serve(vec![fake::json(200, &body)]);
    let reply = chat_at(&server.url, None, SERVER_MODEL, &chat_request(vec![]), 10).unwrap();
    assert_eq!(reply.text, "Answer");
    assert_eq!(reply.reasoning, "plan it");
    let replay = assistant_message(&reply);
    assert_eq!(replay["content"], "Answer");
    assert_eq!(replay["reasoning_content"], "plan it");
}

#[test]
fn llama_server_stream_keeps_think_blocks_out_of_text_and_replays_the_tool_call() {
    let body = sse(&[
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": "Look"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": " up"}, "finish_reason": null}]}),
        delta("<think>ponder</think>Ok"),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_0", "type": "function",
            "function": {"name": "read_file", "arguments": ""}}]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"a\"}"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "timings": {"predicted_n": 9}}),
    ]);
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, pieces) = collect(&server.url, None, &chat_request(a_tool()), 10);
    let reply = reply.unwrap();
    assert_eq!(pieces.concat(), "Ok");
    assert_eq!(reply.text, "Ok");
    assert!(
        reply.reasoning.starts_with("Look up"),
        "{}",
        reply.reasoning
    );
    assert!(reply.reasoning.contains("ponder"), "{}", reply.reasoning);
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a"}));
    let replay = assistant_message(&reply);
    assert_eq!(replay["content"], "Ok");
    assert!(
        replay["reasoning_content"]
            .as_str()
            .is_some_and(|r| r.starts_with("Look up"))
    );
    assert_eq!(
        replay["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"a\"}"
    );
}

#[test]
fn tls_bad_record_mac_reconnects_only_before_a_response() {
    let error = transport_error(
        ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(rustls::AlertDescription::BadRecordMac),
        )),
        "https://chatgpt.com/backend-api/codex/responses?secret=hidden",
        None,
    );
    assert!(is_transient(&error), "{error:#}");
    assert!(!error.to_string().contains("secret"));
    assert!(!is_transient(&response_error(error, None)));

    for tls in [
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
        rustls::Error::General("received fatal alert: BadRecordMac".into()),
    ] {
        let error = transport_error(
            ureq::Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, tls)),
            "https://chatgpt.com/backend-api/codex/responses",
            None,
        );
        assert!(!is_transient(&error), "{error:#}");
    }
    let error = transport_error(
        ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "received fatal alert: BadRecordMac",
        )),
        "https://chatgpt.com/backend-api/codex/responses",
        None,
    );
    assert!(
        !is_transient(&error),
        "text alone must not classify a TLS error"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn socket_invalid_argument_retries_only_before_a_response() {
    let error = transport_error(
        ureq::Error::Io(std::io::Error::from_raw_os_error(libc::EINVAL)),
        "https://api.anthropic.com/v1/messages?secret=hidden",
        None,
    );
    assert!(is_transient(&error), "{error:#}");
    assert!(!error.to_string().contains("secret"));
    // Invalid headers/settings are client errors, even when their I/O kind is InvalidInput.
    let invalid = transport_error(
        ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid setting",
        )),
        "https://api.anthropic.com/v1/messages",
        None,
    );
    assert!(!is_transient(&invalid));
    // Once a response is being consumed, reconnecting could duplicate partial output.
    assert!(!is_transient(&response_error(error, None)));
}

#[test]
fn http_errors_are_typed_and_classified() {
    let server = fake::serve(vec![
        fake::json(429, &json!({"error": "slow down"})),
        fake::json(503, &json!({"error": "busy"})),
    ]);
    let (limited, _) = collect(&server.url, None, &chat_request(vec![]), 10);
    let limited = limited.unwrap_err();
    assert_eq!(
        limited.downcast_ref::<HttpError>().map(|e| e.status),
        Some(429)
    );
    assert!(is_transient(&limited));
    let url = server.url.clone();
    let busy = chat(&url, SERVER_MODEL, &chat_request(vec![]), 10).unwrap_err();
    assert!(is_transient(&busy), "{busy:#}");
    assert!(busy.to_string().contains("HTTP 503"));

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let refused = chat(
        &format!("http://127.0.0.1:{port}"),
        SERVER_MODEL,
        &chat_request(vec![]),
        5,
    )
    .unwrap_err();
    assert!(is_transient(&refused), "{refused:#}");
}

#[test]
fn server_error_messages_replace_raw_json_and_load_failures_get_a_next_step() {
    let server = fake::serve(vec![
        fake::json(
            400,
            &json!({"error": {"message": "Failed to load model \"big-model\". Error: out of memory", "type": "invalid_request_error"}}),
        ),
        fake::json(500, &json!({"error": "plain string error"})),
        fake::json(
            400,
            &json!({"error": {"message": "Unrelated validation failure"}}),
        ),
    ]);
    let load = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
        .unwrap_err()
        .to_string();
    assert!(
        load.contains("Failed to load model \"big-model\""),
        "{load}"
    );
    assert!(
        !load.contains("invalid_request_error") && !load.contains('{'),
        "no raw JSON in the message: {load}"
    );
    assert!(load.contains("Load it in LM Studio first"), "{load}");
    let plain = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
        .unwrap_err()
        .to_string();
    assert!(
        plain.contains("HTTP 500")
            && plain.contains("plain string error")
            && !plain.contains("LM Studio"),
        "{plain}"
    );
    let other = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
        .unwrap_err()
        .to_string();
    assert!(
        other.contains("Unrelated validation failure") && !other.contains("LM Studio"),
        "the hint is only for load failures: {other}"
    );
}

#[test]
fn generic_error_message_reports_provider_error_type() {
    let server = fake::serve(vec![fake::json(
        400,
        &json!({"type":"error","error":{"type":"permission_error","message":"Error"}}),
    )]);
    let err = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
        .unwrap_err()
        .to_string();
    assert!(err.contains("(type: permission_error)"), "{err}");
}

#[test]
fn image_support_hint_requires_an_explicit_model_capability_error() {
    for (message, hint) in [
        (
            "messages.1.content.0: Input tag 'dial_response_item' found using 'type' does not match any expected tags: 'document', 'image', 'text', 'tool_use'",
            false,
        ),
        ("Image exceeds maximum dimensions", false),
        ("Invalid image format", false),
        ("This model does not support image input", true),
        (
            "Invalid content type. image_url is only supported by certain models.",
            true,
        ),
        ("Image inputs are not supported by this model", true),
    ] {
        let error = http_error(
            400,
            &Bounded {
                data: serde_json::to_vec(
                    &json!({"error":{"type":"invalid_request_error","message":message}}),
                )
                .unwrap(),
                truncated: false,
            },
            None,
            None,
        )
        .to_string();
        assert_eq!(
            error.contains("select a vision-capable model"),
            hint,
            "{error}"
        );
        assert!(
            error.contains(message),
            "keep the actual provider error: {error}"
        );
    }
}

#[test]
fn bearer_token_is_sent_only_when_given_and_never_echoed() {
    let ok = json!({"choices": [{"finish_reason": "stop", "message": {"content": "hi"}}]});
    let server = fake::serve(vec![fake::json(200, &ok), fake::json(200, &ok)]);
    chat_with_auth(
        &server.url,
        Some("sk-secret-123"),
        SERVER_MODEL,
        &chat_request(vec![]),
        5,
    )
    .unwrap();
    chat_with_auth(&server.url, None, SERVER_MODEL, &chat_request(vec![]), 5).unwrap();
    let requests = server.requests();
    assert_eq!(
        header(&requests[0], "authorization").as_deref(),
        Some("Bearer sk-secret-123")
    );
    assert_eq!(header(&requests[1], "authorization"), None);

    // A server that echoes the credential back must not make it into the error.
    let echo = fake::serve(vec![fake::json(
        401,
        &json!({"error": "invalid key sk-secret-123"}),
    )]);
    let error = chat_with_auth(
        &echo.url,
        Some("sk-secret-123"),
        SERVER_MODEL,
        &chat_request(vec![]),
        5,
    )
    .unwrap_err();
    let text = format!("{error:#} {error:?}");
    assert!(!text.contains("sk-secret-123"), "{text}");
    assert!(
        text.contains("HTTP 401") && text.contains("[redacted]"),
        "{text}"
    );

    // Unusable tokens are refused without being quoted.
    for bad in ["bad token", "line\nbreak", ""] {
        let error = chat_with_auth(
            "http://127.0.0.1:9",
            Some(bad),
            SERVER_MODEL,
            &chat_request(vec![]),
            2,
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("api key"), "{text}");
        assert!(bad.is_empty() || !text.contains(bad), "{text}");
    }

    // Connection failures do not carry the token either.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let error = chat_with_auth(
        &format!("http://127.0.0.1:{port}"),
        Some("sk-secret-123"),
        SERVER_MODEL,
        &chat_request(vec![]),
        2,
    )
    .unwrap_err();
    assert!(!format!("{error:#}").contains("sk-secret-123"));
}

#[test]
fn text_tool_calls_become_structured_when_tools_are_offered() {
    let text = "I'll read it.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n</tool_call>";
    let body = json!({"choices": [{"finish_reason": "stop", "message": {"content": text}}]});
    let server = fake::serve(vec![fake::json(200, &body), fake::json(200, &body)]);
    let reply = chat(&server.url, SERVER_MODEL, &chat_request(a_tool()), 5).unwrap();
    assert_eq!(reply.text, "I'll read it.");
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a.txt"}));
    // Without tools the text is just text.
    let plain = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5).unwrap();
    assert_eq!(plain.text, text);
    assert!(plain.tool_calls.is_empty());
}

#[test]
fn streamed_text_tool_calls_are_hidden_from_the_text_callback() {
    let body = sse(&[
        delta("Checking "),
        delta("<tool"),
        delta("_call>{\"name\": \"read_file\", "),
        delta("\"arguments\": {\"path\": \"x\"}}</tool_call>"),
        stop(),
    ]);
    let server = fake::serve(vec![Script::Sse(body)]);
    let (reply, pieces) = collect(&server.url, None, &chat_request(a_tool()), 10);
    let reply = reply.unwrap();
    assert_eq!(pieces.concat(), "Checking ");
    assert_eq!(reply.text, "Checking");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path": "x"}));
    assert_eq!(reply.finish_reason, "tool_calls");
}

#[test]
fn structured_calls_are_left_alone_by_the_text_fallback() {
    let body = json!({"choices": [{"finish_reason": "tool_calls", "message": {
        "content": "<tool_call>{\"name\":\"other\",\"arguments\":{}}</tool_call>",
        "tool_calls": [{"id": "s", "function": {"name": "read_file", "arguments": "{}"}}]}}]});
    let server = fake::serve(vec![fake::json(200, &body)]);
    let reply = chat(&server.url, SERVER_MODEL, &chat_request(a_tool()), 5).unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "read_file");
}
