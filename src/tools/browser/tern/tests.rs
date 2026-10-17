#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::thread;

use super::*;

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |key: &str| {
        owned
            .iter()
            .find(|(name, _)| name.as_str() == key)
            .map(|(_, value)| value.clone())
    }
}

#[cfg(unix)]
fn read_frame(stream: &mut UnixStream) -> Value {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).expect("frame header");
    let mut body = vec![0u8; u32::from_le_bytes(header) as usize];
    stream.read_exact(&mut body).expect("frame body");
    serde_json::from_slice(&body).expect("frame json")
}

#[cfg(unix)]
fn write_frame(stream: &mut UnixStream, message: &Value) {
    stream
        .write_all(&encode_frame(message).expect("encode"))
        .expect("write frame");
}

fn socket_in(dir: &tempfile::TempDir) -> String {
    dir.path().join("tern.sock").to_string_lossy().into_owned()
}

#[test]
fn resolve_requires_socket_and_pane_and_respects_flag() {
    let both = [("TERN_PANE_SOCKET", "/tmp/t.sock"), ("TERN_PANE", "42")];
    assert_eq!(
        resolve_tern_kind(&ResolveTernKindOptions::default(), env_of(&both)),
        Some(TernKind {
            socket_path: "/tmp/t.sock".to_owned(),
            pane: 42
        })
    );
    let disabled = ResolveTernKindOptions {
        setting_enabled: Some(false),
    };
    assert!(resolve_tern_kind(&disabled, env_of(&both)).is_none());
    let overridden = [
        ("DIAL_BROWSER_TERN", "1"),
        ("TERN_PANE_SOCKET", "/tmp/t.sock"),
        ("TERN_PANE", "1"),
    ];
    assert!(resolve_tern_kind(&disabled, env_of(&overridden)).is_some());
    assert!(
        resolve_tern_kind(
            &ResolveTernKindOptions::default(),
            env_of(&[("TERN_PANE_SOCKET", "/s"), ("TERN_PANE", "4x")])
        )
        .is_none()
    );
    assert!(
        resolve_tern_kind(
            &ResolveTernKindOptions::default(),
            env_of(&[("TERN_PANE", "1")])
        )
        .is_none()
    );
    assert!(
        resolve_tern_pane(env_of(&[
            ("TERN_PANE_SOCKET", "/s"),
            ("TERN_PANE", "9007199254740992")
        ]))
        .is_none()
    );
}

#[test]
fn frames_are_u32_little_endian_length_prefixed() {
    let frame = encode_frame(&json!({ "hello": {} })).expect("encode");
    let length = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    assert_eq!(length, frame.len() - 4);
    let mut buffered = frame.clone();
    let payload = take_frame(&mut buffered)
        .expect("take")
        .expect("complete frame");
    assert_eq!(
        serde_json::from_slice::<Value>(&payload).expect("json"),
        json!({ "hello": {} })
    );
    assert!(buffered.is_empty());

    let mut partial = frame[..frame.len() - 1].to_vec();
    assert!(take_frame(&mut partial).expect("partial").is_none());

    let mut oversize = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec();
    assert_eq!(
        take_frame(&mut oversize).expect_err("oversize").kind,
        TernErrorKind::Protocol
    );
}

#[test]
fn malformed_answers_are_protocol_errors_and_kinds_map() {
    assert_eq!(
        answer_of("browser x", &json!({ "ok": 3 })).expect("ok"),
        json!(3)
    );
    let malformed = answer_of("browser x", &json!({})).expect_err("malformed");
    assert_eq!(malformed.kind, TernErrorKind::Protocol);
    assert_eq!(
        malformed.message,
        "Tern answered browser x without \"ok\" or \"error\""
    );
    let unknown = answer_of(
        "browser x",
        &json!({ "error": { "kind": "weird", "message": "m" } }),
    )
    .expect_err("error");
    assert_eq!(unknown.kind, TernErrorKind::Failed);
    assert_eq!(unknown.message, "Tern browser x failed (weird): m");
}

#[cfg(unix)]
#[test]
fn hello_welcome_then_browser_requests_round_trip_and_skip_stale_answers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = socket_in(&dir);
    let listener = UnixListener::bind(&path).expect("bind");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        assert_eq!(read_frame(&mut stream), json!({ "hello": {} }));
        write_frame(
            &mut stream,
            &json!({ "welcome": { "ops": ["browser", "fork"] } }),
        );

        let goto = read_frame(&mut stream);
        assert_eq!(goto["browser"]["op"], "goto");
        assert_eq!(goto["browser"]["block"], 7);
        let id = goto["id"].clone();
        write_frame(
            &mut stream,
            &json!({ "id": 999, "browser": { "ok": null } }),
        );
        write_frame(
            &mut stream,
            &json!({ "id": id, "browser": { "ok": { "url": "https://example.test/" } } }),
        );

        let state = read_frame(&mut stream);
        assert_eq!(state["browser"]["op"], "state");
        let id = state["id"].clone();
        write_frame(
            &mut stream,
            &json!({ "id": id, "browser": { "error": { "kind": "no_window", "message": "no pane" } } }),
        );
    });

    let client = TernClient::new(&TernKind {
        socket_path: path,
        pane: 1,
    });
    let value = client.goto(7, "https://example.test/").expect("goto");
    assert_eq!(value["url"], "https://example.test/");
    let error = client.state(7).expect_err("state must fail");
    assert_eq!(error.kind, TernErrorKind::NoWindow);
    assert!(error.is_unavailable());
    assert_eq!(
        error.message,
        "Tern browser state failed (no_window): no pane"
    );
    server.join().expect("server");
}

#[cfg(unix)]
#[test]
fn open_and_fork_return_blocks_and_check_channel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = socket_in(&dir);
    let listener = UnixListener::bind(&path).expect("bind");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        assert_eq!(read_frame(&mut stream), json!({ "hello": {} }));
        write_frame(&mut stream, &json!({ "welcome": { "ops": [] } }));

        let open = read_frame(&mut stream);
        assert_eq!(open["browser"]["op"], "open");
        assert_eq!(open["browser"]["owner"], 1);
        assert_eq!(open["browser"]["width"], 800);
        let id = open["id"].clone();
        write_frame(
            &mut stream,
            &json!({ "id": id, "browser": { "ok": { "block": 12 } } }),
        );

        let fork = read_frame(&mut stream);
        assert_eq!(fork["fork"]["block"], 12);
        assert_eq!(fork["fork"]["dir"], "down");
        let id = fork["id"].clone();
        write_frame(
            &mut stream,
            &json!({ "id": id, "fork": { "ok": { "block": 30 } } }),
        );
    });

    let client = TernClient::new(&TernKind {
        socket_path: path,
        pane: 1,
    });
    assert_eq!(client.open_tab(1, 800, 600).expect("open"), 12);
    assert_eq!(client.fork(12, Some(ForkDir::Down)).expect("fork"), 30);
    server.join().expect("server");
}

#[test]
fn connect_failure_is_unavailable_connect_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TernClient::new(&TernKind {
        socket_path: socket_in(&dir),
        pane: 1,
    });
    let error = client.state(1).expect_err("no daemon");
    assert_eq!(error.kind, TernErrorKind::Connect);
    assert!(error.is_unavailable());
}
