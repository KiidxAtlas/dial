//! Anthropic subscription OAuth for the official Messages API.
use super::{auth::SecretToken, oauth::util::run_opener};
use crate::storage::settings_store::{
    locked, locked_cancellable, locked_for, read_private, write_private,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const FILE: &str = "anthropic-subscription.json";
const CALLBACK_PORT: u16 = 54545;
const CALLBACK_PATH: &str = "/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const MAX_JSON: u64 = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    tokens: Option<Tokens>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn decode(value: &str) -> Result<String> {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            ensure!(i + 2 < bytes.len(), "invalid Anthropic OAuth callback");
            out.push(
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3])?, 16)
                    .context("invalid OAuth callback escape")?,
            );
            i += 3;
        } else {
            out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
            i += 1;
        }
    }
    String::from_utf8(out).context("invalid UTF-8 in OAuth callback")
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::SeqCst),
        "Anthropic sign-in cancelled"
    );
    Ok(())
}
fn load(dir: &Path) -> Result<State> {
    let Some(bytes) = read_private(&dir.join(FILE))? else {
        return Ok(State::default());
    };
    serde_json::from_slice(&bytes).context("Anthropic subscription credentials are invalid")
}
fn save(dir: &Path, state: &State) -> Result<()> {
    write_private(&dir.join(FILE), &serde_json::to_vec(state)?)
}
pub fn signed_in(dir: &Path) -> bool {
    load(dir).ok().is_some_and(|s| s.tokens.is_some())
}
fn authorization_url(redirect: &str, state: &str, verifier: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?{}",
        [
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect),
            ("scope", SCOPES),
            ("state", state),
            ("code_challenge", &challenge(verifier)),
            ("code_challenge_method", "S256")
        ]
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
    )
}
fn callback_code(
    stream: &mut TcpStream,
    expected_state: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    let started = std::time::Instant::now();
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                cancelled(cancel)?;
                ensure!(
                    started.elapsed() < Duration::from_secs(2),
                    "Anthropic OAuth callback timed out"
                );
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
        ensure!(
            bytes.len() <= 16 * 1024,
            "Anthropic OAuth callback too large"
        );
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
    let uri: ureq::http::Uri = target.parse().context("invalid OAuth callback URL")?;
    ensure!(
        uri.path() == CALLBACK_PATH,
        "unexpected OAuth callback path"
    );
    let mut fields = std::collections::HashMap::new();
    for pair in uri.query().unwrap_or_default().split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = decode(parts.next().unwrap_or_default())?;
        let value = decode(parts.next().unwrap_or_default())?;
        fields.insert(key, value);
    }
    ensure!(
        fields.get("state").is_some_and(|s| s == expected_state),
        "Anthropic OAuth state did not match"
    );
    if fields.contains_key("error") {
        anyhow::bail!("Anthropic sign-in was declined");
    }
    fields
        .remove("code")
        .filter(|s| !s.is_empty())
        .context("Anthropic OAuth callback did not contain a code")
}
fn browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    let opened = run_opener(&mut command);
    ensure!(opened, "could not open the browser for Anthropic sign-in");
    Ok(())
}
fn read_callback(listener: &TcpListener, state: &str, cancel: &AtomicBool) -> Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        cancelled(cancel)?;
        ensure!(
            std::time::Instant::now() < deadline,
            "Anthropic sign-in timed out; try again from /providers"
        );
        match listener.accept() {
            Ok((mut stream, _)) => {
                // macOS accepted sockets inherit the listener's non-blocking mode, which makes the
                // callback read fail with WouldBlock before the browser request arrives.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                let result = callback_code(&mut stream, state, cancel);
                let accepted = result.is_ok();
                let declined = result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string() == "Anthropic sign-in was declined");
                let body = if accepted {
                    "Anthropic authorization received. Return to Dial."
                } else {
                    "This is not the active Anthropic authorization callback."
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    if accepted {
                        "200 OK"
                    } else {
                        "400 Bad Request"
                    },
                    body.len(),
                    body
                );
                if accepted || declined {
                    return result;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn request_token(body: Value, cancel: Arc<AtomicBool>) -> Result<Value> {
    request_token_at(TOKEN_URL, body, cancel)
}
fn request_token_at(url: &str, body: Value, cancel: Arc<AtomicBool>) -> Result<Value> {
    let mut response = super::agent_with(30, true, Some(cancel), None)
        .post(url)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(
            "User-Agent",
            "anthropic-sdk-typescript/0.112.1 userOAuthProvider",
        )
        .send_json(&body)
        .map_err(|_| anyhow::anyhow!("Anthropic OAuth token request failed"))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_JSON,
        "Anthropic OAuth response exceeded the size limit"
    );
    ensure!(
        (200..300).contains(&status),
        "Anthropic OAuth token request returned HTTP {status}"
    );
    serde_json::from_slice(&bytes).context("invalid Anthropic OAuth token response")
}
fn tokens(value: &Value, previous: Option<&Tokens>) -> Result<Tokens> {
    let access = value["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Anthropic OAuth response has no access token")?
        .to_owned();
    let refresh = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| previous.map(|p| p.refresh_token.clone()))
        .context("Anthropic OAuth response has no refresh token")?;
    ensure!(
        access.bytes().all(|b| b.is_ascii_graphic())
            && refresh.bytes().all(|b| b.is_ascii_graphic()),
        "invalid Anthropic OAuth token"
    );
    let expires = value["expires_in"].as_u64().unwrap_or(3600);
    Ok(Tokens {
        access_token: access,
        refresh_token: refresh,
        expires_at: now().saturating_add(expires),
    })
}
pub fn login(dir: &Path, cancel: Arc<AtomicBool>) -> Result<String> {
    let listener = TcpListener::bind(("localhost", CALLBACK_PORT))
        .context("Anthropic sign-in callback port 54545 is unavailable")?;
    listener.set_nonblocking(true)?;
    let redirect = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
    let state = random();
    let verifier = random();
    cancelled(&cancel)?;
    browser(&authorization_url(&redirect, &state, &verifier))?;
    let code = read_callback(&listener, &state, &cancel)?;
    let value = request_token(
        json!({"grant_type":"authorization_code","client_id":CLIENT_ID,"code":code,"redirect_uri":redirect,"code_verifier":verifier,"state":state}),
        cancel,
    )?;
    let tokens = tokens(&value, None)?;
    let _lock = locked(&dir.join(FILE))?;
    save(
        dir,
        &State {
            tokens: Some(tokens),
        },
    )?;
    Ok("Claude subscription signed in; choose an Anthropic model in /models".into())
}
pub fn access_token(dir: &Path, cancel: Arc<AtomicBool>) -> Result<SecretToken> {
    access_token_at(dir, TOKEN_URL, cancel)
}
fn access_token_at(dir: &Path, token_url: &str, cancel: Arc<AtomicBool>) -> Result<SecretToken> {
    let _lock = locked_cancellable(&dir.join(FILE), &cancel)?;
    let mut state = load(dir)?;
    let previous = state
        .tokens
        .as_ref()
        .context("sign in with Claude subscription from /providers first")?;
    if previous.expires_at <= now() + 300 {
        let value = request_token_at(
            token_url,
            json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":previous.refresh_token}),
            cancel,
        )?;
        state.tokens = Some(tokens(&value, state.tokens.as_ref())?);
        save(dir, &state)?;
    }
    let token = state.tokens.as_ref().unwrap().access_token.clone();
    Ok(SecretToken::new(token))
}
pub fn sign_out(dir: &Path) -> Result<String> {
    let _lock = locked_for(&dir.join(FILE), None, Duration::from_secs(2))?;
    let mut state = load(dir)?;
    state.tokens = None;
    save(dir, &state)?;
    Ok("Claude subscription signed out of Dial".into())
}
#[cfg(test)]
mod tests;
