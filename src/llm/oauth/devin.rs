//! Devin browser sign-in: PKCE authorization-code grant over a fixed loopback redirect
//! The token endpoint takes a JSON body with `code` and `code_verifier`.
use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const AUTHORIZE: &str = "https://app.devin.ai/auth/cli/continue";
const TOKEN: &str = "https://api.devin.ai/auth/cli/token";
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 59653;
const CALLBACK_PATH: &str = "/callback";
const INSTRUCTIONS: &str = "Sign in to Devin in your browser.";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Default expiry skew for JWT-based tokens.
const SKEW_MS: u64 = 5 * 60 * 1000;
/// Used when the access token carries no JWT `exp`.
const FALLBACK_EXPIRY_MS: u64 = 31_536_000_000;
const API_ENDPOINT: &str = "https://api.devin.ai";
const ENTERPRISE_URL: &str = "https://app.devin.ai";

pub const FLOW: Flow = Flow {
    id: "devin",
    name: "Devin",
    base_url: "https://server.codeium.com",
    protocol: ApiProtocol::DevinAgent,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: None,
    access,
};

fn login(l: &Login<'_>) -> Result<Credentials> {
    let pkce = util::pkce();
    let state = util::uuid_v4();
    // The loopback listener is the normal path. When its port is busy the redirect still goes to
    // the same advertised address; the user pastes the URL from the address bar instead.
    let callback = util::Callback::bind(CALLBACK_HOST, CALLBACK_PORT, CALLBACK_PATH).ok();
    let redirect = match &callback {
        Some(callback) => callback.redirect_uri(),
        None => format!("http://{CALLBACK_HOST}:{CALLBACK_PORT}{CALLBACK_PATH}"),
    };
    let instructions = match &callback {
        Some(_) => INSTRUCTIONS.to_owned(),
        None => format!(
            "{INSTRUCTIONS} The local callback port {CALLBACK_PORT} is unavailable: after signing in, paste the full redirect URL from the browser address bar."
        ),
    };
    l.open(
        &authorize_url(&redirect, &state, &pkce.challenge),
        Some(&instructions),
    );
    let code = match &callback {
        Some(callback) => {
            let fields = callback.wait(Some(&state), &l.cancel, LOGIN_TIMEOUT)?;
            fields
                .get("code")
                .cloned()
                .context("Devin callback omitted authorization code")?
        }
        None => paste_code(l, &state)?,
    };
    let body = exchange_code(TOKEN, &code, &pkce.verifier, &l.cancel)?;
    credentials(&body)
}

/// Pasted redirect URL, query string, or bare code. A pasted `state` must match the one we sent.
fn paste_code(l: &Login<'_>, state: &str) -> Result<String> {
    let input =
        l.ui.prompt("Redirect URL or authorization code", None, false)?;
    let (code, received) = util::parse_pasted_code(&input)?;
    if let Some(received) = received {
        anyhow::ensure!(received == state, "Devin OAuth state mismatch");
    }
    Ok(code)
}

/// Authorize URL: standard PKCE set (no `client_id`/`scope` configured), then
/// the rule's `prompt=select_account`.
fn authorize_url(redirect_uri: &str, state: &str, challenge: &str) -> String {
    util::with_query(
        AUTHORIZE,
        &[
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("prompt", "select_account"),
        ],
    )
}

fn exchange_code(
    token_url: &str,
    code: &str,
    verifier: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<Value> {
    let body = json!({"code": code, "code_verifier": verifier});
    util::request(
        "POST",
        token_url,
        &[("accept", "application/json")],
        util::Body::Json(&body),
        cancel,
    )?
    .require_ok("Devin token exchange")?
    .json()
}

/// Map the token response: `token` is both access and refresh; expiry comes from the JWT `exp`.
fn credentials(body: &Value) -> Result<Credentials> {
    let access = util::token_field(body, "/token", "Devin token")?;
    let expires_at_ms = util::jwt_expiry_ms(&access, SKEW_MS)
        .unwrap_or_else(|| util::now_ms() + FALLBACK_EXPIRY_MS);
    Ok(Credentials {
        refresh: access.clone(),
        access,
        expires_at_ms,
        api_endpoint: Some(API_ENDPOINT.into()),
        enterprise_url: Some(ENTERPRISE_URL.into()),
        ..Default::default()
    })
}

fn access(c: &Credentials) -> Result<Access> {
    super::bearer(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot loopback server; returns the token URL and the captured request.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/auth/cli/token",
            listener.local_addr().unwrap().port()
        );
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                raw.extend_from_slice(&chunk[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..at]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= at + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&raw).into_owned()
        });
        (url, handle)
    }

    fn jwt_with_exp(exp: u64) -> String {
        format!(
            "h.{}.s",
            URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#))
        )
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_redirect() {
        let pkce = util::pkce();
        let url = authorize_url(
            "http://127.0.0.1:59653/callback",
            "state-1",
            &pkce.challenge,
        );
        let (base, query) = url.split_once('?').unwrap();
        assert_eq!(base, AUTHORIZE);
        let params = util::parse_query(query).unwrap();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:59653/callback");
        assert_eq!(
            params["code_challenge"],
            util::pkce_challenge(&pkce.verifier)
        );
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], "state-1");
        assert_eq!(params["prompt"], "select_account");
        assert!(!params.contains_key("client_id"));
    }

    #[test]
    fn token_exchange_posts_json_and_maps_credentials() {
        let jwt = jwt_with_exp(4_102_444_800);
        let (url, server) = serve_once("200 OK", &format!(r#"{{"token":"{jwt}"}}"#));
        let cancel = Arc::new(AtomicBool::new(false));
        let body = exchange_code(&url, "code-9", "verifier-9", &cancel).unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /auth/cli/token "));
        assert!(request.contains(r#""code":"code-9""#));
        assert!(request.contains(r#""code_verifier":"verifier-9""#));

        let c = credentials(&body).unwrap();
        assert_eq!(c.access, jwt);
        assert_eq!(c.refresh, jwt);
        assert_eq!(c.expires_at_ms, 4_102_444_800_000 - SKEW_MS);
        assert_eq!(c.api_endpoint.as_deref(), Some(API_ENDPOINT));
        assert_eq!(c.enterprise_url.as_deref(), Some(ENTERPRISE_URL));
    }

    #[test]
    fn token_exchange_rejects_http_errors_without_echoing_body() {
        let (url, server) = serve_once("401 Unauthorized", r#"{"error":"secret-detail"}"#);
        let cancel = Arc::new(AtomicBool::new(false));
        let error = exchange_code(&url, "bad", "v", &cancel).unwrap_err();
        server.join().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("401"), "{message}");
        assert!(!message.contains("secret-detail"), "{message}");
    }

    #[test]
    fn missing_token_is_an_error() {
        assert!(credentials(&json!({"other": "x"})).is_err());
    }

    struct PasteUi(String);

    impl crate::llm::oauth::Interaction for PasteUi {
        fn auth(&self, _url: &str, _instructions: Option<&str>) {}
        fn progress(&self, _message: &str) {}
        fn prompt(
            &self,
            _message: &str,
            _placeholder: Option<&str>,
            _allow_empty: bool,
        ) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    fn paste_login(input: &str, state: &str) -> Result<String> {
        let ui = PasteUi(input.to_owned());
        let login = Login {
            ui: &ui,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        paste_code(&login, state)
    }

    #[test]
    fn pasted_redirect_url_yields_code_when_state_matches() {
        let url = format!("{CALLBACK_PATH}?code=code-7&state=st-1");
        let input = format!("http://127.0.0.1:{CALLBACK_PORT}{url}");
        assert_eq!(paste_login(&input, "st-1").unwrap(), "code-7");
        assert_eq!(paste_login("code-8#st-1", "st-1").unwrap(), "code-8");
        assert_eq!(paste_login("code-9", "st-1").unwrap(), "code-9");
    }

    #[test]
    fn pasted_code_with_other_state_is_rejected() {
        let error = paste_login("code-7#other", "st-1").unwrap_err();
        assert!(format!("{error:#}").contains("state mismatch"));
    }
}
