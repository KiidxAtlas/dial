use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::thread;

fn read_line(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read request line");
    line
}

fn client(socket_path: &str, password: Option<&str>) -> CmuxClient {
    CmuxClient::new(CmuxClientConfig {
        socket_path: socket_path.to_owned(),
        password: password.map(str::to_owned),
        relay_id: None,
        relay_token: None,
        home_dir: None,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
    })
}

fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    }
}

#[test]
fn resolve_requires_socket_and_respects_flag() {
    let options = ResolveCmuxKindOptions::default();
    let kind = resolve_cmux_kind(&options, env_of(&[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")]))
        .expect("kind");
    assert_eq!(kind.socket_path, "/tmp/cmux.sock");
    assert_eq!(kind.password, None);
    assert!(resolve_cmux_kind(&options, env_of(&[])).is_none());
    assert!(
        resolve_cmux_kind(
            &options,
            env_of(&[("DIAL_BROWSER_CMUX", "0"), ("CMUX_SOCKET_PATH", "/x")])
        )
        .is_none()
    );
    let disabled_setting = ResolveCmuxKindOptions {
        surface: None,
        setting_enabled: Some(false),
    };
    assert!(resolve_cmux_kind(&disabled_setting, env_of(&[("CMUX_SOCKET_PATH", "/x")])).is_none());
    let empty_password = resolve_cmux_kind(
        &options,
        env_of(&[("CMUX_SOCKET_PATH", "/x"), ("CMUX_SOCKET_PASSWORD", "")]),
    )
    .expect("kind");
    assert_eq!(empty_password.password, None);
}

#[test]
fn parse_flag_truthy_set() {
    assert!(parse_flag(Some("yes"), false));
    assert!(parse_flag(Some("1"), false));
    assert!(!parse_flag(Some("0"), true));
    assert!(parse_flag(None, true));
    assert!(!parse_flag(Some(""), false));
}

#[cfg(unix)]
#[test]
fn request_round_trips_method_params_and_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
    let listener = UnixListener::bind(&path).expect("bind");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let request: Value = serde_json::from_str(&read_line(&mut reader)).expect("json request");
        assert_eq!(request["method"], "browser.url.get");
        assert_eq!(request["params"]["surface_id"], "surf-1");
        assert!(request["id"].as_str().is_some_and(|id| !id.is_empty()));
        writeln!(
            writer,
            r#"{{"id":"ignored","ok":true,"result":{{"url":"https://example.test/"}}}}"#
        )
        .expect("reply");
    });
    let result = client(&path, None)
        .surface_request("surf-1", "browser.url.get", json!({}))
        .expect("result");
    assert_eq!(result["url"], "https://example.test/");
    server.join().expect("server");
}

#[cfg(unix)]
#[test]
fn error_reply_surfaces_as_typed_remote_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
    let listener = UnixListener::bind(&path).expect("bind");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let _ = read_line(&mut reader);
        writeln!(
            writer,
            r#"{{"ok":false,"error":{{"code":"not_found","message":"no surface","details":{{"surface_id":"surf-9"}}}}}}"#
        )
        .expect("reply");
    });
    let error = client(&path, None)
        .surface_request("surf-9", "browser.snapshot", json!({}))
        .expect_err("error");
    match error {
        CmuxError::Remote(remote) => {
            assert_eq!(remote.code, "not_found");
            assert_eq!(remote.message, "no surface");
            assert_eq!(
                remote.to_string(),
                r#"not_found: no surface details={"surface_id":"surf-9"}"#
            );
        }
        other => panic!("expected remote error, got {other:?}"),
    }
    server.join().expect("server");
}

#[cfg(unix)]
#[test]
fn password_is_sent_before_first_request_and_err_lines_are_socket_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
    let listener = UnixListener::bind(&path).expect("bind");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        assert_eq!(read_line(&mut reader).trim_end(), "auth s3cret");
        writeln!(writer, "OK").expect("auth reply");
        let _ = read_line(&mut reader);
        writeln!(writer, "ERROR: surface missing").expect("error reply");
    });
    let error = client(&path, Some("s3cret"))
        .request("surface.close", json!({"surface_id": "gone"}))
        .expect_err("socket error");
    assert!(matches!(&error, CmuxError::Socket(text) if text == "ERROR: surface missing"));
    server.join().expect("server");
}

#[test]
fn relay_tcp_target_completes_hmac_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
    let port = listener.local_addr().expect("addr").port();
    let token = "00ff10";
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        writeln!(
            writer,
            r#"{{"protocol":"cmux-relay-auth","version":1,"relay_id":"relay-1","nonce":"n-42"}}"#
        )
        .expect("challenge");
        let auth: Value = serde_json::from_str(&read_line(&mut reader)).expect("auth json");
        assert_eq!(auth["relay_id"], "relay-1");
        let mac = decode_hex(auth["mac"].as_str().expect("mac")).expect("mac hex");
        let key = hmac::Key::new(hmac::HMAC_SHA256, &decode_hex(token).expect("token"));
        hmac::verify(&key, b"relay_id=relay-1\nnonce=n-42\nversion=1", &mac).expect("valid mac");
        writeln!(writer, r#"{{"ok":true}}"#).expect("auth ok");
        let _ = read_line(&mut reader);
        writeln!(writer, r#"{{"ok":true,"result":{{"surface_id":"s-1"}}}}"#).expect("result");
    });
    let config = CmuxClientConfig {
        socket_path: format!("127.0.0.1:{port}"),
        password: None,
        relay_id: Some("relay-1".to_owned()),
        relay_token: Some(token.to_owned()),
        home_dir: None,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
    };
    let result = CmuxClient::new(config)
        .request("browser.open_split", json!({"url": "about:blank"}))
        .expect("result");
    assert_eq!(result["surface_id"], "s-1");
    server.join().expect("server");
}

#[cfg(unix)]
#[test]
fn open_split_requires_surface_id_in_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
    let listener = UnixListener::bind(&path).expect("bind");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let request: Value = serde_json::from_str(&read_line(&mut reader)).expect("json");
        assert_eq!(request["params"]["focus"], false);
        writeln!(
            writer,
            r#"{{"ok":true,"result":{{"surface_id":"s-7","url":"about:blank"}}}}"#
        )
        .expect("reply");
    });
    let opened = client(&path, None)
        .open_split("about:blank", None, None)
        .expect("open");
    assert_eq!(
        opened,
        OpenSplit {
            surface_id: "s-7".to_owned(),
            url: Some("about:blank".to_owned())
        }
    );
    server.join().expect("server");
}

#[cfg(unix)]
#[test]
fn socket_timeouts_accept_nanoseconds_near_a_second_boundary() {
    for peer_closed in [false, true] {
        for timeout in [
            Duration::from_nanos(1),
            Duration::new(0, 999_999_999),
            Duration::new(4, 999_999_999),
            Duration::from_secs(5),
        ] {
            let (socket, peer) = UnixStream::pair().unwrap();
            let stream = Stream::Unix(socket);
            let _peer = (!peer_closed).then_some(peer);
            stream.set_timeouts(Some(timeout)).expect("socket timeout");
            stream.set_timeouts(None).unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn buffered_reply_survives_peer_closure_before_timeout_update() {
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"final reply\n").unwrap();
    drop(peer);
    let mut connection = Connection {
        stream: Stream::Unix(socket),
        pending: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    assert_eq!(connection.read_line(deadline).unwrap(), "final reply");
    assert!(matches!(
        connection.read_line(deadline),
        Err(CmuxError::Closed)
    ));
}

#[cfg(unix)]
#[test]
fn connected_peer_still_respects_read_deadline() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let mut connection = Connection {
        stream: Stream::Unix(socket),
        pending: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_millis(20);
    assert!(matches!(
        connection.read_line(deadline),
        Err(CmuxError::Timeout)
    ));
}
