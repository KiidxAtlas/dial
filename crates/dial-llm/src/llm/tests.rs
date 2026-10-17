use super::*;
use std::io::Write;
use std::net::TcpListener;
use std::thread::JoinHandle;

/// Serve exactly one HTTP request with a canned response; returns the base
/// URL and a handle yielding the raw request bytes the client sent.
fn serve_once(status: u16, body: Vec<u8>) -> (String, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0, "client closed before sending a request");
            request.extend_from_slice(&chunk[..count]);
            if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(|value| value.trim().parse().unwrap())
            .unwrap_or(0);
        while request.len() < header_end + length {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0, "client closed mid-body");
            request.extend_from_slice(&chunk[..count]);
        }
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        // The client may hang up early (oversized responses); that is fine.
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
        request
    });
    (url, handle)
}

fn request(tools: Vec<Value>, seed: Option<u64>) -> Chat {
    Chat {
        messages: vec![json!({"role": "user", "content": "SECRET-PROMPT"})],
        tools,
        temperature: 0.0,
        max_tokens: 16,
        seed,
    }
}

fn body_of(raw: &[u8]) -> Value {
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    serde_json::from_slice(&raw[at..]).unwrap()
}

#[test]
fn text_reply_and_request_shape() {
    let reply = json!({
        "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "hello"}}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 2}
    });
    let (url, server) = serve_once(200, reply.to_string().into_bytes());
    let reply = chat(&url, SERVER_MODEL, &request(vec![], Some(9)), 5).unwrap();
    assert_eq!(reply.text, "hello");
    assert!(reply.tool_calls.is_empty());
    assert_eq!(reply.finish_reason, "stop");
    assert_eq!(
        (reply.prompt_tokens, reply.completion_tokens),
        (Some(7), Some(2))
    );
    let sent = body_of(&server.join().unwrap());
    assert_eq!(sent["model"], SERVER_MODEL);
    assert_eq!(sent["seed"], 9);
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["max_tokens"], 16);
    assert_eq!(sent.get("tools"), Some(&json!([])));
}

#[test]
fn tool_call_with_string_arguments() {
    let reply = json!({
        "choices": [{"finish_reason": "tool_calls", "message": {
            "role": "assistant", "content": null,
            "tool_calls": [{"id": "abc", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\": \"a.txt\"}"}}]
        }}],
    });
    let (url, server) = serve_once(200, reply.to_string().into_bytes());
    let tool = json!({"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}});
    let reply = chat(&url, SERVER_MODEL, &request(vec![tool], None), 5).unwrap();
    assert_eq!(reply.text, "");
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(
        reply.tool_calls,
        vec![ToolCall {
            id: "abc".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.txt"}),
            malformed_arguments: false,
        }]
    );
    assert_eq!(reply.prompt_tokens, None);
    let sent = body_of(&server.join().unwrap());
    assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
    assert!(sent.get("seed").is_none());
}

#[test]
fn malformed_and_missing_fields_are_tolerated() {
    let body = json!({
        "choices": [{"message": {"content": "", "tool_calls": [
            {"function": {"name": "a", "arguments": "{not json"}},
            {"function": {"name": "b", "arguments": "[1, 2]"}},
            {"id": "dup", "function": {"name": "c", "arguments": {"x": 1}}},
            {"id": "dup", "function": {"name": "d"}},
            {"function": {"name": "e", "arguments": ""}},
            {"function": {"arguments": 7}}
        ]}}]
    });
    let reply = parse_reply(body.to_string().as_bytes()).unwrap();
    let calls = &reply.tool_calls;
    assert_eq!(calls[0].id, "call_0");
    assert!(calls[0].malformed_arguments && calls[0].arguments.is_null());
    assert!(
        calls[1].malformed_arguments,
        "non-object arguments are malformed"
    );
    assert_eq!(
        (calls[2].id.as_str(), calls[2].arguments.clone()),
        ("dup", json!({"x": 1}))
    );
    assert_eq!(calls[3].id, "call_3", "repeated ids are replaced");
    assert!(!calls[3].malformed_arguments && calls[3].arguments == json!({}));
    assert!(!calls[4].malformed_arguments && calls[4].arguments == json!({}));
    assert_eq!(calls[5].name, "");
    assert!(calls[5].malformed_arguments);
    assert_eq!(reply.finish_reason, "");
}

#[test]
fn malformed_arguments_round_trip_over_http() {
    let reply = json!({"choices": [{"finish_reason": "tool_calls", "message": {"content": null, "tool_calls": [
        {"id": "t1", "function": {"name": "bash", "arguments": "{\"cmd\": "}}]}}]});
    let (url, _server) = serve_once(200, reply.to_string().into_bytes());
    let reply = chat(&url, SERVER_MODEL, &request(vec![], None), 5).unwrap();
    assert!(reply.tool_calls[0].malformed_arguments);
    // History replay must stay valid for the server's json.loads.
    let message = assistant_message(&reply);
    assert_eq!(message["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(message["content"], Value::Null);
}

#[test]
fn http_error_status_and_bounded_body() {
    let (url, server) = serve_once(500, "x".repeat(100_000).into_bytes());
    let error = chat(&url, SERVER_MODEL, &request(vec![], None), 5)
        .unwrap_err()
        .to_string();
    server.join().unwrap();
    assert!(error.contains("HTTP 500"), "{error}");
    assert!(
        error.len() < 700,
        "error not bounded: {} bytes",
        error.len()
    );
    assert!(error.ends_with('…'));
    assert!(!error.contains("SECRET-PROMPT"));
}

#[test]
fn template_rejection_is_not_reported_as_a_missing_model() {
    for (status, message, missing) in [
        (404, "System message must be at the beginning.", false),
        (400, "System message must be at the beginning.", false),
        (404, "Model not found", true),
    ] {
        let body = json!({"error": {"message": message}}).to_string();
        let (url, server) = serve_once(status, body.into_bytes());
        let error = chat(&url, SERVER_MODEL, &request(vec![], None), 5)
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(error.contains(message), "{error}");
        assert_eq!(error.contains("refresh /models"), missing, "{error}");
        assert_eq!(error.contains("chat template"), !missing, "{error}");
    }
}

#[test]
fn oversized_success_response_is_rejected() {
    let filler = "y".repeat(MAX_RESPONSE_BYTES + 10);
    let body = json!({"choices": [{"message": {"content": filler}}]}).to_string();
    let (url, _server) = serve_once(200, body.into_bytes());
    let error = chat(&url, SERVER_MODEL, &request(vec![], None), 10)
        .unwrap_err()
        .to_string();
    assert!(error.contains("exceeds"), "{error}");
}

#[test]
fn unreachable_server_error_has_no_prompt() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let error = chat(
        &format!("http://127.0.0.1:{port}"),
        SERVER_MODEL,
        &request(vec![], None),
        2,
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("SECRET-PROMPT"), "{error}");
}

#[test]
fn server_dropped_tool_call_is_detectable() {
    let body = json!({"choices": [{"finish_reason": "tool_calls", "message": {"role": "assistant", "content": null}}]});
    let reply = parse_reply(body.to_string().as_bytes()).unwrap();
    assert!(reply.dropped_tool_call());
    let text = json!({"choices": [{"finish_reason": "stop", "message": {"content": "done"}}]});
    assert!(
        !parse_reply(text.to_string().as_bytes())
            .unwrap()
            .dropped_tool_call()
    );
}

#[test]
fn history_helpers_match_openai_shape() {
    let reply = Reply {
        text: "thinking".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "ls".into(),
            arguments: json!({"dir": "."}),
            malformed_arguments: false,
        }],
        finish_reason: "tool_calls".into(),
        prompt_tokens: None,
        completion_tokens: None,
        ..Default::default()
    };
    let message = assistant_message(&reply);
    assert_eq!(message["content"], "thinking");
    assert_eq!(
        message["tool_calls"][0]["function"]["arguments"],
        "{\"dir\":\".\"}"
    );
    assert_eq!(message["tool_calls"][0]["type"], "function");
    assert_eq!(
        tool_message("c1", "out"),
        json!({"role": "tool", "tool_call_id": "c1", "content": "out"})
    );
    let plain = assistant_message(&Reply {
        tool_calls: vec![],
        text: String::new(),
        ..reply
    });
    assert_eq!(plain, json!({"role": "assistant", "content": ""}));
}
