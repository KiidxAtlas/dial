//! Scripted HTTP server for tests: each script serves exactly one connection.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long [`Fake::requests`] waits for a connection while scripts remain unused. A client that
/// stopped early (a failed test, an exhausted descriptor table) then fails the test instead of
/// blocking it forever in `accept`.
const ACCEPT_IDLE: Duration = Duration::from_secs(60);

pub enum Script {
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

pub struct Fake {
    pub url: String,
    handle: parking_lot::Mutex<Option<JoinHandle<Vec<Vec<u8>>>>>,
    seen: parking_lot::Mutex<Option<Vec<Vec<u8>>>>,
    accepted: Arc<AtomicUsize>,
    scripts: usize,
    idle: Duration,
}

impl Fake {
    /// Raw requests received, in order. Blocks until every script ran; repeat calls return the
    /// same requests. Panics when scripts remain unused and no connection arrived for the idle
    /// limit, naming how many did.
    pub fn requests(&self) -> Vec<Vec<u8>> {
        let mut seen = self.seen.lock();
        if let Some(requests) = seen.as_ref() {
            return requests.clone();
        }
        let handle = self.handle.lock().take();
        let requests = handle
            .map(|handle| {
                self.await_scripts(&handle);
                handle.join().unwrap_or_default()
            })
            .unwrap_or_default();
        *seen = Some(requests.clone());
        requests
    }

    /// Waits until every script was accepted or the server stopped, resetting the idle clock on
    /// each new connection.
    fn await_scripts(&self, handle: &JoinHandle<Vec<Vec<u8>>>) {
        let mut last = (self.accepted.load(Ordering::Acquire), Instant::now());
        while !handle.is_finished() {
            let accepted = self.accepted.load(Ordering::Acquire);
            if accepted >= self.scripts {
                return;
            }
            if accepted != last.0 {
                last = (accepted, Instant::now());
            } else if last.1.elapsed() >= self.idle {
                panic!(
                    "fake server at {}: {accepted} of {} scripted connections arrived, then none for {:?}",
                    self.url, self.scripts, self.idle
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Parallel tests each hold sockets, databases and child pipes; macOS defaults to 256 descriptors,
/// which a parallel run exceeds. Raise this process's soft limit (never above the hard limit), once.
pub fn raise_fd_limit() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: plain getrlimit/setrlimit calls on a local, fully initialised struct.
            unsafe {
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
                    let target = limit.rlim_max.min(10_240);
                    if limit.rlim_cur < target {
                        limit.rlim_cur = target;
                        libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
                    }
                }
            }
        });
    }
}

pub fn json(status: u16, body: &serde_json::Value) -> Script {
    Script::Http {
        status,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    }
}

/// Drive the real Codex WebSocket transport against a caller-owned loopback server.
/// Keeps transport internals private while higher-layer tests exercise live steering receipts.
pub fn codex_exchange(
    url: &str,
    session_key: &str,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_text: &mut dyn FnMut(&str) -> bool,
) -> anyhow::Result<Option<super::Reply>> {
    let chat = super::Chat {
        messages: vec![serde_json::json!({"role":"user","content":"original"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    super::codex_ws::exchange(
        super::responses::Endpoint {
            url,
            token: "test",
            account_id: None,
            model: "test",
            headers: &[],
        },
        &chat,
        5,
        &mut Some(on_text),
        interrupt,
        None,
        None,
        &mut |_| true,
        session_key,
    )
}

pub fn serve(scripts: Vec<Script>) -> Fake {
    raise_fd_limit();
    serve_on(TcpListener::bind("127.0.0.1:0").expect("bind"), scripts)
}

/// [`serve`] on a listener the caller bound. What earlier servers on this address told the
/// process-wide server caches is forgotten first: tests bind port 0, so a later test can get an
/// earlier test's port, and a stale cache entry would skip a probe its scripts expect.
pub(crate) fn serve_on(listener: TcpListener, scripts: Vec<Script>) -> Fake {
    serve_with_idle(listener, scripts, ACCEPT_IDLE)
}

/// [`serve_on`] with an explicit [`Fake::requests`] idle limit.
pub(crate) fn serve_with_idle(listener: TcpListener, scripts: Vec<Script>, idle: Duration) -> Fake {
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    super::catalog::forget_llama_props(&url);
    super::local_server::forget(&url);
    super::anthropic_cache::forget_refusals(&url);
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = scripts.len();
    let counter = Arc::clone(&accepted);
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut responders = Vec::new();
        for script in scripts {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            seen.push(read_request(&mut stream));
            counter.fetch_add(1, Ordering::Release);
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
        accepted,
        scripts: count,
        idle,
    }
}

pub fn header(raw: &[u8], name: &str) -> Option<String> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

pub fn request_line(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

pub fn body(raw: &[u8]) -> serde_json::Value {
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

#[cfg(test)]
mod tests;
