use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;

use serde_json::{Value, json};
use tungstenite::Message;

use super::transport::{self, Access, Api, Project};
use super::wire::{self, Status};
use super::{Budget, Restart, SESSIONS, Session, Step, drive, forget, session_key, unseen};

/// Accept `responses.len()` HTTP/1.1 requests on loopback, answering each with
/// the paired status and JSON body. Returns the request lines seen.
fn serve_http(responses: Vec<(u16, String)>) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let handle = thread::spawn(move || {
        let mut lines = Vec::new();
        for (status, body) in responses {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).expect("request line");
            lines.push(request.trim_end().to_owned());
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                if header == "\r\n" || header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut sink = vec![0u8; length];
            reader.read_exact(&mut sink).expect("request body");
            let mut stream = reader.into_inner();
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("write");
        }
        lines
    });
    (port, handle)
}

/// Accept one WebSocket connection per script on loopback. Each connection reads
/// the start request, sends its scripted events, then waits until the client
/// closes or replies. Returns the start requests seen.
fn serve_ws(scripts: Vec<Vec<Value>>) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let handle = thread::spawn(move || {
        let mut starts = Vec::new();
        for events in scripts {
            let (stream, _) = listener.accept().expect("accept");
            let Ok(mut ws) = tungstenite::accept(stream) else {
                continue;
            };
            let start = ws
                .read()
                .ok()
                .and_then(|message| message.into_text().ok())
                .map(|text| text.to_string())
                .unwrap_or_default();
            starts.push(start);
            for event in events {
                if ws.send(Message::Text(event.to_string().into())).is_err() {
                    break;
                }
            }
            let _ = ws.read();
        }
        starts
    });
    (port, handle)
}

/// Open a workflow socket against a loopback WebSocket fixture and send start.
fn connect_session(port: u16, interrupt: &Arc<AtomicBool>) -> Session {
    let base = format!("http://127.0.0.1:{port}");
    let api = Api {
        base: &base,
        token: "pat",
        interrupt,
        timeout: 5,
    };
    let access = Access {
        token: "dat".to_owned(),
        headers: Vec::new(),
    };
    let project = Project {
        id: "42".to_owned(),
        namespace_id: "7".to_owned(),
        root_namespace_id: transport::root_namespace("7"),
    };
    let mut socket = transport::connect(&api, &access, &project, "model-x").expect("connect");
    transport::send(&mut socket, &json!({"start": true})).expect("start");
    Session {
        workflow_id: "99".to_owned(),
        socket,
        pending: None,
        base,
        token: "pat".to_owned(),
    }
}

#[test]
fn status_maps_terminal_and_approval_states() {
    assert_eq!(
        wire::status_of(&json!({"status": "FINISHED"})),
        Status::Complete
    );
    assert_eq!(
        wire::status_of(&json!({"status": "INPUT_REQUIRED"})),
        Status::Complete
    );
    assert_eq!(
        wire::status_of(&json!({"status": "TOOL_CALL_APPROVAL_REQUIRED"})),
        Status::Approval
    );
    assert_eq!(
        wire::status_of(&json!({"status": "FAILED", "error": "boom"})),
        Status::Failed("boom".to_owned())
    );
    assert_eq!(
        wire::status_of(&json!({"status": "EXECUTION"})),
        Status::Running
    );
    assert_eq!(wire::status_of(&json!({})), Status::Running);
}

#[test]
fn restart_classifiers_match_known_messages() {
    assert!(wire::is_step_limit("Agent reached its maximum step limit"));
    assert!(wire::is_generic_error(
        "There was an error processing your request in the Duo Agent Platform"
    ));
    assert!(!wire::is_step_limit("network down"));
}

#[test]
fn action_requires_request_id_and_strips_tool_prefix() {
    let missing = json!({"action": {"name": "runMCPTool", "args": {}}});
    assert!(wire::action_of(&missing).is_err());

    let event = json!({"action": {
        "name": "runMCPTool",
        "requestID": "r1",
        "args": {"toolName": "mcp__omp__read", "args": {"path": "a.txt"}}
    }});
    let action = wire::action_of(&event).expect("valid").expect("is action");
    assert_eq!(action.request_id, "r1");
    assert_eq!(action.name, "read");
    assert_eq!(action.arguments, json!({"path": "a.txt"}));

    assert_eq!(
        wire::action_of(&json!({"status": "EXECUTION"})).expect("ok"),
        None
    );
}

#[test]
fn action_response_uses_server_shape() {
    assert_eq!(
        wire::action_response("r9", "done"),
        json!({"actionResponse": {"requestID": "r9", "plainTextResponse": {"response": "done"}}})
    );
}

#[test]
fn budget_caps_restarts() {
    let mut budget = Budget::default();
    for _ in 0..4 {
        budget.spend(Restart::StepLimit).expect("within limit");
    }
    assert!(budget.spend(Restart::StepLimit).is_err());
    budget
        .spend(Restart::Generic("x".into()))
        .expect("first retry");
    assert!(budget.spend(Restart::Generic("x".into())).is_err());
}

#[test]
fn unseen_yields_only_text_past_the_streamed_prefix() {
    assert_eq!(unseen("", "Hel"), "Hel");
    assert_eq!(unseen("Hel", "Hello"), "lo");
    assert_eq!(unseen("Hello", "Hel"), "");
    assert_eq!(unseen("Hello", "Hello"), "");
}

#[test]
fn rest_discovers_project_and_creates_workflow() {
    let (port, server) = serve_http(vec![
        (200, json!([{"id": 42, "namespace": {"id": 7}}]).to_string()),
        (
            200,
            json!({"token": "dat", "duo_workflow_service": {"headers": {"x-a": "1"}}}).to_string(),
        ),
        (201, json!({"id": 99}).to_string()),
        (200, "{}".to_owned()),
    ]);
    let base = format!("http://127.0.0.1:{port}");
    let interrupt = Arc::new(AtomicBool::new(false));
    let api = Api {
        base: &base,
        token: "pat",
        interrupt: &interrupt,
        timeout: 5,
    };
    let project = api.project_for(None).expect("project");
    assert_eq!(project.id, "42");
    assert_eq!(project.namespace_id, "7");
    assert_eq!(project.root_namespace_id, "gid://gitlab/Group/7");
    let access = api
        .direct_access(&project.root_namespace_id, &project.id)
        .expect("access");
    assert_eq!(access.token, "dat");
    assert_eq!(access.headers, vec![("x-a".to_owned(), "1".to_owned())]);
    assert_eq!(
        api.create_workflow(&project.id, "goal").expect("create"),
        "99"
    );
    api.stop_workflow("99");

    let lines = server.join().expect("server");
    assert!(lines[0].starts_with("GET /api/v4/projects?membership=true"));
    assert!(lines[1].starts_with("POST /api/v4/ai/duo_workflows/direct_access"));
    assert!(lines[2].starts_with("POST /api/v4/ai/duo_workflows/workflows"));
    assert!(lines[3].starts_with("PATCH /api/v4/ai/duo_workflows/workflows/99"));
}

#[test]
fn explicit_nested_project_resolves_root_group_by_full_path() {
    let (port, server) = serve_http(vec![
        (
            200,
            json!({"id": 42, "namespace": {"id": 9, "full_path": "group/sub"}}).to_string(),
        ),
        (200, json!({"id": 5}).to_string()),
    ]);
    let base = format!("http://127.0.0.1:{port}");
    let interrupt = Arc::new(AtomicBool::new(false));
    let api = Api {
        base: &base,
        token: "pat",
        interrupt: &interrupt,
        timeout: 5,
    };
    let project = api.project_for(Some("group/sub/proj")).expect("project");
    assert_eq!(project.id, "42");
    assert_eq!(project.namespace_id, "9");
    assert_eq!(project.root_namespace_id, "gid://gitlab/Group/5");

    let lines = server.join().expect("server");
    assert!(lines[0].starts_with("GET /api/v4/projects/group%2Fsub%2Fproj "));
    assert!(lines[1].starts_with("GET /api/v4/groups/group "));
}

#[test]
fn rest_http_error_surfaces_without_token() {
    let (port, server) = serve_http(vec![(
        401,
        json!({"message": "401 Unauthorized"}).to_string(),
    )]);
    let base = format!("http://127.0.0.1:{port}");
    let interrupt = Arc::new(AtomicBool::new(false));
    let api = Api {
        base: &base,
        token: "pat-secret-value",
        interrupt: &interrupt,
        timeout: 5,
    };
    let error = api.project_for(None).expect_err("401 must fail");
    assert!(!format!("{error:#}").contains("pat-secret-value"));
    server.join().expect("server");
}

#[test]
fn websocket_streams_text_and_round_trips_tool_result() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = thread::spawn(move || -> String {
        let (stream, _) = listener.accept().expect("accept");
        let mut ws = tungstenite::accept(stream).expect("ws accept");
        let start = ws
            .read()
            .expect("start")
            .into_text()
            .expect("text")
            .to_string();
        for event in [
            json!({"newCheckpoint": {"message": "Hel"}}),
            json!({"newCheckpoint": {"message": "Hello"}}),
            json!({"action": {
                "name": "runMCPTool",
                "requestID": "r1",
                "args": {"toolName": "mcp__omp__read", "args": {"path": "a"}}
            }}),
        ] {
            ws.send(Message::Text(event.to_string().into()))
                .expect("send");
        }
        let reply = ws
            .read()
            .expect("reply")
            .into_text()
            .expect("text")
            .to_string();
        ws.send(Message::Text(
            json!({"status": "FINISHED"}).to_string().into(),
        ))
        .expect("finish");
        format!("{start}\n{reply}")
    });

    let interrupt = Arc::new(AtomicBool::new(false));
    let mut session = connect_session(port, &interrupt);
    let mut shown = String::new();

    let mut deltas = Vec::new();
    let step = {
        let mut callback = |delta: &str| {
            deltas.push(delta.to_owned());
            true
        };
        let mut slot: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut callback);
        drive(&mut session, &interrupt, &mut slot, &mut shown).expect("drive")
    };
    assert_eq!(deltas, vec!["Hel".to_owned(), "lo".to_owned()]);
    let Step::Reply(reply) = step else {
        panic!("expected a tool-call reply");
    };
    assert_eq!(reply.text, "Hello");
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "r1");
    assert_eq!(reply.tool_calls[0].name, "read");
    assert_eq!(session.pending.as_deref(), Some("r1"));

    transport::send(
        &mut session.socket,
        &wire::action_response("r1", "file body"),
    )
    .expect("send tool result");
    let mut slot: Option<&mut dyn FnMut(&str) -> bool> = None;
    let finished = drive(&mut session, &interrupt, &mut slot, &mut shown).expect("drive to finish");
    assert!(matches!(&finished, Step::Reply(done) if done.tool_calls.is_empty()));

    let received = server.join().expect("server");
    assert!(received.contains("\"start\":true"));
    assert!(received.contains("\"requestID\":\"r1\""));
    assert!(received.contains("file body"));
}

#[test]
fn approval_with_pending_action_surfaces_as_tool_call() {
    let (port, _server) = serve_ws(vec![vec![json!({
        "status": "TOOL_CALL_APPROVAL_REQUIRED",
        "action": {
            "name": "runMCPTool",
            "requestID": "r7",
            "args": {"toolName": "mcp__omp__write", "args": {"path": "a"}}
        }
    })]]);
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut session = connect_session(port, &interrupt);
    let mut shown = String::new();
    let mut slot: Option<&mut dyn FnMut(&str) -> bool> = None;
    let step = drive(&mut session, &interrupt, &mut slot, &mut shown).expect("drive");
    let Step::Reply(reply) = step else {
        panic!("expected a tool-call reply");
    };
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls[0].id, "r7");
    assert_eq!(reply.tool_calls[0].name, "write");
    assert_eq!(session.pending.as_deref(), Some("r7"));
}

#[test]
fn approval_without_tool_action_is_an_error() {
    let (port, _server) = serve_ws(vec![vec![json!({"status": "TOOL_CALL_APPROVAL_REQUIRED"})]]);
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut session = connect_session(port, &interrupt);
    let mut shown = String::new();
    let mut slot: Option<&mut dyn FnMut(&str) -> bool> = None;
    let Err(error) = drive(&mut session, &interrupt, &mut slot, &mut shown) else {
        panic!("approval without an action must fail");
    };
    assert!(format!("{error:#}").contains("approval"));
}

#[test]
fn restart_does_not_repeat_streamed_text() {
    let (port, _server) = serve_ws(vec![
        vec![
            json!({"newCheckpoint": {"message": "Hel"}}),
            json!({"status": "FAILED", "error": "Agent reached its maximum step limit"}),
        ],
        vec![
            json!({"newCheckpoint": {"message": "Hello world"}}),
            json!({"status": "FINISHED"}),
        ],
    ]);
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut deltas: Vec<String> = Vec::new();
    let mut shown = String::new();

    let mut first = connect_session(port, &interrupt);
    let first_step = {
        let mut callback = |delta: &str| {
            deltas.push(delta.to_owned());
            true
        };
        let mut slot: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut callback);
        drive(&mut first, &interrupt, &mut slot, &mut shown).expect("first attempt")
    };
    assert!(matches!(first_step, Step::Restart(Restart::StepLimit)));
    drop(first);

    let mut second = connect_session(port, &interrupt);
    let second_step = {
        let mut callback = |delta: &str| {
            deltas.push(delta.to_owned());
            true
        };
        let mut slot: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut callback);
        drive(&mut second, &interrupt, &mut slot, &mut shown).expect("second attempt")
    };
    let Step::Reply(reply) = second_step else {
        panic!("expected completion after restart");
    };
    assert_eq!(reply.text, "Hello world");
    assert_eq!(reply.finish_reason, "stop");
    assert_eq!(deltas, vec!["Hel".to_owned(), "lo world".to_owned()]);
}

#[test]
fn forget_stops_the_workflow_before_dropping_its_socket() {
    let (ws_port, _ws) = serve_ws(vec![vec![]]);
    let interrupt = Arc::new(AtomicBool::new(false));
    let (http_port, http) = serve_http(vec![(200, "{}".to_owned())]);
    let mut session = connect_session(ws_port, &interrupt);
    session.base = format!("http://127.0.0.1:{http_port}");
    let key = session_key(&session.base, "forget-sid", "model-x");
    SESSIONS
        .lock()
        .expect("sessions lock")
        .insert(key.clone(), session);

    forget("forget-sid");

    assert!(!SESSIONS.lock().expect("sessions lock").contains_key(&key));
    let lines = http.join().expect("server");
    assert!(lines[0].starts_with("PATCH /api/v4/ai/duo_workflows/workflows/99 "));
}
