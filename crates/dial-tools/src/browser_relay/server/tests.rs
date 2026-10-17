use std::net::{Ipv4Addr, SocketAddr};

use serde_json::json;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;

use super::*;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const WAIT: Duration = Duration::from_secs(5);

/// Starts a server on an ephemeral loopback port and returns its shared state.
fn start() -> (Arc<Shared>, u16) {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::new(Shared {
        relay: Arc::new(Relay::new()),
        token: TOKEN.to_owned(),
        port,
    });
    let serving = Arc::clone(&shared);
    thread::spawn(move || serve(listener, serving));
    (shared, port)
}

/// Sends a raw HTTP request and returns (status line, body).
fn http(port: u16, request: &str) -> (String, String) {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    (
        head.lines().next().unwrap_or("").to_owned(),
        body.to_owned(),
    )
}

fn get(port: u16, path: &str, extra_headers: &str) -> (String, String) {
    http(
        port,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}Connection: close\r\n\r\n"
        ),
    )
}

fn authed(port: u16, path: &str) -> (String, String) {
    get(port, path, &format!("{TOKEN_HEADER}: {TOKEN}\r\n"))
}

/// Opens a WebSocket to `path` on the relay. Returns the HTTP status on refusal.
fn ws_connect(port: u16, path: &str, origin: Option<&str>) -> Result<WebSocket<TcpStream>, u16> {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut request = format!("ws://127.0.0.1:{port}{path}")
        .into_client_request()
        .unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
    }
    tungstenite::client(request, stream)
        .map(|(ws, _)| ws)
        .map_err(|error| match error {
            HandshakeError::Failure(tungstenite::Error::Http(response)) => {
                response.status().as_u16()
            }
            _ => 0,
        })
}

fn read_json(ws: &mut WebSocket<TcpStream>) -> Value {
    match ws.read().expect("frame") {
        Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
        other => panic!("expected text frame, got {other:?}"),
    }
}

fn hello_frame() -> String {
    json!({
        "t": "hello",
        "dialProtocol": PROTOCOL_VERSION,
        "extensionVersion": "1.0.0",
        "product": "Chrome/137.0.1",
        "userAgent": "Mozilla/5.0 Chrome/137.0.1",
        "tabs": [{"tabId": 1, "targetId": "CDP-TARGET-1", "url": "https://a.test/", "title": "A", "discarded": false}],
    })
    .to_string()
}

#[test]
fn requests_without_the_token_are_unauthorized() {
    let (_shared, port) = start();
    let (status, body) = get(port, "/json/version", "");
    assert!(status.contains("401"), "{status}");
    assert!(body.contains("token"), "{body}");
}

#[test]
fn a_wrong_token_in_the_header_or_query_is_unauthorized() {
    let (_shared, port) = start();
    let (status, _) = get(port, "/json/version", &format!("{TOKEN_HEADER}: nope\r\n"));
    assert!(status.contains("401"), "{status}");
    let (status, _) = get(port, "/cdp?token=nope", "");
    assert!(status.contains("401"), "{status}");
}

#[test]
fn discovery_is_503_with_an_actionable_message_until_the_extension_connects() {
    let (_shared, port) = start();
    let (status, body) = authed(port, "/json/version");
    assert!(status.contains("503"), "{status}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    let error = parsed["error"].as_str().unwrap();
    assert!(error.contains("dial browser-relay install"), "{error}");
    assert_eq!(parsed["dialRelayProtocol"], PROTOCOL_VERSION);
    assert_eq!(parsed["extensionSeen"], false);
    let (status, _) = authed(port, "/json/list");
    assert!(status.contains("503"), "{status}");
}

#[test]
fn cdp_websocket_is_503_over_plain_http_until_the_extension_connects() {
    let (_shared, port) = start();
    let (status, _) = get(port, &format!("/cdp?token={TOKEN}"), "");
    assert!(status.contains("503"), "{status}");
}

#[test]
fn cdp_clients_sending_an_origin_are_refused() {
    let (_shared, port) = start();
    let (status, _) = get(
        port,
        &format!("/cdp?token={TOKEN}"),
        "Origin: https://evil.test\r\n",
    );
    assert!(status.contains("403"), "{status}");
}

#[test]
fn only_extension_origins_may_open_the_extension_socket() {
    let (_shared, port) = start();
    match ws_connect(port, &format!("/ext?token={TOKEN}"), None) {
        Err(403) => {}
        other => panic!("expected 403, got {other:?}"),
    }
    match ws_connect(
        port,
        &format!("/ext?token={TOKEN}"),
        Some("https://evil.test"),
    ) {
        Err(403) => {}
        other => panic!("expected 403, got {other:?}"),
    }
}

#[test]
fn extension_hello_makes_discovery_return_200_with_a_token_bearing_ws_url() {
    let (shared, port) = start();
    let mut ext = ws_connect(
        port,
        &format!("/ext?token={TOKEN}"),
        Some("chrome-extension://abcdefghijklmnop"),
    )
    .expect("extension socket");
    ext.send(Message::Text(hello_frame().into())).unwrap();
    for _ in 0..100 {
        if shared.relay.snapshot().connected {
            break;
        }
        thread::sleep(POLL);
    }
    let (status, body) = authed(port, "/json/version");
    assert!(status.contains("200"), "{status}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        parsed["webSocketDebuggerUrl"],
        format!("ws://127.0.0.1:{port}/cdp?token={TOKEN}")
    );
    assert_eq!(parsed["Browser"], "Chrome/137.0.1");
    assert_eq!(parsed["dialExtensionVersion"], "1.0.0");
    let (status, body) = authed(port, "/json/list");
    assert!(status.contains("200"), "{status}");
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list[0]["targetId"], "CDP-TARGET-1");
    ext.close(None).unwrap();
}

#[test]
fn a_fake_extension_socket_serves_a_cdp_client_end_to_end() {
    let (shared, port) = start();

    let mut ext = ws_connect(
        port,
        &format!("/ext?token={TOKEN}"),
        Some("chrome-extension://abcdefghijklmnop"),
    )
    .expect("extension socket");
    ext.send(Message::Text(hello_frame().into())).unwrap();

    // The fake extension answers every relay operation the way chrome.debugger would.
    let answering = thread::spawn(move || {
        let mut answered = Vec::new();
        loop {
            let frame = match ext.read() {
                Ok(Message::Text(text)) => serde_json::from_str::<Value>(text.as_str()).unwrap(),
                Ok(_) => continue,
                Err(_) => break,
            };
            if frame["t"] != "rpc" {
                continue;
            }
            let result = match frame["op"].as_str() {
                Some("attach" | "detach") => json!({}),
                Some("send") => json!({"echo": frame["method"]}),
                other => panic!("unexpected op {other:?}"),
            };
            answered.push(frame["op"].as_str().unwrap().to_owned());
            let reply = json!({"t": "rpcResult", "id": frame["id"], "ok": true, "result": result});
            ext.send(Message::Text(reply.to_string().into())).unwrap();
            if answered.len() == 2 {
                // Chrome reports a child frame session on the parent tab.
                let child = json!({
                    "t": "cdpEvent", "tabId": 1, "sessionId": null,
                    "method": "Target.attachedToTarget",
                    "params": {"sessionId": "CHILD-1", "targetInfo": {"type": "iframe"}},
                });
                ext.send(Message::Text(child.to_string().into())).unwrap();
                let event = json!({
                    "t": "cdpEvent", "tabId": 1, "sessionId": "CHILD-1",
                    "method": "Runtime.consoleAPICalled", "params": {"type": "log"},
                });
                ext.send(Message::Text(event.to_string().into())).unwrap();
                // Stay until the client leaves so the sessions are still live.
                ext.get_mut().set_read_timeout(Some(WAIT)).unwrap();
            }
        }
        answered
    });
    // The relay registers the extension when it processes the hello frame, on its own thread.
    for _ in 0..100 {
        if shared.relay.snapshot().connected {
            break;
        }
        thread::sleep(POLL);
    }

    let mut client = ws_connect(port, &format!("/cdp?token={TOKEN}"), None).expect("cdp socket");
    client
        .send(Message::Text(
            json!({"id": 1, "method": "Target.setAutoAttach",
                   "params": {"autoAttach": true, "flatten": true}})
            .to_string()
            .into(),
        ))
        .unwrap();
    // Replies and asynchronous events may be delivered in either order.
    let initial: Vec<_> = (0..2).map(|_| read_json(&mut client)).collect();
    let set = initial
        .iter()
        .find(|frame| frame["id"] == 1)
        .expect("setAutoAttach reply");
    assert_eq!(set["id"], 1);
    assert_eq!(set["result"], json!({}));

    let attached = initial
        .iter()
        .find(|frame| frame["method"] == "Target.attachedToTarget")
        .expect("attachment event");
    assert_eq!(attached["method"], "Target.attachedToTarget");
    let session = attached["params"]["sessionId"].as_str().unwrap().to_owned();

    client
        .send(Message::Text(
            json!({"id": 2, "sessionId": session, "method": "Runtime.evaluate",
                   "params": {"expression": "document.title"}})
            .to_string()
            .into(),
        ))
        .unwrap();
    let evaluated_frames: Vec<_> = (0..3).map(|_| read_json(&mut client)).collect();
    let evaluated = evaluated_frames
        .iter()
        .find(|frame| frame["id"] == 2)
        .expect("evaluation reply");
    assert_eq!(evaluated["id"], 2);
    assert_eq!(evaluated["sessionId"], session.as_str());
    assert_eq!(evaluated["result"]["echo"], "Runtime.evaluate");

    let child_attached = evaluated_frames
        .iter()
        .find(|frame| frame["method"] == "Target.attachedToTarget")
        .expect("child attachment event");
    assert_eq!(child_attached["params"]["sessionId"], "CHILD-1");
    assert_eq!(child_attached["sessionId"], session.as_str());
    let child_event = evaluated_frames
        .iter()
        .find(|frame| frame["method"] == "Runtime.consoleAPICalled")
        .expect("child console event");
    assert_eq!(child_event["sessionId"], "CHILD-1");

    client.close(None).unwrap();
    while client.read().is_ok() {}
    // Client leaving detaches the child frame session first (a `send`), then releases
    // the tab's debugger (`detach`), which the fake extension answers.
    let answered = wait_for_answers(answering);
    assert_eq!(
        answered,
        vec![
            "attach".to_owned(),
            "send".to_owned(),
            "send".to_owned(),
            "detach".to_owned()
        ]
    );
}

fn wait_for_answers(handle: thread::JoinHandle<Vec<String>>) -> Vec<String> {
    handle.join().expect("fake extension thread")
}
