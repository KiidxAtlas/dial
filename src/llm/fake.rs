//! Scripted HTTP server for tests: each script serves exactly one connection.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) enum Script {
    /// Complete JSON response with `Content-Length`.
    Http {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
    },
    HttpHeaders {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
    /// `200 text/event-stream`, close-delimited, written in one go.
    Sse(String),
    /// Declared body length exceeds the bytes sent, producing Peer disconnected.
    SseDisconnect(String),
    /// Valid SSE body with no media type header, as subscription routing can return.
    SseNoContentType(String),
    /// Like [`Script::Sse`] but each piece is flushed with a pause between.
    SseSlow {
        pieces: Vec<String>,
        pause: Duration,
    },
    /// Read the request, then close without answering.
    Hangup,
    /// SSE head, then a keep-alive comment every `pause` for `count` rounds.
    Drip { pause: Duration, count: usize },
    /// SSE head, then silence for `silence` before closing.
    Stall { silence: Duration },
    /// SSE head, one real event, then a keep-alive comment every `pause` for `count` rounds.
    DripAfter {
        first: String,
        pause: Duration,
        count: usize,
    },
    /// SSE head, one real event, then silence for `silence` before closing.
    StallAfter { first: String, silence: Duration },
    /// SSE head at once (as a server does before it has finished reading a long prompt), silence for `delay`, then `body`.
    ColdStart { delay: Duration, body: String },
}

pub(crate) struct Fake {
    pub url: String,
    handle: parking_lot::Mutex<Option<JoinHandle<Vec<Vec<u8>>>>>,
    seen: parking_lot::Mutex<Option<Vec<Vec<u8>>>>,
}

impl Fake {
    /// Raw requests received, in order. Blocks until every script ran; repeat calls return the
    /// same requests.
    pub(crate) fn requests(&self) -> Vec<Vec<u8>> {
        let mut seen = self.seen.lock();
        if let Some(requests) = seen.as_ref() {
            return requests.clone();
        }
        let handle = self.handle.lock().take();
        let requests = handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        *seen = Some(requests.clone());
        requests
    }
}

pub(crate) fn json(status: u16, body: &serde_json::Value) -> Script {
    Script::Http {
        status,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    }
}

pub(crate) fn serve(scripts: Vec<Script>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut responders = Vec::new();
        for script in scripts {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            seen.push(read_request(&mut stream));
            // Each connection gets its own responder: a stalled script must not queue the
            // client's retry behind it, or the retry's own deadline races the stall.
            responders.push(std::thread::spawn(move || respond(&mut stream, script)));
        }
        for responder in responders {
            responder.join().expect("fake responder");
        }
        seen
    });
    Fake {
        url,
        handle: parking_lot::Mutex::new(Some(handle)),
        seen: parking_lot::Mutex::new(None),
    }
}

pub(crate) fn header(raw: &[u8], name: &str) -> Option<String> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

pub(crate) fn request_line(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn body(raw: &[u8]) -> serde_json::Value {
    let at = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(0, |p| p + 4);
    serde_json::from_slice(&raw[at..]).unwrap_or(serde_json::Value::Null)
}

fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let Ok(count) = stream.read(&mut chunk) else {
            return request;
        };
        if count == 0 {
            return request;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let length: usize = header(&request, "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while request.len() < header_end + length {
        let Ok(count) = stream.read(&mut chunk) else {
            break;
        };
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
    }
    request
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";

fn respond(stream: &mut std::net::TcpStream, script: Script) {
    // Clients may hang up early (oversized or interrupted streams); ignore it.
    match script {
        Script::Http {
            status,
            content_type,
            body,
        } => {
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
        Script::HttpHeaders {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
        Script::Sse(body) => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
        Script::SseDisconnect(body) => {
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() + 1
            );
            let _ = stream.write_all(body.as_bytes());
        }
        Script::SseNoContentType(body) => {
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
        Script::SseSlow { pieces, pause } => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            for piece in pieces {
                if stream
                    .write_all(piece.as_bytes())
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(pause);
            }
        }
        Script::Hangup => {}
        Script::Drip { pause, count } => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            for _ in 0..count {
                if stream
                    .write_all(b": keepalive\n")
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(pause);
            }
        }
        Script::Stall { silence } => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(silence);
        }
        Script::DripAfter {
            first,
            pause,
            count,
        } => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            let _ = stream.write_all(first.as_bytes());
            for _ in 0..count {
                if stream
                    .write_all(b": keepalive\n")
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(pause);
            }
        }
        Script::StallAfter { first, silence } => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            let _ = stream.write_all(first.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(silence);
        }
        Script::ColdStart { delay, body } => {
            let _ = stream.write_all(SSE_HEAD.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(delay);
            let _ = stream.write_all(body.as_bytes());
        }
    }
    let _ = stream.flush();
}
