//! Shared OAuth plumbing: bounded HTTP, PKCE, loopback callbacks, RFC 8628 device-code polling and
//! JWT claim inspection. Nothing here logs or formats a token; provider bodies never reach errors.
use anyhow::{Context, Result, bail, ensure};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Largest accepted OAuth/userinfo/discovery response body.
pub const MAX_BODY: usize = 256 * 1024;
/// Default whole-request bound for OAuth endpoints.
pub const TIMEOUT_SECS: u64 = 30;
/// Expiry value for grants that never expire.
pub const NEVER_EXPIRES: u64 = u64::MAX;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `bytes` random bytes, base64url without padding.
pub fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// Lowercase hex of `bytes` random bytes.
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    crate::hex(&buffer)
}

/// RFC 4122 version-4 UUID.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = crate::hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// S256 PKCE pair (RFC 7636) with a 43-character verifier.
pub fn pkce() -> Pkce {
    let verifier = random_urlsafe(32);
    let challenge = pkce_challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Percent-encode everything outside RFC 3986's unreserved set.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode `application/x-www-form-urlencoded` text (`+` is a space).
pub fn decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                ensure!(i + 2 < bytes.len(), "invalid percent escape");
                let hex =
                    std::str::from_utf8(&bytes[i + 1..i + 3]).context("invalid percent escape")?;
                out.push(u8::from_str_radix(hex, 16).context("invalid percent escape")?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).context("invalid UTF-8 in URL component")
}

/// `k=v&k=v` with every component percent-encoded.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Append encoded query pairs to `url`.
pub fn with_query(url: &str, pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return url.to_owned();
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{}", form(pairs))
}

/// Parse a query string or form body into decoded key/value pairs (last value wins).
pub fn parse_query(query: &str) -> Result<HashMap<String, String>> {
    let mut fields = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        fields.insert(decode(key)?, decode(value)?);
    }
    Ok(fields)
}

/// Request body variants accepted by [`request`].
pub enum Body<'a> {
    Empty,
    Form(&'a [(&'a str, &'a str)]),
    Json(&'a Value),
}

/// A bounded HTTP response. `text` is never placed in an error by this module.
pub struct Response {
    pub status: u16,
    pub text: String,
    headers: Vec<(String, String)>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.text).context("OAuth endpoint returned invalid JSON")
    }
    /// JSON when parseable, otherwise `Value::Null` (for error classification).
    pub fn json_or_null(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn headers(&self, name: &str) -> impl Iterator<Item = &str> {
        self.headers
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    /// Require a 2xx response; the error names the operation and status only.
    pub fn require_ok(self, what: &str) -> Result<Self> {
        ensure!(self.ok(), "{what} returned HTTP {}", self.status);
        Ok(self)
    }
}

/// One bounded request. Redirects are never followed (a token must not be replayed to another
/// host), HTTP errors are returned as responses, and remote targets honour proxy settings.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Body<'_>,
    cancel: &Arc<AtomicBool>,
) -> Result<Response> {
    request_with_timeout(method, url, headers, body, cancel, TIMEOUT_SECS)
}

pub fn request_with_timeout(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Body<'_>,
    cancel: &Arc<AtomicBool>,
    timeout_secs: u64,
) -> Result<Response> {
    ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
    let uri: ureq::http::Uri = url.parse().context("invalid OAuth endpoint URL")?;
    let scheme = uri.scheme_str().unwrap_or_default();
    let host = uri.host().unwrap_or_default();
    // The GCE metadata server is served only over plain HTTP on its link-local name.
    let local = crate::llm::is_local_host(host) || host == "metadata.google.internal";
    ensure!(
        scheme == "https" || (scheme == "http" && local),
        "OAuth endpoints must use https"
    );
    let agent = crate::llm::agent_with(timeout_secs, !local, Some(Arc::clone(cancel)), None);
    let display = crate::llm::display_url(url);
    let failed = |_| anyhow::anyhow!("request to {display} failed");
    let mut response = match method {
        "GET" | "DELETE" => {
            let mut builder = if method == "GET" {
                agent.get(url)
            } else {
                agent.delete(url)
            };
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            builder.call().map_err(failed)?
        }
        "POST" | "PUT" | "PATCH" => {
            let mut builder = match method {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                _ => agent.patch(url),
            };
            let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            match body {
                Body::Empty => builder.send_empty().map_err(failed)?,
                Body::Form(pairs) => {
                    if !has("content-type") {
                        builder =
                            builder.header("Content-Type", "application/x-www-form-urlencoded");
                    }
                    builder.send(form(pairs).as_bytes()).map_err(failed)?
                }
                Body::Json(value) => {
                    if !has("content-type") {
                        builder = builder.header("Content-Type", "application/json");
                    }
                    builder
                        .send(serde_json::to_vec(value)?.as_slice())
                        .map_err(failed)?
                }
            }
        }
        other => bail!("unsupported OAuth HTTP method {other}"),
    };
    let status = response.status().as_u16();
    let mut headers = response
        .headers()
        .iter()
        .filter(|(n, _)| !n.as_str().eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(n, v)| Some((n.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect::<Vec<_>>();
    headers.extend(
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| Some(("set-cookie".to_owned(), v.to_str().ok()?.to_owned()))),
    );
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("reading the response from {display} failed"))?;
    ensure!(
        bytes.len() <= MAX_BODY,
        "response from {display} exceeded the size limit"
    );
    Ok(Response {
        status,
        text: String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    })
}

/// Open `url` in the user's browser; false when no opener succeeded (the caller shows the URL).
pub fn open_browser(url: &str) -> bool {
    if std::env::var_os("DIAL_NO_BROWSER").is_some() {
        return false;
    }
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    run_opener(&mut command)
}

/// Runs a browser opener and reports whether it succeeded. A text-mode browser can keep the opener
/// running in the foreground, so the wait is bounded; on timeout the opener is left alone and
/// reaped in the background, and the caller falls back to showing the URL.
pub fn run_opener(command: &mut Command) -> bool {
    let Ok(child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    match crate::platform::process::wait_exited(&child, Duration::from_secs(10)) {
        Ok(true) => {
            let mut child = child;
            child.wait().is_ok_and(|s| s.success())
        }
        _ => {
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
            false
        }
    }
}

/// A loopback listener for an authorization-code redirect.
pub struct Callback {
    listener: TcpListener,
    port: u16,
    path: String,
    host: String,
}

impl Callback {
    /// Bind `host:port` (port 0 picks a free port). `path` is the expected redirect path.
    pub fn bind(host: &str, port: u16, path: &str) -> Result<Self> {
        let listener = TcpListener::bind((host, port))
            .with_context(|| format!("sign-in callback port {port} on {host} is unavailable"))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            port,
            path: path.to_owned(),
            host: host.to_owned(),
        })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    /// `http://{host}:{port}{path}`.
    pub fn redirect_uri(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }
    /// Wait for the redirect whose `state` matches (when given) and return its query fields.
    /// Requests for other paths or states are answered with 400 and ignored.
    pub fn wait(
        &self,
        expected_state: Option<&str>,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<HashMap<String, String>> {
        let deadline = Instant::now() + timeout;
        loop {
            ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
            ensure!(
                Instant::now() < deadline,
                "sign-in timed out waiting for the browser redirect"
            );
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let parsed = read_redirect(&mut stream, &self.path);
                    let accepted = match &parsed {
                        Ok(fields) => expected_state
                            .is_none_or(|state| fields.get("state").is_some_and(|s| s == state)),
                        Err(_) => false,
                    };
                    let declined = accepted
                        && parsed
                            .as_ref()
                            .is_ok_and(|fields| fields.contains_key("error"));
                    respond(
                        &mut stream,
                        accepted && !declined,
                        if declined {
                            "Sign-in was declined. You may close this page."
                        } else if accepted {
                            "Sign-in received. Return to Dial; you may close this page."
                        } else {
                            "This is not the active Dial sign-in callback."
                        },
                    );
                    if accepted {
                        let fields = parsed?;
                        if let Some(error) = fields.get("error") {
                            bail!("sign-in was declined by the provider ({})", sanitize(error));
                        }
                        return Ok(fields);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Longest a client may take to deliver its request head. The per-read socket timeout alone lets a
/// client that dribbles one byte at a time hold [`Callback::wait`] past its own cancel/timeout.
const REDIRECT_READ_BUDGET: Duration = Duration::from_secs(5);

fn read_redirect(stream: &mut TcpStream, path: &str) -> Result<HashMap<String, String>> {
    read_redirect_within(stream, path, REDIRECT_READ_BUDGET)
}

fn read_redirect_within(
    stream: &mut TcpStream,
    path: &str,
    budget: Duration,
) -> Result<HashMap<String, String>> {
    let deadline = Instant::now() + budget;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "OAuth callback request timed out");
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut chunk).map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                anyhow::anyhow!("OAuth callback request timed out")
            } else {
                e.into()
            }
        })?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
        ensure!(bytes.len() <= 16 * 1024, "OAuth callback request too large");
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&bytes)?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("invalid OAuth callback request")?;
    let (target_path, query) = target.split_once('?').unwrap_or((target, ""));
    ensure!(target_path == path, "unexpected OAuth callback path");
    parse_query(query)
}

fn respond(stream: &mut TcpStream, ok: bool, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        if ok { "200 OK" } else { "400 Bad Request" },
        body.len(),
        body
    );
}

/// Provider-controlled short codes, safe for one-line messages.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || "_-. ".contains(*c))
        .take(80)
        .collect()
}

/// Extract `code` (and `state`, when present) from a pasted redirect URL, query string, or bare code.
pub fn parse_pasted_code(input: &str) -> Result<(String, Option<String>)> {
    let input = input.trim();
    ensure!(!input.is_empty(), "no authorization code was entered");
    let query = if let Some((_, rest)) = input.split_once('?') {
        Some(rest.split('#').next().unwrap_or(rest))
    } else if input.contains("code=") {
        Some(input)
    } else {
        None
    };
    if let Some(query) = query {
        let fields = parse_query(query)?;
        if let Some(error) = fields.get("error") {
            bail!("sign-in was declined by the provider ({})", sanitize(error));
        }
        let code = fields
            .get("code")
            .filter(|c| !c.is_empty())
            .context("the pasted URL has no authorization code")?;
        return Ok((code.clone(), fields.get("state").cloned()));
    }
    // `code#state` is what some providers display for manual copy.
    if let Some((code, state)) = input.split_once('#') {
        return Ok((code.to_owned(), Some(state.to_owned())));
    }
    Ok((input.to_owned(), None))
}

/// One RFC 8628 poll outcome.
pub enum Poll<T> {
    Complete(T),
    Pending,
    SlowDown,
    Failed(String),
}

/// Poll a device-code grant until completion, failure, expiry or cancellation.
/// `interval_secs` defaults to RFC 8628's five seconds; slow_down adds five seconds.
pub fn poll_device<T>(
    interval_secs: Option<u64>,
    expires_in_secs: Option<u64>,
    cancel: &AtomicBool,
    mut poll: impl FnMut() -> Result<Poll<T>>,
) -> Result<T> {
    // Clamp provider-supplied lifetimes so `Instant + Duration` cannot overflow.
    let deadline =
        expires_in_secs.map(|s| Instant::now() + Duration::from_secs(s.min(365 * 86_400)));
    let mut interval =
        Duration::from_millis(interval_secs.unwrap_or(5).saturating_mul(1000).max(1000));
    let mut slow_downs = 0;
    loop {
        ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        match poll()? {
            Poll::Complete(value) => return Ok(value),
            Poll::Failed(message) => bail!("{message}"),
            Poll::SlowDown => {
                slow_downs += 1;
                interval += Duration::from_secs(5);
            }
            Poll::Pending => {}
        }
        let wait = deadline.map_or(interval, |d| {
            interval.min(d.saturating_duration_since(Instant::now()))
        });
        if wait.is_zero() {
            break;
        }
        sleep_cancellable(wait, cancel)?;
    }
    if slow_downs > 0 {
        bail!(
            "device sign-in timed out after one or more slow_down responses; this is often caused by clock drift in a VM. Sync the clock and try again"
        );
    }
    bail!("device sign-in timed out; start sign-in again")
}

/// Classify a standard RFC 8628 token-endpoint response.
pub fn classify_device_response(response: &Response) -> Poll<Value> {
    let value = response.json_or_null();
    if response.ok()
        && value["access_token"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    {
        return Poll::Complete(value);
    }
    match value["error"].as_str() {
        Some("authorization_pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowDown,
        Some("expired_token") => {
            Poll::Failed("the device code expired; start sign-in again".into())
        }
        Some("access_denied") => Poll::Failed("device sign-in was denied".into()),
        Some(error) => Poll::Failed(format!("device sign-in failed ({})", sanitize(error))),
        None => Poll::Failed(format!(
            "device sign-in failed with HTTP {}",
            response.status
        )),
    }
}

pub fn sleep_cancellable(duration: Duration, cancel: &AtomicBool) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < duration {
        ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
        std::thread::sleep(
            duration
                .saturating_sub(start.elapsed())
                .min(Duration::from_millis(50)),
        );
    }
    Ok(())
}

/// Unverified JWT payload claims (used only for expiry/identity hints, never for authorization).
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| STANDARD.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok().filter(Value::is_object)
}

/// JWT `exp` in milliseconds minus `skew_ms`.
pub fn jwt_expiry_ms(token: &str, skew_ms: u64) -> Option<u64> {
    let exp = jwt_claims(token)?["exp"].as_u64()?;
    Some(exp.saturating_mul(1000).saturating_sub(skew_ms))
}

/// Expiry for an `expires_in` seconds response, minus `skew_ms`.
pub fn expires_in_ms(seconds: Option<u64>, skew_ms: u64, default_secs: u64) -> u64 {
    now_ms()
        .saturating_add(seconds.unwrap_or(default_secs).saturating_mul(1000))
        .saturating_sub(skew_ms)
}

/// A non-empty header-safe token from `value[field]`.
pub fn token_field(value: &Value, pointer: &str, what: &str) -> Result<String> {
    let token = value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{what} response has no {}", pointer.trim_start_matches('/')))?;
    ensure!(
        token.bytes().all(|b| b.is_ascii_graphic()),
        "{what} returned an invalid token"
    );
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests;
