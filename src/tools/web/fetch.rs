//! The guarded HTTP client behind both web tools.
//!
//! * http/https only, no credentials in the URL, no proxy (a proxy would resolve the name itself and
//!   bypass [`GuardedResolver`]).
//! * Redirects are followed manually, at most [`MAX_REDIRECTS`], and each hop is re-validated and
//!   re-resolved through the guard. A bearer token is only sent to the origin it was meant for.
//! * One overall deadline across all hops, and a cap on how much of the body is read.

use super::guard::GuardedResolver;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use ureq::Error;
use ureq::http::Uri;
use ureq::unversioned::transport::DefaultConnector;

pub const MAX_REDIRECTS: usize = 5;
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
/// How often a blocked request re-checks the interrupt flag.
const INTERRUPT_POLL: Duration = Duration::from_millis(50);
const MAX_URL_LEN: usize = 2048;
pub const USER_AGENT: &str = concat!(
    "dial-agent/",
    env!("CARGO_PKG_VERSION"),
    " (coding agent; web tools)"
);

pub struct Response {
    /// The URL that finally answered, after redirects.
    pub url: String,
    pub status: u16,
    /// Media type without parameters, lowercased.
    pub media_type: Option<String>,
    pub charset: Option<String>,
    pub body: Vec<u8>,
    /// The body was longer than [`MAX_BODY_BYTES`] and has been cut.
    pub truncated: bool,
}

pub struct Client {
    agent: ureq::Agent,
    interrupt: Arc<AtomicBool>,
}

impl Client {
    pub fn new() -> Self {
        Self::with_resolver(GuardedResolver::new())
    }

    #[cfg(test)]
    pub fn allowing_loopback() -> Self {
        Self::with_resolver(GuardedResolver::allowing_loopback())
    }

    fn with_resolver(resolver: GuardedResolver) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .build();
        Self {
            agent: ureq::Agent::with_parts(config, DefaultConnector::new(), resolver),
            interrupt: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Abort in-flight and future requests until [`reset_interrupt`](Self::reset_interrupt).
    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::Release);
    }

    /// Clear a previous [`interrupt`](Self::interrupt) at the start of a new operation.
    pub fn reset_interrupt(&self) {
        self.interrupt.store(false, Ordering::Release);
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupt.load(Ordering::Acquire)
    }

    /// GET `url`. `bearer` is attached only to requests to the origin of `url`.
    pub fn get(&self, url: &str, accept: &str, bearer: Option<&str>) -> Result<Response, String> {
        let authorization = bearer.map(|token| format!("Bearer {token}"));
        let headers: Vec<(&str, &str)> = authorization
            .as_deref()
            .map(|value| ("Authorization", value))
            .into_iter()
            .collect();
        self.send(url, accept, &headers, None)
    }

    /// GET (or POST when `post` carries `(content_type, body)`) through the guard. `headers` (API
    /// keys included) are attached only to requests to the origin of `url`, never to a redirect
    /// target elsewhere. 307/308 keep the method and body; other redirects continue as GET.
    pub fn send(
        &self,
        url: &str,
        accept: &str,
        headers: &[(&str, &str)],
        post: Option<(&str, &[u8])>,
    ) -> Result<Response, String> {
        self.send_limited(url, accept, headers, post, MAX_BODY_BYTES)
    }

    /// [`send`](Self::send) with a caller-chosen body cap (documents exceed [`MAX_BODY_BYTES`]).
    /// Same guard, redirects, header scoping, and deadline; `truncated` reports a cut at `limit`.
    /// The request runs on a worker thread so [`interrupt`](Self::interrupt) returns within
    /// [`INTERRUPT_POLL`]; an abandoned request still ends at its own deadline.
    pub fn send_limited(
        &self,
        url: &str,
        accept: &str,
        headers: &[(&str, &str)],
        post: Option<(&str, &[u8])>,
        limit: usize,
    ) -> Result<Response, String> {
        let agent = self.agent.clone();
        let url = url.to_owned();
        let accept = accept.to_owned();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        let post = post.map(|(content_type, body)| (content_type.to_owned(), body.to_vec()));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let post = post
                .as_ref()
                .map(|(content_type, body)| (content_type.as_str(), body.as_slice()));
            let _ = tx.send(Self::send_hops(
                &agent, &url, &accept, &headers, post, limit,
            ));
        });
        loop {
            match rx.recv_timeout(INTERRUPT_POLL) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {
                    if self.interrupt.load(Ordering::Acquire) {
                        return Err("web request cancelled".into());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err("web request failed".into()),
            }
        }
    }

    fn send_hops(
        agent: &ureq::Agent,
        url: &str,
        accept: &str,
        headers: &[(&str, &str)],
        mut post: Option<(&str, &[u8])>,
        limit: usize,
    ) -> Result<Response, String> {
        let deadline = Instant::now() + TIMEOUT;
        let first = parse_url(url)?;
        let mut current = first.clone();
        for _ in 0..=MAX_REDIRECTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("request timed out".into());
            }
            let same_origin = origin(&current) == origin(&first);
            let sent = match post {
                None => {
                    let mut request = agent
                        .get(current.to_string())
                        .config()
                        .timeout_global(Some(remaining))
                        .build()
                        .header("Accept", accept);
                    for &(name, value) in headers.iter().filter(|_| same_origin) {
                        request = request.header(name, value);
                    }
                    request.call()
                }
                Some((content_type, body)) => {
                    let mut request = agent
                        .post(current.to_string())
                        .config()
                        .timeout_global(Some(remaining))
                        .build()
                        .header("Accept", accept)
                        .header("Content-Type", content_type);
                    for &(name, value) in headers.iter().filter(|_| same_origin) {
                        request = request.header(name, value);
                    }
                    request.send(body)
                }
            };
            let mut response = sent.map_err(describe)?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| format!("HTTP {status} redirect without a Location header"))?;
                let next = resolve_url(&current.to_string(), location)
                    .ok_or_else(|| "redirect to an invalid URL".to_string())?;
                current = parse_url(&next).map_err(|error| format!("redirect refused: {error}"))?;
                if !matches!(status, 307 | 308) {
                    post = None;
                }
                continue;
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let (media_type, charset) = split_content_type(content_type.as_deref());
            let mut body = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(limit as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|error| format!("reading response body: {}", io_message(&error)))?;
            let truncated = body.len() > limit;
            body.truncate(limit);
            return Ok(Response {
                url: current.to_string(),
                status,
                media_type,
                charset,
                body,
                truncated,
            });
        }
        Err(format!("too many redirects (more than {MAX_REDIRECTS})"))
    }
}

/// Validate a model- or config-supplied URL: http(s), a host, no embedded credentials.
pub fn parse_url(url: &str) -> Result<Uri, String> {
    let url = url.trim();
    if url.len() > MAX_URL_LEN {
        return Err(format!("URL is longer than {MAX_URL_LEN} characters"));
    }
    if has_scheme(url) {
        let scheme = url
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!(
                "unsupported URL scheme {scheme:?}; only http and https are allowed"
            ));
        }
    }
    let uri: Uri = url.parse().map_err(|_| "not a valid URL".to_string())?;
    if uri.scheme_str().is_none() {
        return Err("URL must be absolute and start with http:// or https://".into());
    }
    let authority = uri.authority().ok_or("URL has no host")?;
    if authority.as_str().contains('@') {
        return Err("URLs with embedded credentials are not allowed".into());
    }
    Ok(uri)
}

fn origin(uri: &Uri) -> (Option<&str>, Option<&str>, Option<u16>) {
    let default_port = if uri.scheme_str() == Some("https") {
        443
    } else {
        80
    };
    (
        uri.scheme_str(),
        uri.host(),
        Some(uri.port_u16().unwrap_or(default_port)),
    )
}

fn split_content_type(header: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(header) = header else {
        return (None, None);
    };
    let mut parts = header.split(';');
    let media = parts
        .next()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty());
    let charset = parts.find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    });
    (media, charset)
}

fn io_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "request timed out".into(),
        _ => error.to_string(),
    }
}

fn describe(error: Error) -> String {
    match error {
        Error::Io(io) => io_message(&io),
        Error::Timeout(_) => "request timed out".into(),
        Error::HostNotFound => "host not found".into(),
        other => other.to_string(),
    }
}

/// Resolve `reference` (absolute, scheme-relative, absolute-path, query-only or relative) against
/// the absolute URL `base`. Fragments are dropped. `None` when `base` has no scheme and authority.
pub fn resolve_url(base: &str, reference: &str) -> Option<String> {
    let reference = reference.trim().split('#').next().unwrap_or("");
    if has_scheme(reference) {
        return Some(reference.to_owned());
    }
    let base: Uri = base.parse().ok()?;
    let (scheme, authority) = (base.scheme_str()?, base.authority()?);
    if let Some(rest) = reference.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let (path, query) = match reference.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (reference, None),
    };
    let merged = if path.starts_with('/') {
        path.to_owned()
    } else if path.is_empty() {
        base.path().to_owned()
    } else {
        let dir = &base.path()[..base.path().rfind('/').map_or(0, |i| i + 1)];
        format!("{dir}{path}")
    };
    let query = match (query, path.is_empty()) {
        (Some(query), _) => Some(query),
        (None, true) => base.query(),
        (None, false) => None,
    };
    let mut url = format!("{scheme}://{authority}{}", normalize_path(&merged));
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    Some(url)
}

fn has_scheme(reference: &str) -> bool {
    let Some((scheme, _)) = reference.split_once(':') else {
        return false;
    };
    scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Remove `.` and `..` segments (RFC 3986 section 5.2.4), keeping a trailing slash.
fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        match segment {
            "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
        if matches!(segment, "." | "..") && segments.peek().is_none() {
            out.push("");
        }
    }
    if out.first() != Some(&"") {
        out.insert(0, "");
    }
    out.join("/")
}

pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (byte, ..) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests;
