use super::*;
use crate::llm::model_config::ApiProtocol;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

fn chat(messages: Vec<Value>) -> Chat {
    Chat {
        messages,
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 0,
        seed: None,
    }
}

fn update(field: u32, inner: &Enc) -> Vec<u8> {
    let mut update = Enc(Vec::new());
    update.message(field, inner);
    client_message(SERVER_INTERACTION_UPDATE, &update)
}

fn text_delta(field: u32, text: &str) -> Vec<u8> {
    let mut delta = Enc(Vec::new());
    delta.string(1, text);
    update(field, &delta)
}

fn turn_ended(input: u64, output: u64) -> Vec<u8> {
    let mut ended = Enc(Vec::new());
    ended.uint(1, input);
    ended.uint(2, output);
    ended.uint(3, 4);
    ended.uint(4, 2);
    ended.uint(5, 1);
    update(14, &ended)
}

fn mcp_exec(id: u64, exec_id: &str, call_id: &str, tool: &str, args: &[(&str, &str)]) -> Vec<u8> {
    let mut mcp = Enc(Vec::new());
    mcp.string(1, tool);
    for (key, value) in args {
        let mut entry = Enc(Vec::new());
        entry.string(1, key);
        entry.bytes(2, value.as_bytes());
        mcp.message(2, &entry);
    }
    mcp.string(3, call_id);
    mcp.string(5, tool);
    let mut exec = Enc(Vec::new());
    exec.uint(1, id);
    exec.string(15, exec_id);
    exec.message(11, &mcp);
    client_message(SERVER_EXEC, &exec)
}

fn kv_server(id: u64, field: u32, key: &[u8], data: Option<&[u8]>) -> Vec<u8> {
    let mut args = Enc(Vec::new());
    args.bytes(1, key);
    if let Some(data) = data {
        args.bytes(2, data);
    }
    let mut kv = Enc(Vec::new());
    kv.uint(1, id);
    kv.message(field, &args);
    client_message(SERVER_KV, &kv)
}

#[test]
fn trailing_user_message_becomes_the_turn_action() {
    let bytes = run_request(
        &chat(vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hi"}),
        ]),
        "model-x",
        "conv-1",
    )
    .unwrap();
    let client = Fields::parse(&bytes).unwrap();
    let run = client.message(CLIENT_RUN_REQUEST).unwrap().unwrap();
    let state = run.message(1).unwrap().unwrap();
    let roots: Vec<&[u8]> = state.bytes_all(1).collect();
    assert_eq!(
        roots.len(),
        1,
        "trailing user message is not repeated in the root"
    );
    let system: Value = serde_json::from_slice(roots[0]).unwrap();
    assert_eq!(system["role"], "system");
    let action = run.message(2).unwrap().unwrap();
    let user = action
        .message(1)
        .unwrap()
        .unwrap()
        .message(1)
        .unwrap()
        .unwrap();
    assert_eq!(user.string(1), "hi");
    assert_eq!(run.string(5), "conv-1");
}

#[test]
fn trailing_tool_result_resumes_with_every_message_in_the_root() {
    let bytes = run_request(
        &chat(vec![
            json!({"role": "user", "content": "read it"}),
            json!({"role": "assistant", "content": null, "tool_calls": []}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "body"}),
        ]),
        "model-x",
        "conv-1",
    )
    .unwrap();
    let client = Fields::parse(&bytes).unwrap();
    let run = client.message(CLIENT_RUN_REQUEST).unwrap().unwrap();
    let state = run.message(1).unwrap().unwrap();
    let roots: Vec<Value> = state
        .bytes_all(1)
        .map(|bytes| serde_json::from_slice::<Value>(bytes).unwrap())
        .collect();
    assert_eq!(roots.len(), 3);
    assert_eq!(roots[2]["role"], "tool");
    let action = run.message(2).unwrap().unwrap();
    assert!(action.message(1).unwrap().is_none());
    assert!(action.message(2).unwrap().is_some());
}

#[test]
fn runs_that_cannot_be_resumed_or_carry_images_are_rejected() {
    assert!(
        run_request(
            &chat(vec![json!({"role": "assistant", "content": "done"})]),
            "m",
            "c"
        )
        .is_err()
    );
    assert!(
        run_request(
            &chat(vec![json!({"role": "user", "content": "   "})]),
            "m",
            "c"
        )
        .is_err()
    );
    let image = chat(vec![
        json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}),
    ]);
    let error = run_request(&image, "m", "c").unwrap_err();
    assert!(error.to_string().contains("image_url"));
}

#[test]
fn tool_definitions_carry_schema_and_dial_provider_identifier() {
    let tools = [
        json!({"type": "function", "function": {"name": "read", "description": "Read a file", "parameters": {"type": "object"}}}),
        json!({"type": "function", "function": {"name": ""}}),
    ];
    let defs = tool_definitions(&tools).unwrap();
    assert_eq!(defs.len(), 1, "unnamed tools are skipped");
    let def = Fields::parse(&defs[0]).unwrap();
    assert_eq!(def.string(1), "read");
    assert_eq!(def.string(4), "dial");
    assert_eq!(def.string(5), "read");
    let schema: Value = serde_json::from_slice(def.bytes(3).unwrap()).unwrap();
    assert_eq!(schema, json!({"type": "object"}));
}

#[test]
fn mcp_argument_values_decode_json_and_nested_json_text() {
    assert_eq!(decode_arg(b"3"), json!(3));
    assert_eq!(decode_arg(br#""{\"a\":1}""#), json!({"a": 1}));
    assert_eq!(decode_arg(b"not json"), json!("not json"));
}

#[test]
fn mcp_call_becomes_a_tool_call_answered_with_a_handoff() {
    let sent = RefCell::new(Vec::new());
    let send = |bytes: Vec<u8>| {
        sent.borrow_mut().push(bytes);
        Ok(())
    };
    let mut session = Session::new(&send, Vec::new());
    session
        .feed(&mcp_exec(
            7,
            "e1",
            "call-1",
            "read",
            &[("path", "\"a.txt\""), ("limit", "3")],
        ))
        .unwrap();
    assert_eq!(session.tool_calls.len(), 1);
    let call = &session.tool_calls[0];
    assert_eq!(call.id, "call-1");
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments, json!({"path": "a.txt", "limit": 3}));
    let sent = sent.borrow();
    assert_eq!(sent.len(), 1);
    let reply = Fields::parse(&sent[0]).unwrap();
    let exec = reply.message(CLIENT_EXEC).unwrap().unwrap();
    assert_eq!(exec.uint(1), 7);
    assert_eq!(exec.string(15), "e1");
    assert!(exec.message(11).unwrap().is_some());
}

#[test]
fn unsupported_exec_is_thrown_back_and_the_stream_closed() {
    let sent = RefCell::new(Vec::new());
    let send = |bytes: Vec<u8>| {
        sent.borrow_mut().push(bytes);
        Ok(())
    };
    let mut session = Session::new(&send, Vec::new());
    let mut exec = Enc(Vec::new());
    exec.uint(1, 4);
    session.feed(&client_message(SERVER_EXEC, &exec)).unwrap();
    let sent = sent.borrow();
    assert_eq!(sent.len(), 2);
    let control = Fields::parse(&sent[0]).unwrap();
    let throw = control
        .message(CLIENT_CONTROL)
        .unwrap()
        .unwrap()
        .message(2)
        .unwrap()
        .unwrap();
    assert_eq!(throw.uint(1), 4);
    assert_eq!(throw.string(4), "exec_variant_unsupported");
    let close = Fields::parse(&sent[1]).unwrap();
    let stream_close = close
        .message(CLIENT_CONTROL)
        .unwrap()
        .unwrap()
        .message(1)
        .unwrap()
        .unwrap();
    assert_eq!(stream_close.uint(1), 4);
}

#[test]
fn kv_blobs_round_trip_within_a_turn() {
    let sent = RefCell::new(Vec::new());
    let send = |bytes: Vec<u8>| {
        sent.borrow_mut().push(bytes);
        Ok(())
    };
    let mut session = Session::new(&send, Vec::new());
    session.feed(&kv_server(2, 3, b"k", Some(b"v"))).unwrap();
    session.feed(&kv_server(3, 2, b"k", None)).unwrap();
    session.feed(&kv_server(4, 2, b"missing", None)).unwrap();
    let sent = sent.borrow();
    assert_eq!(sent.len(), 3);
    let set = Fields::parse(&sent[0])
        .unwrap()
        .message(CLIENT_KV)
        .unwrap()
        .unwrap();
    assert_eq!(set.uint(1), 2);
    assert!(set.message(3).unwrap().is_some());
    let got = Fields::parse(&sent[1])
        .unwrap()
        .message(CLIENT_KV)
        .unwrap()
        .unwrap();
    assert_eq!(got.message(2).unwrap().unwrap().bytes(1), Some(&b"v"[..]));
    let missing = Fields::parse(&sent[2])
        .unwrap()
        .message(CLIENT_KV)
        .unwrap()
        .unwrap();
    assert_eq!(missing.message(2).unwrap().unwrap().bytes(1), None);
}

#[test]
fn text_reasoning_and_turn_usage_map_to_reply_fields() {
    let send = |_: Vec<u8>| Ok(());
    let mut session = Session::new(&send, Vec::new());
    let mut thinking = Enc(Vec::new());
    thinking.string(1, "hmm");
    let first = session.feed(&text_delta(1, "Hel")).unwrap();
    assert!(matches!(first.as_slice(), [Emit::Text(text)] if text == "Hel"));
    let second = session.feed(&update(4, &thinking)).unwrap();
    assert!(
        matches!(second.as_slice(), [Emit::Event(StreamEvent::Reasoning(text))] if text == "hmm")
    );
    session.feed(&text_delta(1, "lo")).unwrap();
    session.feed(&turn_ended(10, 5)).unwrap();
    let reply = session.finish();
    assert_eq!(reply.text, "Hello");
    assert_eq!(reply.reasoning, "hmm");
    assert_eq!(reply.finish_reason, "stop");
    assert_eq!(reply.prompt_tokens, Some(10));
    assert_eq!(reply.completion_tokens, Some(5));
    assert_eq!(reply.cached_prompt_tokens, Some(4));
    assert_eq!(reply.cache_creation_tokens, Some(2));
    assert_eq!(reply.reasoning_tokens, Some(1));
}

#[test]
fn connect_error_codes_map_to_dial_error_classes() {
    let class = |code: &str| connect_failure(code, "x").map(|c| c.class);
    assert!(matches!(class("unavailable"), Some(ErrorClass::Transient)));
    assert!(matches!(
        class("deadline_exceeded"),
        Some(ErrorClass::Transient)
    ));
    assert!(matches!(
        class("resource_exhausted"),
        Some(ErrorClass::RateLimited { retry_after: None })
    ));
    assert!(matches!(class("permission_denied"), Some(ErrorClass::Auth)));
    assert!(class("invalid_argument").is_none());
}

#[test]
fn trailer_errors_are_classified_and_malformed_trailers_fail() {
    trailer_error(b"").unwrap();
    trailer_error(b"{}").unwrap();
    let error = trailer_error(br#"{"error":{"code":"aborted","message":"stop"}}"#).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Classified>().unwrap().class,
        ErrorClass::Transient
    ));
    let error =
        trailer_error(br#"{"error":{"code":"resource_exhausted","message":"quota"}}"#).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Classified>().unwrap().class,
        ErrorClass::RateLimited { .. }
    ));
    let error =
        trailer_error(br#"{"error":{"code":"invalid_argument","message":"bad"}}"#).unwrap_err();
    assert!(error.downcast_ref::<Classified>().is_none());
    assert!(error.to_string().contains("bad"));
    assert!(trailer_error(b"not json").is_err());
}

/// Reads one HTTP/1.1 request and returns its path and body.
fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let path = line.split_whitespace().nth(1).unwrap().to_owned();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (path, body)
}

/// Fake Cursor server: BidiAppend bodies are reported on the returned channel. RunSSE waits
/// for the first append (so the run request is proven uploaded) and then streams `frames`.
fn spawn_fake_server(frames: Vec<u8>) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (appends_tx, appends_rx) = mpsc::channel::<Vec<u8>>();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let ready_rx = Arc::new(Mutex::new(ready_rx));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let appends_tx = appends_tx.clone();
            let ready_tx = ready_tx.clone();
            let ready_rx = Arc::clone(&ready_rx);
            let frames = frames.clone();
            thread::spawn(move || {
                let (path, body) = read_request(&mut stream);
                let response = if path.ends_with("BidiAppend") {
                    let _ = appends_tx.send(body);
                    let _ = ready_tx.send(());
                    Vec::new()
                } else {
                    let _ = ready_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5));
                    frames
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/connect+proto\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&response);
            });
        }
    });
    (format!("http://{addr}"), appends_rx)
}

#[test]
fn fake_server_receives_the_run_request_and_streams_the_reply() {
    let mut frames = frame(0, &text_delta(1, "hello")).unwrap();
    frames.extend(frame(0, &turn_ended(7, 3)).unwrap());
    frames.extend(frame(FLAG_END_STREAM, b"{}").unwrap());
    let (base, appends) = spawn_fake_server(frames);
    let route = Route {
        protocol: ApiProtocol::Cursor,
        base_url: &base,
        model: "model-x",
        auth: Some("test-token"),
        account_id: None,
        headers: &[],
        project_id: None,
        allow_insecure: true,
        session_id: None,
        request_id: None,
    };
    let reply = exchange(
        &route,
        &chat(vec![json!({"role": "user", "content": "hi"})]),
        30,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap();
    assert_eq!(reply.text, "hello");
    assert_eq!(reply.finish_reason, "stop");
    assert_eq!(reply.prompt_tokens, Some(7));
    let first = appends.recv_timeout(Duration::from_secs(5)).unwrap();
    let envelope = Fields::parse(&first).unwrap();
    assert_eq!(envelope.uint(3), 0);
    let data = envelope.bytes(4).unwrap();
    assert!(
        Fields::parse(data)
            .unwrap()
            .message(CLIENT_RUN_REQUEST)
            .unwrap()
            .is_some()
    );
}
