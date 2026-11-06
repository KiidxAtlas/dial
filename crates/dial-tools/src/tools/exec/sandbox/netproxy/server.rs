//! The filtering proxy server and the Linux namespace bridge (see the parent module docs).

use super::{BRIDGE_PORT, Decision, NetworkRules, is_public};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// A running proxy: a loopback TCP port (macOS clients) and a Unix socket in a private directory
/// (bound into Linux sandboxes). It serves until the process exits.
#[derive(Debug)]
pub struct Endpoint {
    port: u16,
    socket: PathBuf,
    _dir: tempfile::TempDir,
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

/// The process-wide proxy for `rules`, started on first use.
pub fn endpoint(rules: &NetworkRules) -> Result<Arc<Endpoint>, String> {
    static RUNNING: LazyLock<Mutex<HashMap<NetworkRules, Arc<Endpoint>>>> =
        LazyLock::new(Mutex::default);
    let mut running = RUNNING.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(endpoint) = running.get(rules) {
        return Ok(endpoint.clone());
    }
    let endpoint = Arc::new(
        start(rules.clone())
            .map_err(|error| format!("starting the sandbox network proxy: {error}"))?,
    );
    running.insert(rules.clone(), endpoint.clone());
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
    let tcp_rules = rules.clone();
    std::thread::Builder::new()
        .name("dial-netproxy-tcp".into())
        .spawn(move || {
            serve(tcp.incoming(), "dial-netproxy-conn", move |client| {
                let _ = handle(client, &tcp_rules);
            })
        })?;
    std::thread::Builder::new()
        .name("dial-netproxy-unix".into())
        .spawn(move || {
            serve(unix.incoming(), "dial-netproxy-conn", move |client| {
                let _ = handle(client, &rules);
            })
        })?;
    Ok(Endpoint {
        port,
        socket,
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
    handler: impl Fn(S) + Send + Sync + 'static,
) {
    let handler = Arc::new(handler);
    let active = Arc::new(AtomicUsize::new(0));
    for client in incoming {
        let client = match client {
            Ok(client) => client,
            Err(_) => {
                std::thread::sleep(ACCEPT_BACKOFF);
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
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line");
    };
    if !version.starts_with("HTTP/1.") {
        return Err("only HTTP/1.x proxy requests are supported");
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, None)?;
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
    let mut forward = format!("{method} {path} {version}\r\n");
    let mut has_host = false;
    for header in lines.filter(|line| !line.is_empty()) {
        let name = header.split(':').next().unwrap_or_default().trim();
        if [
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
        has_host |= name.eq_ignore_ascii_case("host");
        forward.push_str(header);
        forward.push_str("\r\n");
    }
    if !has_host {
        forward.push_str(&format!("Host: {authority}\r\n"));
    }
    // One request per connection, so every request passes the host check.
    forward.push_str("Connection: close\r\n\r\n");
    Ok(Request {
        host,
        port,
        forward: Some(forward.into_bytes()),
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    (text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &text[prefix.len()..])
}

fn split_host_port(authority: &str, default: Option<u16>) -> Result<(String, u16), &'static str> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("unterminated IPv6 address")?;
        (host, after.strip_prefix(':'))
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
            serve(listener.incoming(), "dial-net-bridge-conn", move |client| {
                if let Ok(proxy) = UnixStream::connect(&socket) {
                    let _ = relay(client, proxy);
                }
            })
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
    if fork_fence
        && let Err(error) = super::super::fork_fence::direct(&mut command)
    {
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
