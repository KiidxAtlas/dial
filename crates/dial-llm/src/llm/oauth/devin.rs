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
mod tests;
