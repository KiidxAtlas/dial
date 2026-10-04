//! Native OpenRouter browser sign-in using its PKCE authorization flow.

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CALLBACK_PATH: &str = "/callback";
const CALLBACK_PORT: u16 = 54549;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_CALLBACK_REQUEST: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

/// An API token whose formatting and destruction do not expose its contents.
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_mut_string(&mut self) -> &mut String {
        &mut self.0
    }

    fn wipe(&mut self) {
        // NUL is valid UTF-8, so overwriting the current contents preserves String's invariant.
        // SAFETY: writing zero bytes cannot create invalid UTF-8.
        unsafe { self.0.as_mut_vec().fill(0) };
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<redacted>)")
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// Run the OpenRouter PKCE flow and return the minted durable API key.
pub fn openrouter_login() -> Result<SecretToken> {
    let listener = TcpListener::bind(("localhost", CALLBACK_PORT))
        .map_err(|_| anyhow::anyhow!("could not start the OpenRouter sign-in callback listener"))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| anyhow::anyhow!("could not configure the OpenRouter callback listener"))?;

    let verifier = SecretToken::new(random_verifier());
    let challenge = pkce_challenge(verifier.as_str());
    let url = authorization_url(&challenge);
    let opened = Command::new("open")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !opened {
        bail!("could not open browser; open this URL to sign in to OpenRouter: {url}");
    }

    let code = SecretToken::new(receive_callback(&listener, Instant::now() + LOGIN_TIMEOUT)?);
    exchange_code(code.as_str(), verifier.as_str())
}

fn random_verifier() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorization_url(challenge: &str) -> String {
    format!(
        "https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A54549%2Fcallback&code_challenge={challenge}&code_challenge_method=S256&key_label=dial"
    )
}

fn receive_callback(listener: &TcpListener, deadline: Instant) -> Result<String> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // The listener polls non-blocking and macOS accepted sockets inherit that, which would
                // ignore the read timeout below and fail instantly if the request has not arrived yet.
                stream
                    .set_nonblocking(false)
                    .and_then(|()| stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now()))))
                    .map_err(|_| anyhow::anyhow!("could not configure the sign-in callback connection"))?;
                let result = read_callback(&mut stream);
                let (status, message) = match &result {
                    Ok(_) => ("200 OK", "OpenRouter sign-in complete. You may close this page."),
                    Err(_) => ("400 Bad Request", "Invalid OpenRouter sign-in callback."),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
                    message.len()
                );
                let _ = stream.write_all(response.as_bytes());
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("OpenRouter sign-in timed out");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => bail!("could not accept the OpenRouter sign-in callback"),
        }
    }
}

fn read_callback(stream: &mut TcpStream) -> Result<String> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        if request.len() >= MAX_CALLBACK_REQUEST {
            bail!("OpenRouter callback request exceeds the size limit");
        }
        let remaining = (MAX_CALLBACK_REQUEST - request.len()).min(chunk.len());
        let count = stream
            .read(&mut chunk[..remaining])
            .map_err(|_| anyhow::anyhow!("could not read the OpenRouter callback"))?;
        if count == 0 {
            bail!("incomplete OpenRouter callback request");
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    if request.len() > header_end {
        bail!("OpenRouter callback must not include a request body");
    }
    let headers = std::str::from_utf8(&request[..header_end])
        .map_err(|_| anyhow::anyhow!("malformed OpenRouter callback request"))?;
    if headers.lines().skip(1).any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("content-length:") || lower.starts_with("transfer-encoding:")
    }) {
        bail!("OpenRouter callback must not include a request body");
    }
    let request_line = headers.lines().next().unwrap_or_default();
    let mut parts = request_line.split_ascii_whitespace();
    if parts.next() != Some("GET") {
        bail!("malformed OpenRouter callback request");
    }
    let target = parts.next().ok_or_else(|| anyhow::anyhow!("malformed OpenRouter callback request"))?;
    if parts.next().is_none() || parts.next().is_some() {
        bail!("malformed OpenRouter callback request");
    }
    parse_callback_target(target)
}

fn parse_callback_target(target: &str) -> Result<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        bail!("unexpected OpenRouter callback path");
    }
    let mut code = None;
    for item in query.split('&') {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        if decode_component(key)? == "code" {
            if code.is_some() {
                bail!("malformed OpenRouter callback code");
            }
            let value = decode_component(value)?;
            if value.is_empty() || value.len() > 4096 || !value.bytes().all(|b| b.is_ascii_graphic()) {
                bail!("malformed OpenRouter callback code");
            }
            code = Some(value);
        }
    }
    code.ok_or_else(|| anyhow::anyhow!("OpenRouter callback did not include an authorization code"))
}

fn decode_component(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => { decoded.push(b' '); index += 1; }
            b'%' => {
                if index + 2 >= bytes.len() { bail!("malformed OpenRouter callback encoding"); }
                let high = hex(bytes[index + 1]).ok_or_else(|| anyhow::anyhow!("malformed OpenRouter callback encoding"))?;
                let low = hex(bytes[index + 2]).ok_or_else(|| anyhow::anyhow!("malformed OpenRouter callback encoding"))?;
                decoded.push(high * 16 + low);
                index += 3;
            }
            byte if byte.is_ascii() => { decoded.push(byte); index += 1; }
            _ => bail!("malformed OpenRouter callback encoding"),
        }
    }
    String::from_utf8(decoded).map_err(|_| anyhow::anyhow!("malformed OpenRouter callback encoding"))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn exchange_code(code: &str, verifier: &str) -> Result<SecretToken> {
    exchange_code_at(TOKEN_URL, code, verifier)
}

fn exchange_code_at(url: &str, code: &str, verifier: &str) -> Result<SecretToken> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    );
    let mut response = agent
        .post(url)
        .header("Content-Type", "application/json")
        .send_json(serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256"
        }))
        .map_err(|_| anyhow::anyhow!("OpenRouter token exchange failed"))?;
    if !(200..300).contains(&response.status().as_u16()) {
        bail!("OpenRouter token exchange was rejected");
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("could not read OpenRouter token response"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bail!("OpenRouter token response exceeds the size limit");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid OpenRouter token response"))?;
    let key = validate_key_response(&value)?;
    Ok(SecretToken::new(key))
}

fn validate_key_response(value: &serde_json::Value) -> Result<String> {
    let key = value
        .get("key")
        .and_then(serde_json::Value::as_str)
        .filter(|key| key.starts_with("sk-or-") && key.len() > "sk-or-".len() && key.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or_else(|| anyhow::anyhow!("OpenRouter token response did not contain a valid API key"))?;
    Ok(key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        thread,
    };

    fn token_server(status: u16, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let job = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut body_len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("content-length:") {
                    body_len = lower["content-length:".len()..].trim().parse::<usize>().unwrap();
                }
                request.push_str(&line);
                if line == "\r\n" || line.is_empty() { break; }
            }
            let mut payload = vec![0; body_len];
            reader.read_exact(&mut payload).unwrap();
            request.push_str(std::str::from_utf8(&payload).unwrap());
            write!(stream, "HTTP/1.1 {status} result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            request
        });
        (format!("http://{address}/api/v1/auth/keys"), job)
    }

    #[test]
    fn callback_listener_accepts_only_the_local_callback_and_times_out() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || receive_callback(&listener, Instant::now() + Duration::from_secs(10)));
        let mut browser = TcpStream::connect(address).unwrap();
        browser.write_all(b"GET /callback?code=callback-code HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).unwrap();
        assert!(response.contains("OpenRouter sign-in complete"));
        assert_eq!(server.join().unwrap().unwrap(), "callback-code");

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(receive_callback(&listener, Instant::now()).unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn callback_waits_for_a_request_that_arrives_after_the_connection() {
        // A browser can open the loopback connection before it sends the request. Accepted sockets
        // inherit non-blocking mode from the polling listener on macOS, which made the read fail instantly.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || receive_callback(&listener, Instant::now() + Duration::from_secs(10)));
        let mut browser = TcpStream::connect(address).unwrap();
        thread::sleep(Duration::from_millis(300));
        browser.write_all(b"GET /callback?code=late-code HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).unwrap();
        assert!(response.contains("OpenRouter sign-in complete"), "{response}");
        assert_eq!(server.join().unwrap().unwrap(), "late-code");
    }

    #[test]
    fn code_exchange_posts_pkce_and_never_echoes_provider_errors() {
        let (url, job) = token_server(200, r#"{"key":"sk-or-created"}"#);
        let token = exchange_code_at(&url, "auth-code", "pkce-verifier").unwrap();
        assert_eq!(token.as_str(), "sk-or-created");
        assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
        let request = job.join().unwrap();
        assert!(request.starts_with("POST /api/v1/auth/keys "));
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["code"], "auth-code");
        assert_eq!(body["code_verifier"], "pkce-verifier");
        assert_eq!(body["code_challenge_method"], "S256");

        let (url, job) = token_server(403, r#"{"error":"do not expose this body"}"#);
        let error = exchange_code_at(&url, "private-code", "private-verifier").unwrap_err().to_string();
        assert!(error.contains("rejected"));
        assert!(!error.contains("private-code") && !error.contains("private-verifier") && !error.contains("do not expose"));
        job.join().unwrap();
    }

    #[test]
    fn authorization_url_contains_documented_pkce_parameters() {
        let url = authorization_url("challenge-value");
        assert_eq!(url, "https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A54549%2Fcallback&code_challenge=challenge-value&code_challenge_method=S256&key_label=dial");
        assert!(!url.contains("state="));
        assert!(!url.contains("client_id="));
    }

    #[test]
    fn callback_accepts_exact_path_and_decodes_code() {
        assert_eq!(parse_callback_target("/callback?other=x&code=a%2Bb%2Fc").unwrap(), "a+b/c");
        assert!(parse_callback_target("/elsewhere?code=abc").is_err());
        assert!(parse_callback_target("/callback?code=x&code=y").is_err());
        assert!(parse_callback_target("/callback?error=access_denied").is_err());
    }

    #[test]
    fn token_response_requires_nonempty_openrouter_key() {
        assert_eq!(validate_key_response(&serde_json::json!({"key":"sk-or-test"})).unwrap(), "sk-or-test");
        for value in [serde_json::json!({}), serde_json::json!({"key": 1}), serde_json::json!({"key":""}), serde_json::json!({"key":"sk-test"})] {
            assert!(validate_key_response(&value).is_err());
        }
    }

    #[test]
    fn secret_debug_redacts_and_drop_zeroes_contents() {
        let mut token = SecretToken::new("sk-or-private".into());
        assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
        assert_eq!(token.as_mut_string(), "sk-or-private");
        token.wipe();
        assert!(token.0.bytes().all(|byte| byte == 0));
        std::mem::forget(token);
    }
}
