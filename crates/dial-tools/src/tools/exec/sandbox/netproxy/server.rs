//! The filtering proxy server and the Linux namespace bridge (see the parent module docs).

use super::{BRIDGE_PORT, Decision, NetworkRules, is_public};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Largest request head (request line plus headers) the proxy reads.
const MAX_HEAD: usize = 64 * 1024;
/// Concurrent client connections per listener; more are dropped.
const MAX_CONNECTIONS: usize = 256;
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// After one relay direction ends, how long the other may keep running before both are closed.
const DRAIN_GRACE: Duration = Duration::from_secs(30);
/// Pause after a failed `accept` (for example, out of file descriptors) so the loop cannot spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// Maximum distinct proxy endpoints kept alive in one process before LRU eviction.
const MAX_ENDPOINTS: usize = 16;
/// Short timeout for the local wake connection used to unblock `accept()` during shutdown.
const SHUTDOWN_WAKE_TIMEOUT: Duration = Duration::from_millis(200);

/// A running proxy: a loopback TCP port (macOS clients) and a Unix socket in a private directory
/// (bound into Linux sandboxes). Shuts down its listener threads and removes its socket directory
/// once evicted from the process cache and dropped by every active caller.
#[derive(Debug)]
pub struct Endpoint {
    port: u16,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    tcp_thread: Option<std::thread::JoinHandle<()>>,
    unix_thread: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            SHUTDOWN_WAKE_TIMEOUT,
        );
        let _ = UnixStream::connect(&self.socket);
        if let Some(handle) = self.tcp_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.unix_thread.take() {
            let _ = handle.join();
        }
    }
}
impl Endpoint {
    /// The loopback TCP port in this process's network namespace.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The Unix socket path; it is private to the current user.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The port sandboxed clients use: the bridge port inside a Linux network namespace,
    /// otherwise the proxy port itself.
    pub fn client_port(&self) -> u16 {
        if cfg!(target_os = "linux") {
            BRIDGE_PORT
        } else {
            self.port
        }
    }
}

pub(super) struct EndpointCache {
    max: usize,
    next_seq: u64,
    entries: HashMap<NetworkRules, (u64, Arc<Endpoint>)>,
}

impl EndpointCache {
    pub(super) fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            next_seq: 0,
            entries: HashMap::new(),
        }
    }

    pub(super) fn get_or_start(
        &mut self,
        rules: &NetworkRules,
    ) -> Result<(Arc<Endpoint>, Option<Arc<Endpoint>>), String> {
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        if let Some((last_used, endpoint)) = self.entries.get_mut(rules) {
            *last_used = seq;
            return Ok((endpoint.clone(), None));
        }
        let endpoint = Arc::new(
            start(rules.clone())
                .map_err(|error| format!("starting the sandbox network proxy: {error}"))?,
        );
        let evicted = if self.entries.len() >= self.max {
            // Prefer evicting an endpoint with no active callers outside the cache; otherwise
            // evict the oldest entry (its listeners remain alive until active callers drop it).
            let victim = self
                .entries
                .iter()
                .filter(|(_, (_, ep))| Arc::strong_count(ep) == 1)
                .min_by_key(|(_, (used, _))| *used)
                .or_else(|| self.entries.iter().min_by_key(|(_, (used, _))| *used))
                .map(|(k, _)| k.clone());
            victim.and_then(|key| self.entries.remove(&key).map(|(_, ep)| ep))
        } else {
            None
        };
        self.entries.insert(rules.clone(), (seq, endpoint.clone()));
        Ok((endpoint, evicted))
    }
}

/// The process-wide proxy for `rules`, started on first use and bounded by LRU eviction.
pub fn endpoint(rules: &NetworkRules) -> Result<Arc<Endpoint>, String> {
    static RUNNING: LazyLock<Mutex<EndpointCache>> =
        LazyLock::new(|| Mutex::new(EndpointCache::new(MAX_ENDPOINTS)));
    let (endpoint, evicted) = {
        let mut running = RUNNING.lock().unwrap_or_else(|error| error.into_inner());
        running.get_or_start(rules)?
    };
    // Drop any evicted endpoint outside the cache lock so its shutdown join never blocks callers.
    drop(evicted);
    Ok(endpoint)
}

fn start(rules: NetworkRules) -> io::Result<Endpoint> {
    let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = tcp.local_addr()?.port();
    // A short private directory keeps the socket path under the Unix length limit.
    let dir = tempfile::Builder::new()
        .prefix("dial-net-")
        .tempdir_in("/tmp")?;
    let socket = dir.path().join("proxy.sock");
    let unix = UnixListener::bind(&socket)?;
    let rules = Arc::new(rules);
    let stop = Arc::new(AtomicBool::new(false));
    let tcp_rules = rules.clone();
    let tcp_stop = stop.clone();
    let tcp_thread = std::thread::Builder::new()
        .name("dial-netproxy-tcp".into())
        .spawn(move || {
            serve(
                tcp.incoming(),
                "dial-netproxy-conn",
                Some(tcp_stop),
                move |client| {
                    let _ = handle(client, &tcp_rules);
                },
            )
        })?;
    let unix_stop = stop.clone();
    let unix_thread = match std::thread::Builder::new()
        .name("dial-netproxy-unix".into())
        .spawn(move || {
            serve(
                unix.incoming(),
                "dial-netproxy-conn",
                Some(unix_stop),
                move |client| {
                    let _ = handle(client, &rules);
                },
            )
        }) {
        Ok(handle) => handle,
        Err(error) => {
            stop.store(true, Ordering::Release);
            let _ = TcpStream::connect_timeout(
                &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                SHUTDOWN_WAKE_TIMEOUT,
            );
            let _ = tcp_thread.join();
            return Err(error);
        }
    };
    Ok(Endpoint {
        port,
        socket,
        stop,
        tcp_thread: Some(tcp_thread),
        unix_thread: Some(unix_thread),
        _dir: dir,
    })
}

/// A byte stream the proxy can split into a reader and a writer.
pub(super) trait Stream: Read + Write + Send + Sized + 'static {
    fn split(&self) -> io::Result<Self>;
    fn read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn close(&self, how: Shutdown);
}

impl Stream for TcpStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }
    fn close(&self, how: Shutdown) {
        let _ = self.shutdown(how);
    }
}

impl Stream for UnixStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }
    fn close(&self, how: Shutdown) {
        let _ = self.shutdown(how);
    }
}

/// Decrements the connection count when a connection thread ends.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Run `handler` on its own thread for each accepted client, at most [`MAX_CONNECTIONS`] at a
/// time; clients past the limit are dropped. A failed accept backs off briefly instead of
/// retrying in a tight loop.
fn serve<S: Send + 'static>(
    incoming: impl Iterator<Item = io::Result<S>>,
    name: &str,
    stop: Option<Arc<AtomicBool>>,
    handler: impl Fn(S) + Send + Sync + 'static,
) {
    let handler = Arc::new(handler);
    let active = Arc::new(AtomicUsize::new(0));
    let stopping = || {
        stop.as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    };
    for client in incoming {
        if stopping() {
            break;
        }
        let client = match client {
            Ok(client) => client,
            Err(_) => {
                std::thread::sleep(ACCEPT_BACKOFF);
                if stopping() {
                    break;
                }
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let slot = Slot(active.clone());
        let handler = handler.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _slot = slot;
                handler(client);
            });
        drop(spawned);
    }
}

/// What the client asked for.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) host: String,
    pub(super) port: u16,
    /// `None` for `CONNECT`; otherwise the rewritten origin-form head to send upstream.
    pub(super) forward: Option<Vec<u8>>,
}

/// Serve one proxy client: read and check the request, then relay bytes both ways.
pub(super) fn handle<S: Stream>(mut client: S, rules: &NetworkRules) -> io::Result<()> {
    client.read_timeout(Some(HEAD_TIMEOUT))?;
    let Some((head, rest)) = read_head(&mut client)? else {
        return respond(&mut client, 431, "request head too large");
    };
    let request = match parse(&head) {
        Ok(request) => request,
        Err(message) => return respond(&mut client, 400, message),
    };
    let private_ok = match rules.decide(&request.host) {
        Decision::Deny => {
            return respond(
                &mut client,
                403,
                &format!(
                    "Dial sandbox network policy does not allow {} (sandbox.network_allow / sandbox.network_deny)",
                    request.host
                ),
            );
        }
        Decision::Allow { private_ok } => private_ok,
    };
    let addresses = match (request.host.as_str(), request.port).to_socket_addrs() {
        Ok(addresses) => addresses.collect::<Vec<SocketAddr>>(),
        Err(error) => {
            return respond(
                &mut client,
                502,
                &format!("cannot resolve {}: {error}", request.host),
            );
        }
    };
    if let Some(blocked) = addresses
        .iter()
        .find(|address| !private_ok && !is_public(address.ip()))
    {
        return respond(
            &mut client,
            403,
            &format!(
                "Dial sandbox network policy blocks {} because it resolves to the non-public address {}; list the exact host in sandbox.network_allow to permit it",
                request.host,
                blocked.ip()
            ),
        );
    }
    let Some(mut upstream) = addresses
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, CONNECT_TIMEOUT).ok())
    else {
        return respond(
            &mut client,
            502,
            &format!("cannot connect to {}:{}", request.host, request.port),
        );
    };
    match &request.forward {
        None => client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?,
        Some(head) => upstream.write_all(head)?,
    }
    upstream.write_all(&rest)?;
    client.read_timeout(None)?;
    relay(client, upstream)
}

/// Read up to the end of the request head. Returns the head and any bytes read past it, or
/// `None` when the head exceeds the size limit.
fn read_head(client: &mut impl Read) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = client.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed before the request head ended",
            ));
        }
        let searched_from = buffer.len().saturating_sub(3);
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer[searched_from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let end = searched_from + end + 4;
            let rest = buffer.split_off(end);
            return Ok(Some((buffer, rest)));
        }
        if buffer.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

/// Parse a `CONNECT host:port` or absolute-form `http://` request head.
pub(super) fn parse(head: &[u8]) -> Result<Request, &'static str> {
    let text = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    if has_bare_line_or_ctl(line, false) {
        return Err("control character in request line");
    }
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line");
    };
    if !is_http_token(method) {
        return Err("invalid HTTP method");
    }
    if !version.starts_with("HTTP/1.") {
        return Err("only HTTP/1.x proxy requests are supported");
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, None)?;
        for header in lines.filter(|line| !line.is_empty()) {
            if has_bare_line_or_ctl(header, true) {
                return Err("control character in request header");
            }
        }
        return Ok(Request {
            host,
            port,
            forward: None,
        });
    }
    let Some(rest) = strip_prefix_ignore_case(target, "http://") else {
        return Err("expected CONNECT host:port or an absolute http:// URL");
    };
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_host_port(authority, Some(80))?;
    let path = match path.split('#').next().unwrap_or_default() {
        "" => "/".to_owned(),
        query if query.starts_with('?') => format!("/{query}"),
        path => path.to_owned(),
    };
    // A proxy replaces the client's Host with the authority it checked (RFC 9112 §3.2.2), so a
    // request to an allowed address cannot name another virtual host there.
    let mut forward = format!("{method} {path} {version}\r\nHost: {authority}\r\n");
    for header in lines.filter(|line| !line.is_empty()) {
        if has_bare_line_or_ctl(header, true) {
            return Err("control character in request header");
        }
        let name = header.split(':').next().unwrap_or_default().trim();
        if [
            "host",
            "proxy-connection",
            "proxy-authorization",
            "connection",
            "keep-alive",
        ]
        .iter()
        .any(|skip| name.eq_ignore_ascii_case(skip))
        {
            continue;
        }
        forward.push_str(header);
        forward.push_str("\r\n");
    }
    // One request per connection, so every request passes the host check.
    forward.push_str("Connection: close\r\n\r\n");
    Ok(Request {
        host,
        port,
        forward: Some(forward.into_bytes()),
    })
}

/// Reject bare CR/LF (which enable HTTP request smuggling when only CRLF was split) and other
/// ASCII control bytes. Horizontal tabs are allowed only in header values (RFC 9110 §5.5).
fn has_bare_line_or_ctl(line: &str, allow_tab: bool) -> bool {
    line.bytes().any(|b| match b {
        b'\t' => !allow_tab,
        0x00..=0x1f | 0x7f => true,
        _ => false,
    })
}

/// RFC 9110 §5.6.2 HTTP token characters for request methods.
fn is_http_token(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

fn split_host_port(authority: &str, default: Option<u16>) -> Result<(String, u16), &'static str> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("unterminated IPv6 address")?;
        let port = if after.is_empty() {
            None
        } else if let Some(port) = after.strip_prefix(':') {
            Some(port)
        } else {
            return Err("invalid IPv6 authority");
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, Some(port)),
            _ => (authority, None),
        }
    };
    let port = match (port, default) {
        (Some(port), _) => port.parse::<u16>().ok().filter(|port| *port > 0),
        (None, default) => default,
    }
    .ok_or("missing or invalid port")?;
    if host.is_empty() {
        return Err("missing host");
    }
    Ok((host.to_owned(), port))
}

fn respond(client: &mut impl Write, status: u16, message: &str) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        431 => "Request Header Fields Too Large",
        _ => "Bad Gateway",
    };
    let body = format!("{message}\n");
    write!(
        client,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nX-Dial-Sandbox: network-policy\r\n\r\n{body}",
        body.len()
    )?;
    client.flush()
}

/// Copy bytes both ways, half-closing as each direction ends. Once the upstream side has
/// finished, the client gets [`DRAIN_GRACE`] to finish its upload; after that both sockets are
/// closed so an idle client cannot hold the connection (and its slot) open indefinitely.
pub(super) fn relay<A: Stream, B: Stream>(a: A, b: B) -> io::Result<()> {
    let mut a_read = a.split()?;
    let mut b_write = b.split()?;
    let upload = std::thread::Builder::new()
        .name("dial-netproxy-relay".into())
        .spawn(move || {
            let _ = io::copy(&mut a_read, &mut b_write);
            b_write.close(Shutdown::Write);
        })?;
    let (mut b_read, mut a_write) = (b, a);
    let _ = io::copy(&mut b_read, &mut a_write);
    a_write.close(Shutdown::Write);
    let deadline = std::time::Instant::now() + DRAIN_GRACE;
    while !upload.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !upload.is_finished() {
        // Shutting the shared sockets down wakes the upload thread's blocked read.
        a_write.close(Shutdown::Both);
        b_read.close(Shutdown::Both);
    }
    let _ = upload.join();
    Ok(())
}

/// Accept loopback connections inside the sandbox and splice each one to the proxy socket.
pub(super) fn spawn_bridge(listener: TcpListener, socket: PathBuf) -> io::Result<()> {
    std::thread::Builder::new()
        .name("dial-net-bridge".into())
        .spawn(move || {
            serve(
                listener.incoming(),
                "dial-net-bridge-conn",
                None,
                move |client| {
                    if let Ok(proxy) = UnixStream::connect(&socket) {
                        let _ = relay(client, proxy);
                    }
                },
            )
        })?;
    Ok(())
}

/// Entry point for [`super::BRIDGE_ARG`]: `<socket> -- <program> [args...]`. Listens on the
/// namespace's loopback [`BRIDGE_PORT`], then runs the program and exits with its status.
pub fn bridge_main() -> i32 {
    use std::os::unix::process::ExitStatusExt;
    let mut args = std::env::args_os().skip(2);
    let (Some(socket), Some(separator), Some(program)) = (args.next(), args.next(), args.next())
    else {
        eprintln!(
            "dial: usage: {} <socket> -- <program> [args...]",
            super::BRIDGE_ARG
        );
        return 125;
    };
    let fork_fence = separator == OsStr::new(super::FENCED_SEPARATOR);
    if separator != OsStr::new("--") && !(cfg!(target_os = "linux") && fork_fence) {
        eprintln!("dial: network bridge expects `--` before the program");
        return 125;
    }
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, BRIDGE_PORT)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("dial: sandbox network bridge cannot listen on port {BRIDGE_PORT}: {error}");
            return 125;
        }
    };
    if let Err(error) = spawn_bridge(listener, PathBuf::from(socket)) {
        eprintln!("dial: sandbox network bridge cannot start: {error}");
        return 125;
    }
    let mut command = std::process::Command::new(&program);
    command.args(args);
    #[cfg(target_os = "linux")]
    if fork_fence && let Err(error) = super::super::fork_fence::direct(&mut command) {
        eprintln!("dial: cannot establish eval process-fork containment: {error}");
        return 125;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // The command never outlives the bridge that gives it network.
        // SAFETY: the post-fork closure only invokes Linux's prctl syscall wrapper
        // with scalar PR_SET_PDEATHSIG/SIGKILL arguments and reads errno on failure;
        // it neither allocates nor takes a Rust lock inherited from another thread.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    match command.status() {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(error) => {
            eprintln!("dial: cannot run {}: {error}", program.to_string_lossy());
            127
        }
    }
}
