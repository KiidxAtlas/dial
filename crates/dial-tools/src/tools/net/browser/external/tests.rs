use super::*;

fn track_cmux(session: &mut Session, surface: &str) {
    match &mut session.backend {
        Backend::Cmux { tabs, .. } => tabs.push(Tab {
            tab: json!(surface),
            handle: surface.to_owned(),
        }),
        Backend::Tern { .. } => panic!("not a cmux session"),
    }
}

fn track_tern(session: &mut Session, block: u64) {
    match &mut session.backend {
        Backend::Tern { tabs, .. } => tabs.push(Tab {
            tab: json!(block),
            handle: block,
        }),
        Backend::Cmux { .. } => panic!("not a tern session"),
    }
}

fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    }
}

#[test]
fn cmux_is_selected_from_socket_env_and_respects_setting() {
    let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")]);
    let config = BrowserConfig::default();
    assert_eq!(select(&config, &env).map(|s| s.label()), Some("cmux"));
    let disabled = BrowserConfig {
        cmux: false,
        tern: false,
        ..BrowserConfig::default()
    };
    assert!(select(&disabled, &env).is_none());
}

#[test]
fn no_backend_without_environment() {
    let env = env_with(&[]);
    assert!(select(&BrowserConfig::default(), &env).is_none());
}

#[test]
fn actions_require_an_open_surface_first() {
    let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux-unreachable.sock")]);
    let mut session = select(&BrowserConfig::default(), &env).unwrap();
    let error = session
        .run("click", &json!({"selector": "#a"}))
        .unwrap_err()
        .into_message();
    assert!(error.contains("run open first"), "{error}");
}

#[test]
fn unsupported_action_is_refused_by_name() {
    let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux-unreachable.sock")]);
    let mut session = select(&BrowserConfig::default(), &env).unwrap();
    track_cmux(&mut session, "surface:1");
    let error = session
        .run("trace_start", &json!({}))
        .unwrap_err()
        .into_message();
    assert!(
        error.contains("not supported by the cmux backend"),
        "{error}"
    );
}

#[test]
fn missing_tern_daemon_is_unavailable_so_open_can_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.sock");
    let mut session = tern_session(path.to_str().unwrap());
    let error = session
        .run("open", &json!({"url": "about:blank"}))
        .unwrap_err();
    assert!(matches!(error, ExternalError::Unavailable(_)), "{error:?}");
}

#[cfg(unix)]
fn read_frame(stream: &mut std::os::unix::net::UnixStream) -> Value {
    use std::io::Read;
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).expect("frame header");
    let mut body = vec![0u8; u32::from_le_bytes(header) as usize];
    stream.read_exact(&mut body).expect("frame body");
    serde_json::from_slice(&body).expect("frame json")
}

#[cfg(unix)]
fn write_frame(stream: &mut std::os::unix::net::UnixStream, message: &Value) {
    use std::io::Write;
    let body = serde_json::to_vec(message).expect("json");
    stream
        .write_all(&(body.len() as u32).to_le_bytes())
        .expect("header");
    stream.write_all(&body).expect("body");
}

fn tern_session(path: &str) -> Session {
    let path = path.to_owned();
    let env = move |key: &str| match key {
        "TERN_PANE_SOCKET" => Some(path.clone()),
        "TERN_PANE" => Some("1".to_owned()),
        _ => None,
    };
    let config = BrowserConfig {
        cmux: false,
        tern: true,
        ..BrowserConfig::default()
    };
    select(&config, env).expect("tern backend")
}

#[cfg(unix)]
#[test]
fn tern_nav_events_and_fork_round_trip_when_advertised() {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tern.sock").to_string_lossy().into_owned();
    let listener = UnixListener::bind(&path).expect("bind");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        assert_eq!(read_frame(&mut stream), json!({ "hello": {} }));
        write_frame(
            &mut stream,
            &json!({ "welcome": { "ops": ["browser", "nav", "events", "fork"] } }),
        );

        let nav = read_frame(&mut stream);
        assert_eq!(nav["browser"]["op"], "nav");
        assert_eq!(nav["browser"]["block"], 7);
        assert_eq!(nav["browser"]["go"], "back");
        let id = nav["id"].clone();
        write_frame(&mut stream, &json!({ "id": id, "browser": { "ok": null } }));

        let events = read_frame(&mut stream);
        assert_eq!(events["browser"]["op"], "events");
        assert_eq!(events["browser"]["after"], 5);
        let id = events["id"].clone();
        write_frame(
            &mut stream,
            &json!({ "id": id, "browser": { "ok": { "events": [{ "seq": 6 }] } } }),
        );

        let fork = read_frame(&mut stream);
        assert_eq!(fork["fork"]["block"], 1);
        assert_eq!(fork["fork"]["dir"], "right");
        let id = fork["id"].clone();
        write_frame(
            &mut stream,
            &json!({ "id": id, "fork": { "ok": { "block": 30 } } }),
        );
    });

    let mut session = tern_session(&path);
    track_tern(&mut session, 7);
    let back = session.run("back", &json!({})).expect("back");
    assert_eq!(back, json!({ "tab": 7, "action": "back" }));
    let events = session
        .run("events", &json!({ "after": 5 }))
        .expect("events");
    assert_eq!(events["events"], json!({ "events": [{ "seq": 6 }] }));
    let forked = session
        .run("fork", &json!({ "dir": "right" }))
        .expect("fork");
    assert_eq!(forked, json!({ "backend": "tern", "block": 30 }));
    assert!(
        session
            .run("fork", &json!({ "dir": "diagonal" }))
            .unwrap_err()
            .into_message()
            .contains("right or down")
    );
    server.join().expect("server");
}

#[cfg(unix)]
#[test]
fn tern_unadvertised_ops_are_refused_before_sending() {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tern.sock").to_string_lossy().into_owned();
    let listener = UnixListener::bind(&path).expect("bind");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        assert_eq!(read_frame(&mut stream), json!({ "hello": {} }));
        write_frame(&mut stream, &json!({ "welcome": { "ops": ["browser"] } }));
        // Dial closes its tracked tab on drop: read that frame, then hang up so the drop
        // sees EOF instead of waiting out a request timeout.
        let _ = read_frame(&mut stream);
    });

    let mut session = tern_session(&path);
    track_tern(&mut session, 7);
    let error = session
        .run("reload", &json!({}))
        .unwrap_err()
        .into_message();
    assert!(
        error.contains("does not advertise the \"nav\" op"),
        "{error}"
    );
    let error = session.run("fork", &json!({})).unwrap_err().into_message();
    assert!(
        error.contains("does not advertise the \"fork\" op"),
        "{error}"
    );
    drop(session);
    server.join().expect("server");
}
