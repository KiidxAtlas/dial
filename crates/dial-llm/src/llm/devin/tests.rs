use super::*;
use crate::llm::connect::MAX_FRAME_PAYLOAD;
use crate::llm::model_config::ApiProtocol;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn serve(bodies: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for body in bodies {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/connect+proto\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (format!("http://{addr}"), handle)
}

fn read_request(stream: &mut TcpStream) {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return;
        }
        data.extend_from_slice(&buf[..n]);
        let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
        let len = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if data.len() >= end + 4 + len {
            return;
        }
    }
}

fn route<'a>(base_url: &'a str, model: &'a str) -> Route<'a> {
    Route {
        protocol: ApiProtocol::DevinAgent,
        base_url,
        model,
        auth: Some("session-token"),
        account_id: None,
        headers: &[],
        project_id: None,
        allow_insecure: true,
        session_id: Some("cascade-1"),
        request_id: None,
    }
}

#[test]
fn metadata_fields_match_the_cli_identity() {
    let meta = metadata("devin-session-token$abc", "jwt-1");
    let fields = Fields::parse(&meta.0).unwrap();
    assert_eq!(fields.string(1), "devin-cli");
    assert_eq!(fields.string(3), "devin-session-token$abc");
    assert_eq!(fields.string(21), "jwt-1");
    assert_eq!(fields.string(28), "chisel");
    assert_eq!(fields.string(12), "chisel");
}

#[test]
fn session_token_gets_the_scheme_prefix_once() {
    assert_eq!(session_token("abc"), "devin-session-token$abc");
    assert_eq!(
        session_token("devin-session-token$abc"),
        "devin-session-token$abc"
    );
}

#[test]
fn frames_reassemble_across_chunks_and_round_trip_gzip() {
    let payload = gzip(b"hello connect").unwrap();
    let wire = frame(FLAG_COMPRESSED, &payload).unwrap();
    let mut frames = Frames::default();
    let mut decoded = Vec::new();
    for chunk in wire.chunks(3) {
        decoded.extend(frames.push(chunk).unwrap());
    }
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].0, FLAG_COMPRESSED);
    assert_eq!(gunzip(&decoded[0].1).unwrap(), b"hello connect");
}

#[test]
fn oversized_frame_is_rejected_before_buffering() {
    let mut header = vec![0u8];
    header.extend_from_slice(&((MAX_FRAME_PAYLOAD as u32) + 1).to_be_bytes());
    assert!(Frames::default().push(&header).is_err());
}

#[test]
fn trailer_error_reports_code_message_and_details() {
    let text = r#"{"error":{"code":"invalid_argument","message":"internal error","details":[{"type":"x.Detail","debug":{"a":1}}]}}"#;
    let error = trailer_error(text).unwrap().formatted;
    assert_eq!(
        error,
        r#"Devin stream error invalid_argument: internal error [details: x.Detail: {"a":1}]"#
    );
    assert!(trailer_error("{}").is_none());
    assert!(trailer_error("not json").is_none());
}

#[test]
fn deterministic_uuid_is_stable_and_well_formed() {
    let a = deterministic_uuid("cascade\x001\0user");
    let b = deterministic_uuid("cascade\x001\0user");
    assert_eq!(a, b);
    assert_eq!(a.len(), 36);
    assert_eq!(a.matches('-').count(), 4);
    assert_ne!(a, deterministic_uuid("cascade\x002\0user"));
}

#[test]
fn gemini_schema_collapses_type_arrays() {
    let schema = json!({"type":"object","properties":{"n":{"type":["number","null"]}}});
    let out = gemini_schema(&schema);
    assert_eq!(out["properties"]["n"]["type"], "number");
    assert_eq!(out["properties"]["n"]["nullable"], true);
    assert_eq!(out["type"], "object");
}

#[test]
fn gemini_schema_strips_keywords_and_spills_them_to_description() {
    let schema = json!({
        "type":"object",
        "description":"Target",
        "additionalProperties":false,
        "properties":{
            "format":{"type":"string","minLength":1,"const":"json"},
            "limit":{"type":"integer","maximum":10}
        }
    });
    let out = gemini_schema(&schema);
    assert!(out.get("additionalProperties").is_none());
    assert_eq!(
        out["description"],
        "Target\n\n{additionalProperties: false}"
    );
    assert!(
        out["properties"].get("format").is_some(),
        "property names stay verbatim"
    );
    assert!(out["properties"]["format"].get("minLength").is_none());
    assert_eq!(out["properties"]["format"]["enum"], json!(["json"]));
    assert_eq!(out["properties"]["limit"]["description"], "{maximum: 10}");
}

#[test]
fn stream_decodes_credit_fields_onto_reply() {
    let mut message = Enc::default();
    message.string(1, "m1");
    message.uint(14, 3);
    message.uint(18, 2);
    message.double(22, 0.25);
    let mut stream = Stream::default();
    stream.feed(0, &message.0).unwrap();
    let reply = stream.finish("scope");
    assert_eq!(
        reply.credits,
        Some(crate::llm::ReplyCredits {
            cost: Some(3),
            committed_cost: Some(2),
            acu_cost: Some(0.25),
        })
    );
}

#[test]
fn chat_request_encodes_history_tools_and_routing() {
    let messages = vec![
        json!({"role":"system","content":"be terse"}),
        json!({"role":"user","content":[
            {"type":"text","text":"look"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}
        ]}),
        json!({"role":"assistant","content":null,"tool_calls":[
            {"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}
        ]}),
        json!({"role":"tool","tool_call_id":"c1","content":"file body"}),
    ];
    let chat = Chat {
        messages,
        tools: vec![
            json!({"type":"function","function":{"name":"read","description":"Read","parameters":{"type":"object"}}}),
        ],
        temperature: 0.4,
        max_tokens: 1000,
        seed: None,
    };
    let session = Session {
        api_key: "devin-session-token$k".to_owned(),
        user_jwt: "jwt".to_owned(),
        base: "http://x".to_owned(),
    };
    let encoded = chat_request(
        "swe-1-6",
        &chat,
        &session,
        "cascade-1",
        "scope",
        "swe-1-6",
        None,
    )
    .unwrap();
    let fields = Fields::parse(&encoded.0).unwrap();
    assert_eq!(fields.string(2), "be terse");
    assert_eq!(fields.uint(7), REQUEST_TYPE_CASCADE);
    assert_eq!(fields.string(16), "cascade-1");
    assert_eq!(fields.string(21), "swe-1-6");
    assert!(fields.bytes(26).is_none());
    let prompts = fields.messages(3).unwrap();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[0].uint(2), SOURCE_USER);
    assert_eq!(prompts[0].messages(10).unwrap()[0].string(2), "image/png");
    assert_eq!(prompts[1].uint(2), SOURCE_SYSTEM);
    assert_eq!(prompts[1].messages(6).unwrap()[0].string(2), "read");
    assert_eq!(prompts[2].uint(2), SOURCE_TOOL);
    assert_eq!(prompts[2].string(7), "c1");
    let tools = fields.messages(10).unwrap();
    assert_eq!(tools[0].string(1), "read");
    assert_eq!(tools[0].string(3), r#"{"type":"object"}"#);
}

#[test]
fn exchange_authenticates_and_streams_text_and_tool_calls() {
    let mut jwt = Enc::default();
    jwt.string(1, "jwt-1");
    let auth_body = jwt.0;

    let mut frames = Vec::new();
    let mut first = Enc::default();
    first.string(1, "m1");
    first.string(3, "Hel");
    frames.extend(frame(FLAG_COMPRESSED, &gzip(&first.0).unwrap()).unwrap());
    let mut second = Enc::default();
    second.string(3, "lo");
    frames.extend(frame(0, &second.0).unwrap());
    let mut call = Enc::default();
    call.string(1, "c1");
    call.string(2, "read");
    call.string(3, r#"{"path":"#);
    let mut third = Enc::default();
    third.message(6, &call);
    frames.extend(frame(0, &third.0).unwrap());
    let mut fourth = Enc::default();
    let mut tail = Enc::default();
    tail.string(1, "c1");
    tail.string(3, r#""a"}"#);
    fourth.message(6, &tail);
    let mut usage = Enc::default();
    usage.uint(2, 11);
    usage.uint(3, 4);
    fourth.message(7, &usage);
    frames.extend(frame(0, &fourth.0).unwrap());
    frames.extend(frame(FLAG_END_STREAM, b"{}").unwrap());

    let (base, server) = serve(vec![auth_body, frames]);
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: Vec::new(),
        temperature: 0.4,
        max_tokens: 100,
        seed: None,
    };
    let mut events = Vec::new();
    let mut text = String::new();
    let mut on_text_fn = |piece: &str| {
        text.push_str(piece);
        true
    };
    let mut on_text_slot: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut on_text_fn);
    let reply = exchange(
        &route(&base, "swe-1-6"),
        &chat,
        5,
        &mut on_text_slot,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |event| {
            events.push(event.clone());
            true
        },
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(text, "Hello");
    assert_eq!(reply.text, "Hello");
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "c1");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a"}));
    assert!(!reply.tool_calls[0].malformed_arguments);
    assert_eq!(reply.prompt_tokens, Some(11));
    assert_eq!(reply.completion_tokens, Some(4));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolArguments { name, .. } if name == "read"
    )));
}

#[test]
fn exchange_surfaces_trailer_errors() {
    let mut jwt = Enc::default();
    jwt.string(1, "jwt-1");
    let trailer = br#"{"error":{"code":"invalid_argument","message":"bad"}}"#;
    let frames = frame(FLAG_END_STREAM, trailer).unwrap();
    let (base, server) = serve(vec![jwt.0, frames]);
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: Vec::new(),
        temperature: 0.4,
        max_tokens: 100,
        seed: None,
    };
    let error = exchange(
        &route(&base, "swe-1-6"),
        &chat,
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap_err();
    server.join().unwrap();
    assert!(
        error
            .to_string()
            .contains("Devin stream error invalid_argument: bad")
    );
}

#[test]
fn large_history_invalid_argument_is_context_overflow() {
    let internal = "Devin stream error invalid_argument: internal error";
    let big = json!({"role": "user", "content": "x".repeat(LARGE_HISTORY_RECOVERY_BYTES)});
    let now = json!({"role": "user", "content": "continue"});
    let overflow = large_history_overflow(
        anyhow::anyhow!(internal),
        &[big, now.clone()],
        "cascade",
        "scope",
    );
    assert!(crate::llm::errors::is_context_overflow(&overflow));
    let small = json!({"role": "user", "content": "hi"});
    let kept = large_history_overflow(anyhow::anyhow!(internal), &[small, now], "cascade", "scope");
    assert!(!crate::llm::errors::is_context_overflow(&kept));
}

/// Serve the auth call, then a chat reply whose frames arrive `pause` apart. With `stall`, the
/// connection then goes silent for `stall` instead of ending the stream.
fn serve_paced(
    frames: Vec<Vec<u8>>,
    pause: Duration,
    stall: Option<Duration>,
) -> (String, std::thread::JoinHandle<()>) {
    let mut jwt = Enc::default();
    jwt.string(1, "jwt-1");
    let auth_body = jwt.0;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/connect+proto\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            auth_body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&auth_body).unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/connect+proto\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
        for frame in frames {
            // The client may give up mid-stream; later writes then fail harmlessly.
            if stream.write_all(&frame).is_err() || stream.flush().is_err() {
                return;
            }
            std::thread::sleep(pause);
        }
        if let Some(stall) = stall {
            std::thread::sleep(stall);
        }
    });
    (format!("http://{addr}"), handle)
}

fn text_frame(text: &str) -> Vec<u8> {
    let mut message = Enc::default();
    message.string(1, "m1");
    message.string(3, text);
    frame(0, &message.0).unwrap()
}

fn paced_exchange(base: &str, idle_limit: Duration) -> (Result<Reply>, std::time::Duration) {
    crate::llm::cancel::Patience::with_test_idle_limit(idle_limit);
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    let started = std::time::Instant::now();
    let result = exchange(
        &route(base, "swe-1-6"),
        &chat,
        1,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    );
    (result, started.elapsed())
}

#[test]
fn a_reply_that_keeps_producing_frames_outlives_the_idle_limit() {
    // 14 frames 150 ms apart: longer than the 1.5 s first-event budget and 1 s idle limit.
    // Leave scheduler headroom for real socket I/O on busy CI runners. The separate
    // silence test retains its tight 300 ms limit and checks the stalled-stream error.
    let mut frames: Vec<Vec<u8>> = (0..14).map(|_| text_frame("x")).collect();
    frames.push(frame(FLAG_END_STREAM, b"{}").unwrap());
    let (base, server) = serve_paced(frames, Duration::from_millis(150), None);
    let (result, elapsed) = paced_exchange(&base, Duration::from_secs(1));
    server.join().unwrap();
    assert_eq!(result.unwrap().text, "x".repeat(14));
    assert!(elapsed > Duration::from_millis(1800), "{elapsed:?}");
}

#[test]
fn silence_after_a_frame_is_a_transient_stall_not_an_unbounded_wait() {
    let (base, server) = serve_paced(
        vec![text_frame("x")],
        Duration::from_millis(10),
        Some(Duration::from_secs(4)),
    );
    let (result, elapsed) = paced_exchange(&base, Duration::from_millis(300));
    let error = result.unwrap_err();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert!(
        error
            .downcast_ref::<crate::llm::stream::StalledStream>()
            .is_some(),
        "{error:#}"
    );
    assert_eq!(
        crate::llm::errors::classify(&error),
        crate::llm::errors::ErrorClass::Transient
    );
    server.join().unwrap();
}

#[test]
fn clean_eof_discards_incomplete_text_tools_and_frames() {
    let mut call = Enc::default();
    call.string(1, "c1");
    call.string(2, "read");
    call.string(3, "{\"path\":");
    let mut message = Enc::default();
    message.message(6, &call);
    let mut stopped = Enc::default();
    stopped.uint(5, 1);
    let mut partial_trailer = text_frame("partial");
    partial_trailer.extend_from_slice(&[FLAG_END_STREAM, 0, 0]);
    let mut complete_then_partial = frame(FLAG_END_STREAM, b"{}").unwrap();
    complete_then_partial.extend_from_slice(&[0, 0]);
    for body in [
        Vec::new(),
        text_frame("partial"),
        frame(0, &message.0).unwrap(),
        frame(0, &stopped.0).unwrap(),
        partial_trailer,
        complete_then_partial,
    ] {
        let mut jwt = Enc::default();
        jwt.string(1, "jwt-1");
        let (base, server) = serve(vec![jwt.0, body]);
        let (result, _) = paced_exchange(&base, Duration::from_millis(300));
        let error = result.unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::stream::IncompleteStream>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(
            super::super::errors::classify(&error),
            ErrorClass::Transient
        );
        server.join().unwrap();
    }
}

#[test]
fn completion_trailer_must_be_valid_and_final() {
    for invalid in [b"".as_slice(), b"not json", b"[]", b"{\"error\":{}}"] {
        assert!(Stream::default().feed(FLAG_END_STREAM, invalid).is_err());
    }
    let mut stream = Stream::default();
    stream
        .feed(FLAG_END_STREAM | FLAG_COMPRESSED, &gzip(b"{}").unwrap())
        .unwrap();
    assert!(stream.ended);
    assert!(stream.feed(0, &[]).is_err());
}

#[test]
fn devin_counts_decoded_frames_and_rejects_gzip_expansion() {
    let mut stream = Stream {
        decoded_bytes: super::super::MAX_STREAM_BYTES,
        ..Default::default()
    };
    assert!(stream.feed(FLAG_COMPRESSED, &gzip(b"x").unwrap()).is_err());
    let packed = gzip(&vec![0; MAX_FRAME_PAYLOAD + 1]).unwrap();
    assert!(Stream::default().feed(FLAG_COMPRESSED, &packed).is_err());
}
