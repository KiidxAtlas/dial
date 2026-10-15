//! Sign in with ChatGPT for Dial's own public OAuth registration. Credentials never enter config,
//! model history, or API-key storage. Protected atomic records and a process lock serialize refresh.
use super::auth::SecretToken;
#[cfg(test)]
use crate::storage::settings_store::locked;
use crate::storage::settings_store::{locked_cancellable, read_private, write_private};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const PROVIDER: &str = "chatgpt";
pub const BASE_URL: &str = "https://chatgpt.com/backend-api";
const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const CLIENT_VERSION: &str = "0.159.0";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const MAX_JSON: u64 = 256 * 1024;
// Account catalogs include model instructions and can be much larger than OAuth responses.
const MAX_CATALOG_JSON: u64 = 8 * 1024 * 1024;
const FILE: &str = "chatgpt-accounts.json";
const CATALOG_FILE: &str = "chatgpt-model-catalog.json";

#[derive(Clone, Default, Serialize, Deserialize)]
struct ModelCatalog {
    models: Vec<super::catalog::ModelEntry>,
    profiles: BTreeMap<String, super::profiles::ModelProfile>,
}

fn cached_catalog(dir: &Path) -> Result<ModelCatalog> {
    cached_catalog_for(dir, None)
}

fn cached_catalog_for(dir: &Path, selected: Option<&str>) -> Result<ModelCatalog> {
    let state = load(dir)?;
    let Some(id) = selected.map(str::to_owned).or(state.active).filter(|id| {
        state
            .accounts
            .get(id)
            .is_some_and(|account| account.tokens.is_some() && account.client_id == CLIENT_ID)
    }) else {
        return Ok(ModelCatalog::default());
    };
    let Some(bytes) = read_private(&dir.join(CATALOG_FILE))? else {
        return Ok(ModelCatalog::default());
    };
    let mut catalogs: BTreeMap<String, ModelCatalog> =
        serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("ChatGPT model metadata cache is invalid; rescan from /models")
        })?;
    Ok(catalogs.remove(&id).unwrap_or_default())
}

/// Account-scoped metadata only. Never reads Codex credentials or makes an inference-time request.
pub fn model_profile(dir: &Path, model: &str) -> Result<Option<super::profiles::ModelProfile>> {
    Ok(cached_catalog(dir)?.profiles.remove(model))
}

pub fn scoped_model_profile(
    dir: &Path,
    account_id: Option<&str>,
    model: &str,
) -> Result<Option<super::profiles::ModelProfile>> {
    Ok(cached_catalog_for(dir, account_id)?.profiles.remove(model))
}

pub(crate) fn scoped_cached_models(
    dir: &Path,
    account_id: &str,
) -> Result<Vec<super::catalog::ModelEntry>> {
    let mut models = cached_catalog_for(dir, Some(account_id))?.models;
    for entry in &mut models {
        entry.selection.account_id = Some(account_id.to_owned());
    }
    Ok(models)
}

pub(crate) fn scoped_model_context_window(
    dir: &Path,
    account_id: Option<&str>,
    model: &str,
) -> Option<u32> {
    cached_catalog_for(dir, account_id)
        .ok()?
        .models
        .into_iter()
        .find(|entry| entry.selection.model == model)?
        .selection
        .context_window
}

fn save_catalog(dir: &Path, id: &str, catalog: &ModelCatalog) -> Result<()> {
    let path = dir.join(CATALOG_FILE);
    let _lock = locked_cancellable(&path, &AtomicBool::new(false))?;
    let mut catalogs: BTreeMap<String, ModelCatalog> = match read_private(&path)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("ChatGPT model metadata cache is invalid; existing records preserved")
        })?,
        None => BTreeMap::new(),
    };
    catalogs.insert(id.into(), catalog.clone());
    write_private(&path, &serde_json::to_vec(&catalogs)?)
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    active: Option<String>,
    accounts: BTreeMap<String, Account>,
}
#[derive(Serialize, Deserialize)]
struct Account {
    client_id: String,
    subject: String,
    email: String,
    tokens: Option<Tokens>,
}
#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: u64,
    scopes: Vec<String>,
    #[serde(default)]
    earliest_refresh_at: u64,
}
// Safe UI metadata only. No bearer, refresh, or ID token can be formatted by a caller.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub client_id: String,
    pub label: String,
    pub active: bool,
    pub signed_in: bool,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn load(dir: &Path) -> Result<State> {
    match read_private(&dir.join(FILE))? {
        None => Ok(State::default()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("ChatGPT credential storage is invalid; existing records preserved")
        }),
    }
}
fn save(dir: &Path, state: &State) -> Result<()> {
    write_private(&dir.join(FILE), &serde_json::to_vec(state)?)
}
pub fn accounts(dir: &Path) -> Result<Vec<AccountInfo>> {
    let state = load(dir)?;
    Ok(state
        .accounts
        .iter()
        .map(|(id, account)| AccountInfo {
            client_id: id.clone(),
            label: format!(
                "{} ({id})",
                if account.email.is_empty() {
                    "ChatGPT account"
                } else {
                    &account.email
                }
            ),
            active: state.active.as_ref() == Some(id),
            signed_in: account.tokens.is_some() && account.client_id == CLIENT_ID,
        })
        .collect())
}
pub fn select_account(dir: &Path, id: &str) -> Result<()> {
    let _lock = locked_cancellable(&dir.join(FILE), &AtomicBool::new(false))?;
    let mut state = load(dir)?;
    ensure!(
        state
            .accounts
            .get(id)
            .is_some_and(|account| account.tokens.is_some() && account.client_id == CLIENT_ID),
        "sign in to this ChatGPT account first"
    );
    state.active = Some(id.into());
    save(dir, &state)
}
fn ascii_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty()
            && token.len() <= 64 * 1024
            && token.bytes().all(|b| b.is_ascii_graphic()),
        "invalid ChatGPT credential"
    );
    Ok(())
}
fn random() -> String {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}
fn decode(s: &str) -> Result<String> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            ensure!(i + 2 < b.len(), "invalid OAuth callback encoding");
            let h = std::str::from_utf8(&b[i + 1..i + 3])?;
            out.push(
                u8::from_str_radix(h, 16)
                    .map_err(|_| anyhow::anyhow!("invalid OAuth callback encoding"))?,
            );
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| anyhow::anyhow!("invalid OAuth callback encoding"))
}
struct Attempt {
    state: String,
    verifier: SecretToken,
    redirect: String,
}
impl Attempt {
    fn url(&self, hints: Option<&Account>) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_str().as_bytes()));
        let fields = [
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect),
            ("scope", SCOPE),
            ("state", &self.state),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "dial"),
        ];
        let mut values = fields
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>();
        if let Some(account) = hints {
            if let Some(tokens) = &account.tokens {
                values.push(format!("id_token_hint={}", encode(&tokens.id_token)));
            }
            if !account.email.is_empty() {
                values.push(format!("login_hint={}", encode(&account.email)));
            }
        }
        format!("{AUTHORIZE_URL}?{}", values.join("&"))
    }
    fn callback(&self, target: &str) -> Result<(SecretToken, String)> {
        let fields = callback_fields(target)?;
        ensure!(
            fields.get("state") == Some(&self.state),
            "OAuth callback state did not match"
        );
        if fields.contains_key("error") {
            bail!("ChatGPT sign-in was declined or failed; no account was changed");
        }
        ensure!(
            fields.get("client_id").is_none_or(|id| id == CLIENT_ID),
            "OAuth callback client ID did not match"
        );
        let code = fields
            .get("code")
            .ok_or_else(|| anyhow::anyhow!("ChatGPT callback did not contain a code"))?
            .clone();
        ascii_token(&code)?;
        Ok((SecretToken::new(code), CLIENT_ID.into()))
    }
}
fn callback_fields(target: &str) -> Result<BTreeMap<String, String>> {
    let (path, query) = target
        .split_once('?')
        .ok_or_else(|| anyhow::anyhow!("invalid OAuth callback"))?;
    ensure!(path == "/auth/callback", "invalid OAuth callback path");
    let mut fields = BTreeMap::new();
    for field in query.split('&') {
        let (k, v) = field.split_once('=').unwrap_or((field, ""));
        ensure!(
            fields.insert(decode(k)?, decode(v)?).is_none(),
            "duplicate OAuth callback field"
        );
    }
    Ok(fields)
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::SeqCst) {
        Err(super::Interrupted.into())
    } else {
        Ok(())
    }
}
fn read_target(stream: &mut TcpStream, cancel: &AtomicBool) -> Result<String> {
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut headers = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        cancelled(cancel)?;
        ensure!(Instant::now() < deadline, "incomplete OAuth callback");
        match stream.read(&mut chunk) {
            Ok(0) => bail!("incomplete OAuth callback"),
            Ok(n) => {
                headers.extend_from_slice(&chunk[..n]);
                ensure!(
                    headers.len() <= 64 * 1024,
                    "OAuth callback headers exceed limits"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => bail!("could not read OAuth callback"),
        }
        if headers.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let line = headers.split(|b| *b == b'\n').next().unwrap_or_default();
    ensure!(line.len() <= 16 * 1024, "OAuth callback exceeds limits");
    let text = std::str::from_utf8(line).map_err(|_| anyhow::anyhow!("invalid OAuth callback"))?;
    let mut parts = text.split_whitespace();
    ensure!(parts.next() == Some("GET"), "invalid OAuth callback method");
    Ok(parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid OAuth callback"))?
        .into())
}
fn receive(
    listener: &TcpListener,
    attempt: &Attempt,
    cancel: &AtomicBool,
) -> Result<(SecretToken, String)> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        cancelled(cancel)?;
        ensure!(
            Instant::now() < deadline,
            "ChatGPT sign-in timed out; start again from /providers"
        );
        match listener.accept() {
            Ok((mut stream, _)) => {
                let target = read_target(&mut stream, cancel);
                let declined = target
                    .as_ref()
                    .ok()
                    .and_then(|target| callback_fields(target).ok())
                    .is_some_and(|fields| {
                        fields.get("state") == Some(&attempt.state) && fields.contains_key("error")
                    });
                let result = target.and_then(|target| attempt.callback(&target));
                let message = if result.is_ok() {
                    "ChatGPT authorization received. Return to Dial while it validates sign-in."
                } else {
                    "Invalid or declined ChatGPT authorization. Return to Dial."
                };
                let status = if result.is_ok() {
                    "200 OK"
                } else {
                    "400 Bad Request"
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
                    message.len()
                );
                if result.is_ok() || declined {
                    return result;
                }
                // Browser preconnections, favicon requests, and callbacks with the wrong state
                // must not consume the pending authorization attempt.
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(_) => bail!("ChatGPT callback listener failed"),
        }
    }
}
fn client(cancel: Arc<AtomicBool>) -> ureq::Agent {
    super::agent_with(25, true, Some(cancel), None)
}
fn response_json(mut response: ureq::http::Response<ureq::Body>) -> Result<Value> {
    limited_json(&mut response, MAX_JSON, "authorization")
}
fn limited_json(
    response: &mut ureq::http::Response<ureq::Body>,
    limit: u64,
    label: &str,
) -> Result<Value> {
    let status = response.status().as_u16();
    ensure!(
        (200..300).contains(&status),
        "ChatGPT authorization request failed (HTTP {status}); sign in again from /providers"
    );
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("could not read ChatGPT authorization response"))?;
    ensure!(
        bytes.len() as u64 <= limit,
        "ChatGPT {label} response exceeds limits"
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT authorization response"))
}
fn get_json(url: &str, cancel: Arc<AtomicBool>) -> Result<Value> {
    response_json(
        client(cancel)
            .get(url)
            .call()
            .map_err(|_| anyhow::anyhow!("could not reach ChatGPT authorization service"))?,
    )
}
fn token_request(url: &str, fields: &[(&str, &str)], cancel: Arc<AtomicBool>) -> Result<Value> {
    response_json(
        client(cancel)
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(form(fields))
            .map_err(|_| anyhow::anyhow!("could not reach ChatGPT token service"))?,
    )
}
#[cfg(test)]
fn verify_id(
    token: &str,
    keys: &Value,
    audience: &str,
    nonce: Option<&str>,
    subject: Option<&str>,
    time: u64,
) -> Result<(String, String)> {
    let parts = token.split('.').collect::<Vec<_>>();
    ensure!(parts.len() == 3, "invalid ChatGPT ID token");
    let decode_part = |part: &str| {
        URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|_| anyhow::anyhow!("invalid ChatGPT ID token"))
    };
    let header: Value = serde_json::from_slice(&decode_part(parts[0])?)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT ID token header"))?;
    let kid = header["kid"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("ChatGPT ID token has no signing key ID"))?;
    let key = keys["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find(|k| k["kid"] == kid))
        .ok_or_else(|| anyhow::anyhow!("ChatGPT ID token signing key is unknown"))?;
    let signature = decode_part(parts[2])?;
    let signed = format!("{}.{}", parts[0], parts[1]);
    ensure!(
        header.get("crit").is_none(),
        "unsupported critical ChatGPT ID token header"
    );
    let verified = match header["alg"].as_str() {
        Some("RS256") if key["kty"] == "RSA" && key.get("alg").is_none_or(|a| a == "RS256") => {
            let n = decode_part(key["n"].as_str().unwrap_or(""))?;
            let e = decode_part(key["e"].as_str().unwrap_or(""))?;
            ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
                .verify(
                    &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                    signed.as_bytes(),
                    &signature,
                )
                .is_ok()
        }
        Some("ES256")
            if key["kty"] == "EC"
                && key["crv"] == "P-256"
                && key.get("alg").is_none_or(|a| a == "ES256") =>
        {
            let mut public = vec![4];
            public.extend(decode_part(key["x"].as_str().unwrap_or(""))?);
            public.extend(decode_part(key["y"].as_str().unwrap_or(""))?);
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::ECDSA_P256_SHA256_FIXED,
                public,
            )
            .verify(signed.as_bytes(), &signature)
            .is_ok()
        }
        _ => false,
    };
    ensure!(verified, "ChatGPT ID token signature verification failed");
    let claims: Value = serde_json::from_slice(&decode_part(parts[1])?)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT identity claims"))?;
    ensure!(
        claims["iss"] == ISSUER,
        "ChatGPT ID token issuer did not match"
    );
    let aud = &claims["aud"];
    let matches = aud == audience
        || aud
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == audience));
    ensure!(matches, "ChatGPT ID token audience did not match");
    if aud.as_array().is_some_and(|a| a.len() > 1) || claims.get("azp").is_some() {
        ensure!(
            claims["azp"] == audience,
            "ChatGPT ID token authorized party did not match"
        );
    }
    ensure!(
        claims["exp"].as_u64().is_some_and(|exp| exp > time),
        "ChatGPT ID token expired"
    );
    ensure!(
        claims
            .get("nbf")
            .is_none_or(|n| n.as_u64().is_some_and(|n| n <= time + 30)),
        "ChatGPT ID token is not yet valid"
    );
    if let Some(nonce) = nonce {
        ensure!(
            claims["nonce"] == nonce,
            "ChatGPT ID token nonce did not match"
        );
    }
    let sub = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("ChatGPT ID token has no subject"))?;
    if let Some(expected) = subject {
        ensure!(
            sub == expected,
            "ChatGPT sign-in returned a different account; existing credentials preserved"
        );
    }
    Ok((sub.into(), claims["email"].as_str().unwrap_or("").into()))
}
fn codex_claims(access_token: &str) -> Result<Value> {
    let payload = access_token
        .split('.')
        .nth(1)
        .context("ChatGPT access token is not a JWT")?;
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .context("invalid ChatGPT access-token claims")?,
    )
    .context("invalid ChatGPT access-token claims")
}
fn codex_profile(access_token: &str, id_token: &str) -> Result<(String, String, String)> {
    let access = codex_claims(access_token)?;
    let identity = if id_token.is_empty() {
        None
    } else {
        codex_claims(id_token).ok()
    };
    let account_id = access[JWT_AUTH_CLAIM]["chatgpt_account_id"]
        .as_str()
        .or_else(|| {
            identity
                .as_ref()?
                .get(JWT_AUTH_CLAIM)?
                .get("chatgpt_account_id")?
                .as_str()
        })
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .context("ChatGPT token has no Codex account identity")?;
    let email = access["https://api.openai.com/profile"]["email"]
        .as_str()
        .or_else(|| {
            identity
                .as_ref()?
                .get("https://api.openai.com/profile")?
                .get("email")?
                .as_str()
        })
        .or_else(|| access["email"].as_str())
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let subject = account_id.clone();
    Ok((account_id, subject, email))
}
fn codex_account_id(access_token: &str) -> Result<String> {
    codex_profile(access_token, "").map(|(account_id, _, _)| account_id)
}
fn tokens(value: &Value, previous: Option<&Tokens>) -> Result<Tokens> {
    ensure!(
        value["token_type"]
            .as_str()
            .is_none_or(|s| s.eq_ignore_ascii_case("Bearer")),
        "ChatGPT token type is not Bearer"
    );
    let field = |name| {
        value[name].as_str().ok_or_else(|| {
            anyhow::anyhow!("ChatGPT token response is missing required credentials")
        })
    };
    let access = field("access_token")?;
    let refresh = value["refresh_token"]
        .as_str()
        .or_else(|| previous.map(|tokens| tokens.refresh_token.as_str()))
        .ok_or_else(|| anyhow::anyhow!("ChatGPT token response is missing required credentials"))?;
    ascii_token(access)?;
    ascii_token(refresh)?;
    let scopes = value["scope"]
        .as_str()
        .map(|scope| {
            scope
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .or_else(|| previous.map(|tokens| tokens.scopes.clone()))
        .unwrap_or_else(|| SCOPE.split_whitespace().map(str::to_owned).collect());
    ensure!(
        scopes.iter().any(|scope| scope == "openid")
            && scopes.iter().any(|scope| scope == "offline_access"),
        "ChatGPT authorization did not grant identity and refresh access"
    );
    let lifetime = value["expires_in"]
        .as_u64()
        .or_else(|| {
            value["expires_in"]
                .as_f64()
                .filter(|seconds| {
                    seconds.is_finite() && *seconds > 0.0 && *seconds < u64::MAX as f64
                })
                .map(|seconds| seconds.ceil() as u64)
        })
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| anyhow::anyhow!("invalid ChatGPT access-token lifetime"))?;
    let id = value["id_token"]
        .as_str()
        .or_else(|| previous.map(|t| t.id_token.as_str()))
        .unwrap_or("");
    Ok(Tokens {
        access_token: access.into(),
        refresh_token: refresh.into(),
        id_token: id.into(),
        expires_at: now().saturating_add(lifetime),
        scopes,
        earliest_refresh_at: value["earliest_refresh_at"].as_u64().unwrap_or(0),
    })
}
/// Browser authorization occurs outside storage locks and activates an account only after validation.
pub fn login(dir: &Path, selected: Option<&str>, cancel: Arc<AtomicBool>) -> Result<String> {
    login_at(dir, selected, cancel, TOKEN_URL, CALLBACK_PORT, |url| {
        let opened = super::oauth::util::open_browser(url);
        ensure!(
            opened,
            "could not open the system browser for ChatGPT sign-in"
        );
        Ok(())
    })
}
fn login_at(
    dir: &Path,
    selected: Option<&str>,
    cancel: Arc<AtomicBool>,
    token_url: &str,
    callback_port: u16,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<String> {
    let state = load(dir)?;
    let selected_account = selected
        .map(|id| {
            state
                .accounts
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("ChatGPT account no longer exists"))
        })
        .transpose()?;
    let hints = selected_account.filter(|account| account.client_id == CLIENT_ID);
    let listener = if callback_port == 0 {
        TcpListener::bind(("127.0.0.1", 0))
    } else {
        TcpListener::bind(("localhost", callback_port))
    }
    .context("OpenAI Codex callback port is unavailable")?;
    listener.set_nonblocking(true)?;
    let redirect = if callback_port == CALLBACK_PORT {
        REDIRECT_URI.into()
    } else {
        format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr()?.port()
        )
    };
    let attempt = Attempt {
        state: random(),
        verifier: SecretToken::new(random()),
        redirect,
    };
    cancelled(&cancel)?;
    open(&attempt.url(hints))?;
    let (code, _) = receive(&listener, &attempt, &cancel)?;
    let value = token_request(
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code.as_str()),
            ("code_verifier", attempt.verifier.as_str()),
            ("redirect_uri", &attempt.redirect),
        ],
        Arc::clone(&cancel),
    )?;
    let tokens = tokens(&value, None)?;
    let (id, subject, email) = codex_profile(&tokens.access_token, &tokens.id_token)?;
    ensure!(
        hints.is_none_or(|_| selected == Some(id.as_str())),
        "ChatGPT sign-in returned a different workspace; existing credentials preserved"
    );
    cancelled(&cancel)?;
    let _lock = locked_cancellable(&dir.join(FILE), &cancel)?;
    let mut current = load(dir)?;
    cancelled(&cancel)?;
    if let Some(existing) = current.accounts.get(&id) {
        ensure!(
            existing.subject == subject,
            "ChatGPT account identity changed; existing credentials preserved"
        );
    }
    current.accounts.insert(
        id.clone(),
        Account {
            client_id: CLIENT_ID.into(),
            subject,
            email,
            tokens: Some(tokens),
        },
    );
    current.active = Some(id);
    save(dir, &current)?;
    Ok("ChatGPT Codex sign-in validated; choose an available model in /models".into())
}
pub fn account_access(dir: &Path, cancel: Arc<AtomicBool>) -> Result<(String, SecretToken)> {
    account_token_at(dir, cancel, TOKEN_URL)
}

/// Capture the active default once; later global switches never retarget this identity.
pub fn resolve_account(dir: &Path, selected: Option<&str>) -> Result<String> {
    let cancel = AtomicBool::new(false);
    let _lock = locked_cancellable(&dir.join(FILE), &cancel)?;
    let state = load(dir)?;
    let id = selected
        .or(state.active.as_deref())
        .context("Continue with ChatGPT in /providers first")?;
    ensure!(!id.is_empty(), "ChatGPT account_id must not be empty");
    let account = state
        .accounts
        .get(id)
        .context("selected ChatGPT account is missing")?;
    ensure!(
        account.client_id == CLIENT_ID && account.tokens.is_some(),
        "ChatGPT account is signed out or requires migration; sign in from /providers"
    );
    Ok(id.to_owned())
}

pub fn scoped_account_access(
    dir: &Path,
    account_id: &str,
    cancel: Arc<AtomicBool>,
) -> Result<(String, SecretToken)> {
    scoped_account_token_at(dir, Some(account_id), cancel, TOKEN_URL)
}
fn account_token_at(
    dir: &Path,
    cancel: Arc<AtomicBool>,
    url: &str,
) -> Result<(String, SecretToken)> {
    scoped_account_token_at(dir, None, cancel, url)
}

fn scoped_account_token_at(
    dir: &Path,
    selected: Option<&str>,
    cancel: Arc<AtomicBool>,
    url: &str,
) -> Result<(String, SecretToken)> {
    cancelled(&cancel)?;
    let _lock = locked_cancellable(&dir.join(FILE), &cancel)?;
    let mut state = load(dir)?;
    let id = selected
        .map(str::to_owned)
        .or_else(|| state.active.clone())
        .ok_or_else(|| anyhow::anyhow!("Continue with ChatGPT in /providers first"))?;
    // Restrictions are operator-private and authoritative on every dispatch, not a stale UI copy.
    if let Some(bytes) = read_private(&dir.join("config.toml"))? {
        let global: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)
            .context("invalid private global account policy")?;
        if let Some(raw) = global.get("chatgpt_accounts") {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Policy {
                allowed: std::collections::BTreeSet<String>,
            }
            let policy: Policy = raw
                .clone()
                .try_into()
                .context("invalid private ChatGPT account policy")?;
            ensure!(
                policy.allowed.iter().all(|id| !id.is_empty()
                    && id.len() <= 256
                    && id.bytes().all(|b| b.is_ascii_graphic())),
                "invalid private ChatGPT account policy"
            );
            ensure!(
                policy.allowed.contains(&id),
                "ChatGPT account is restricted by private global policy"
            );
        }
    }
    let account = state
        .accounts
        .get_mut(&id)
        .ok_or_else(|| anyhow::anyhow!("selected ChatGPT account is missing"))?;
    ensure!(
        account.client_id == CLIENT_ID,
        "ChatGPT subscription credentials need migration; sign in again from /providers"
    );
    let previous = account
        .tokens
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("ChatGPT account is signed out; sign in from /providers"))?;
    ensure!(
        previous.expires_at > now() || previous.earliest_refresh_at <= now(),
        "ChatGPT token expired before renewal is available; sign in again from /providers"
    );
    if previous.expires_at <= now() + 60 && previous.earliest_refresh_at <= now() {
        let response = token_request(
            url,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", &previous.refresh_token),
            ],
            Arc::clone(&cancel),
        )?;
        let replacement = tokens(&response, Some(previous))?;
        let (refreshed_id, refreshed_subject, _) =
            codex_profile(&replacement.access_token, &replacement.id_token)?;
        ensure!(
            refreshed_id == id && refreshed_subject == account.subject,
            "ChatGPT refresh returned a different workspace; sign in again from /providers"
        );
        account.tokens = Some(replacement);
        save(dir, &state)?;
    }
    let tokens = state.accounts[&id].tokens.as_ref().unwrap();
    ascii_token(&tokens.access_token)?;
    let account_id = codex_account_id(&tokens.access_token)?;
    ensure!(
        account_id == id,
        "ChatGPT refresh returned a different workspace; sign in again from /providers"
    );
    Ok((account_id, SecretToken::new(tokens.access_token.clone())))
}
pub fn sign_out(dir: &Path, id: &str, cancel: Arc<AtomicBool>) -> Result<String> {
    let _lock = locked_cancellable(&dir.join(FILE), &cancel)?;
    let mut state = load(dir)?;
    let account = state
        .accounts
        .get_mut(id)
        .ok_or_else(|| anyhow::anyhow!("ChatGPT account registration no longer exists"))?;
    let revoked = if let Some(tokens) = &account.tokens {
        (|| -> Result<()> {
            let discovery = get_json(
                &format!("{ISSUER}/.well-known/openid-configuration"),
                Arc::clone(&cancel),
            )?;
            let url = discovery["revocation_endpoint"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("revocation endpoint missing"))?;
            ensure!(
                url.starts_with(&format!("{ISSUER}/")),
                "unexpected ChatGPT revocation endpoint"
            );
            let response = client(cancel)
                .post(url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .send(form(&[
                    ("token", &tokens.refresh_token),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", &account.client_id),
                ]))
                .map_err(|_| anyhow::anyhow!("ChatGPT revocation could not be confirmed"))?;
            ensure!(
                response.status().as_u16() == 200,
                "ChatGPT revocation could not be confirmed"
            );
            Ok(())
        })()
        .is_ok()
    } else {
        true
    };
    account.tokens = None;
    if state.active.as_deref() == Some(id) {
        state.active = None;
    }
    save(dir, &state)?;
    Ok(if revoked { "Signed out of ChatGPT; account registration retained for later sign-in." } else { "Signed out locally. Remote revocation was not confirmed; disconnect Dial in ChatGPT Settings if needed." }.into())
}
fn codex_request(
    account_id: &str,
    token: &SecretToken,
    url: &str,
    timeout: u64,
) -> Result<ureq::http::Response<ureq::Body>> {
    super::agent_with(timeout, true, None, None)
        .get(url)
        .header("Authorization", format!("Bearer {}", token.as_str()))
        .header("chatgpt-account-id", account_id)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "dial")
        .header("version", CLIENT_VERSION)
        .header("Accept", "application/json")
        .call()
        .map_err(|_| anyhow::anyhow!("OpenAI Codex model discovery unavailable"))
}
/// Metadata-only discovery, scoped to the selected account, with no stale catalog from an API key.
pub fn discover(
    dir: &Path,
) -> (
    super::catalog::ProviderStatus,
    Vec<super::catalog::ModelEntry>,
) {
    let label = "ChatGPT subscription";
    let outcome = (|| -> Result<(String, Vec<super::catalog::ModelEntry>)> {
        let (id, token) = account_token_at(dir, Arc::new(AtomicBool::new(false)), TOKEN_URL)?;
        let urls = [
            format!("{BASE_URL}/codex/models?client_version={CLIENT_VERSION}"),
            format!("{BASE_URL}/models?client_version={CLIENT_VERSION}"),
        ];
        let mut response = urls
            .iter()
            .find_map(|url| codex_request(&id, &token, url, 5).ok())
            .ok_or_else(|| anyhow::anyhow!("OpenAI Codex model discovery failed"))?;
        let value = limited_json(&mut response, MAX_CATALOG_JSON, "model catalog")?;
        let supplemental = codex_model_metadata();
        let catalog = build_catalog(&value, supplemental.as_ref())?;
        save_catalog(dir, &id, &catalog)?;
        let models = catalog.models;
        let info = accounts(dir)?
            .into_iter()
            .find(|a| a.client_id == id)
            .map(|a| a.label)
            .unwrap_or_default();
        Ok((
            format!("{info}; model catalog only, first completed request verifies access"),
            models,
        ))
    })();
    let (state, detail, models) = match outcome {
        Ok((detail, models)) => ("reachable", detail, models),
        Err(error) => ("auth-required", format!("{error}"), vec![]),
    };
    (
        super::catalog::ProviderStatus {
            id: PROVIDER.into(),
            label: label.into(),
            base_url: Some(BASE_URL.into()),
            state: state.into(),
            detail,
            models: models.len(),
        },
        models,
    )
}
#[cfg(test)]
fn fetch_models(url: &str, token: &str) -> Result<Vec<super::catalog::ModelEntry>> {
    let mut response = super::agent_with(5, true, None, None)
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .map_err(|_| anyhow::anyhow!("ChatGPT model discovery unavailable"))?;
    parse_models(&limited_json(
        &mut response,
        MAX_CATALOG_JSON,
        "model catalog",
    )?)
}
fn parse_models(value: &Value) -> Result<Vec<super::catalog::ModelEntry>> {
    let rows = value["models"]
        .as_array()
        .or_else(|| value["data"].as_array())
        .ok_or_else(|| anyhow::anyhow!("invalid OpenAI Codex model catalog"))?;
    Ok(rows
        .iter()
        .filter(|row| {
            row["visibility"]
                .as_str()
                .is_none_or(|visibility| visibility == "list")
        })
        .take(512)
        .filter_map(|row| {
            let slug = row["slug"]
                .as_str()
                .or_else(|| row["id"].as_str())
                .filter(|s| !s.is_empty())?;
            Some(super::catalog::ModelEntry {
                selection: super::model_config::ModelSelection {
                    provider: PROVIDER.into(),
                    model: slug.into(),
                    base_url: Some(BASE_URL.into()),
                    // context_window is Codex's standard default, not the extended capacity.
                    // Validate independently so a malformed maximum still permits a valid default.
                    context_window: ["max_context_window", "context_window"]
                        .into_iter()
                        .filter_map(|key| row[key].as_u64().and_then(|n| u32::try_from(n).ok()))
                        .filter(|n| (2048..=super::model_config::MAX_CONTEXT_WINDOW).contains(n))
                        .max(),
                    ..Default::default()
                },
                label: row["display_name"].as_str().unwrap_or(slug).into(),
                source: "ChatGPT plan (access unverified until inference)".into(),
                available: true,
                metadata: super::catalog::ModelMetadata {
                    vision: row["input_modalities"]
                        .as_array()
                        .map(|items| items.iter().any(|item| item == "image")),
                    tools: row["supports_parallel_tool_calls"]
                        .as_bool()
                        .filter(|supported| *supported),
                    reasoning: row["supported_reasoning_levels"]
                        .as_array()
                        .map(|items| !items.is_empty()),
                    ..Default::default()
                },
            })
        })
        .collect())
}

fn codex_model_metadata() -> Option<Value> {
    use crate::platform::fs;
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| std::path::PathBuf::from(home).join(".codex"))
        })?;
    let path = home.join("models_cache.json");
    let file =
        fs::open_nofollow_nonblocking(&std::fs::OpenOptions::new().read(true), &path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || !fs::owned_by_current_user(&file).ok()?
        || metadata.len() > MAX_CATALOG_JSON
    {
        return None;
    }
    if metadata.modified().ok()?.elapsed().ok()? > Duration::from_secs(7 * 86400) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CATALOG_JSON + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CATALOG_JSON {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
fn build_catalog(value: &Value, supplemental: Option<&Value>) -> Result<ModelCatalog> {
    use super::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
    let mut models = parse_models(value)?;
    let mut rows = value["models"]
        .as_array()
        .or_else(|| value["data"].as_array())
        .ok_or_else(|| anyhow::anyhow!("invalid OpenAI Codex model catalog"))?
        .clone();
    if let Some(extra) = supplemental {
        for mut entry in parse_models(extra).unwrap_or_default() {
            if models
                .iter()
                .any(|model| model.selection.model == entry.selection.model)
            {
                continue;
            }
            if rows.iter().any(|row| row["slug"] == entry.selection.model) {
                continue;
            }
            entry.source = "Codex catalog (access unverified until inference)".into();
            if let Some(row) = extra["models"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["slug"] == entry.selection.model))
            {
                rows.push(row.clone());
            }
            models.push(entry);
        }
    }
    let mut profiles = BTreeMap::new();
    for model in &models {
        let Some(row) = rows
            .iter()
            .find(|row| row["slug"] == model.selection.model || row["id"] == model.selection.model)
        else {
            continue;
        };
        let efforts = row["supported_reasoning_levels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|level| {
                serde_json::from_value::<ReasoningEffort>(level["effort"].clone()).ok()
            })
            .collect::<Vec<_>>();
        let default =
            serde_json::from_value::<ReasoningEffort>(row["default_reasoning_level"].clone())
                .ok()
                .filter(|effort| efforts.contains(effort));
        let profile = ModelProfile {
            provider: PROVIDER.into(),
            model: model.selection.model.clone(),
            reasoning_effort: default,
            capabilities: ModelCapabilities {
                vision: model.metadata.vision,
                reasoning: if efforts.is_empty() {
                    ReasoningApi::None
                } else {
                    ReasoningApi::OpenaiResponses
                },
                reasoning_efforts: efforts,
                ..Default::default()
            },
            ..Default::default()
        };
        profile.validate()?;
        profiles.insert(model.selection.model.clone(), profile);
    }
    Ok(ModelCatalog { models, profiles })
}

#[cfg(test)]
pub(crate) fn test_catalog(dir: &Path, value: &Value) {
    save_catalog(dir, "oaiapp_test", &build_catalog(value, None).unwrap()).unwrap();
}
#[cfg(test)]
pub(crate) fn test_account(dir: &Path, expired: bool) {
    let _lock = locked(&dir.join(FILE)).unwrap();
    let id = "oaiapp_test";
    let account = Account {
        client_id: CLIENT_ID.into(),
        subject: id.into(),
        email: "test@example.com".into(),
        tokens: Some(Tokens {
            access_token: test_access(id),
            refresh_token: "test-refresh".into(),
            id_token: "test-id".into(),
            expires_at: if expired { 0 } else { now() + 3600 },
            scopes: SCOPE.split_whitespace().map(str::to_owned).collect(),
            earliest_refresh_at: 0,
        }),
    };
    save(
        dir,
        &State {
            active: Some(id.into()),
            accounts: BTreeMap::from([(id.into(), account)]),
        },
    )
    .unwrap();
}
#[cfg(test)]
pub(crate) fn test_access(account_id: &str) -> String {
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode("{}"),
        URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "https://api.openai.com/auth":{"chatgpt_account_id":account_id},
                "https://api.openai.com/profile":{"email":"fixture@example.com"}
            })
            .to_string()
        )
    )
}
#[cfg(test)]
fn access_token_at(dir: &Path, cancel: Arc<AtomicBool>, url: &str) -> Result<SecretToken> {
    account_token_at(dir, cancel, url).map(|(_, token)| token)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{catalog::model_context_window, model_config::ModelSelection};
    use ring::signature::KeyPair;
    use serde_json::json;
    fn attempt() -> Attempt {
        Attempt {
            state: "pending-state".into(),
            verifier: SecretToken::new("test-verifier".into()),
            redirect: REDIRECT_URI.into(),
        }
    }
    fn signed(claims: &Value) -> (String, Value) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs.as_ref(),
            &rng,
        )
        .unwrap();
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(json!({"alg":"ES256","kid":"test"}).to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = key.sign(&rng, input.as_bytes()).unwrap();
        let public = key.public_key().as_ref();
        (
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())),
            json!({"keys":[{"kid":"test","kty":"EC","crv":"P-256","x":URL_SAFE_NO_PAD.encode(&public[1..33]),"y":URL_SAFE_NO_PAD.encode(&public[33..65])}]}),
        )
    }
    fn login_fixture(
        dir: &Path,
        selected: Option<&str>,
        subject: &str,
    ) -> (Result<String>, Vec<String>, String) {
        use std::io::{BufRead, BufReader};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let payload = Arc::new(parking_lot::Mutex::new((Value::Null, Value::Null)));
        let shared = Arc::clone(&payload);
        let job = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..1 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    head.push_str(&line);
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                requests.push(format!("{head}{}", String::from_utf8_lossy(&body)));
                let data = {
                    let p = shared.lock();
                    if index == 0 { p.0.clone() } else { p.1.clone() }
                }
                .to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",data.len()).unwrap();
            }
            requests
        });
        let mut seen_url = String::new();
        let outcome = login_at(
            dir,
            selected,
            Arc::new(AtomicBool::new(false)),
            &format!("{base}/token"),
            0,
            |url| {
                seen_url = url.to_owned();
                let fields = url
                    .split_once('?')
                    .unwrap()
                    .1
                    .split('&')
                    .map(|field| {
                        let (key, value) = field.split_once('=').unwrap();
                        (decode(key).unwrap(), decode(value).unwrap())
                    })
                    .collect::<BTreeMap<_, _>>();
                let account_id = format!("workspace-{subject}");
                let (id_token, keys) = signed(
                    &json!({"iss":ISSUER,"aud":CLIENT_ID,"exp":now()+3600,"sub":subject,"email":"fixture@example.com"}),
                );
                *payload.lock() = (
                    json!({"access_token":test_access(&account_id),"refresh_token":"fixture-refresh","id_token":id_token,"token_type":"Bearer","expires_in":3600,"scope":SCOPE}),
                    keys,
                );
                let redirect = &fields["redirect_uri"];
                let (host, path) = redirect
                    .strip_prefix("http://")
                    .unwrap()
                    .split_once('/')
                    .unwrap();
                let mut stream = TcpStream::connect(host).unwrap();
                write!(
                    stream,
                    "GET /{path}?{} HTTP/1.1\r\nHost: {host}\r\n\r\n",
                    form(&[("state", &fields["state"]), ("code", "fixture-code")])
                )
                .unwrap();
                Ok(())
            },
        );
        (outcome, job.join().unwrap(), seen_url)
    }
    #[test]
    fn catalog_merges_new_codex_models_and_preserves_account_capabilities() {
        let account = json!({"models":[{"slug":"existing","display_name":"Existing","visibility":"list","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},{"slug":"internal","visibility":"hide"}]});
        let codex = json!({"models":[{"slug":"existing","visibility":"list","supported_reasoning_levels":[{"effort":"ultra"}]},
            {"slug":"new-sol","display_name":"New Sol","visibility":"list","input_modalities":["text","image"],"default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"max"},{"effort":"ultra"},{"effort":"future"}]},
            {"slug":"internal","visibility":"list"},{"slug":"hidden","visibility":"hide"}]});
        let catalog = build_catalog(&account, Some(&codex)).unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            catalog.models[1].source,
            "Codex catalog (access unverified until inference)"
        );
        let existing = &catalog.profiles["existing"];
        assert_eq!(existing.capabilities.reasoning_efforts.len(), 2);
        let new = &catalog.profiles["new-sol"];
        assert_eq!(new.capabilities.vision, Some(true));
        assert_eq!(new.capabilities.reasoning_efforts.len(), 3);
        assert_eq!(
            new.capabilities.reasoning,
            super::super::profiles::ReasoningApi::OpenaiResponses
        );
    }
    #[test]
    fn codex_model_discovery_sends_dial_account_headers_and_normalizes_catalog() {
        let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
            200,
            &json!({
                "models":[{"slug":"gpt-codex-test","display_name":"Codex Test","visibility":"list","context_window":128000,"input_modalities":["text","image"]}]
            }),
        )]);
        let token = SecretToken::new(test_access("workspace-1"));
        let url = format!(
            "{}/codex/models?client_version={CLIENT_VERSION}",
            server.url
        );
        let mut response = codex_request("workspace-1", &token, &url, 5).unwrap();
        let models =
            parse_models(&limited_json(&mut response, MAX_CATALOG_JSON, "model catalog").unwrap())
                .unwrap();
        assert_eq!(models[0].selection.model, "gpt-codex-test");
        assert_eq!(models[0].metadata.vision, Some(true));
        let request = server.requests().remove(0);
        assert_eq!(
            crate::llm::fake::request_line(&request),
            format!("GET /codex/models?client_version={CLIENT_VERSION} HTTP/1.1")
        );
        assert_eq!(
            crate::llm::fake::header(&request, "chatgpt-account-id").as_deref(),
            Some("workspace-1")
        );
        assert_eq!(
            crate::llm::fake::header(&request, "OpenAI-Beta").as_deref(),
            Some("responses=experimental")
        );
        assert_eq!(
            crate::llm::fake::header(&request, "originator").as_deref(),
            Some("dial")
        );
    }
    #[test]
    fn reasoning_metadata_is_separate_for_each_signed_in_account() {
        let dir = tempfile::tempdir().unwrap();
        test_account(dir.path(), false);
        test_catalog(
            dir.path(),
            &json!({"models":[{"slug":"account-model","visibility":"list","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]}]}),
        );
        assert!(
            model_profile(dir.path(), "account-model")
                .unwrap()
                .is_some()
        );
        let mut state = load(dir.path()).unwrap();
        let tokens = state.accounts.get_mut("oaiapp_test").unwrap().tokens.take();
        state.accounts.insert(
            "other".into(),
            Account {
                client_id: "other".into(),
                subject: "different".into(),
                email: String::new(),
                tokens,
            },
        );
        state.active = Some("other".into());
        save(dir.path(), &state).unwrap();
        assert!(
            model_profile(dir.path(), "account-model")
                .unwrap()
                .is_none()
        );
        let tokens = state.accounts.get_mut("other").unwrap().tokens.take();
        state.accounts.get_mut("oaiapp_test").unwrap().tokens = tokens;
        state.active = Some("oaiapp_test".into());
        save(dir.path(), &state).unwrap();
        assert!(
            model_profile(dir.path(), "account-model")
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn browser_codex_oauth_activates_account_and_preserves_workspace_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let (result, requests, first_url) = login_fixture(dir.path(), None, "first-person");
        assert!(result.unwrap().contains("Codex sign-in"));
        assert!(first_url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(first_url.contains("codex_cli_simplified_flow=true"));
        assert!(first_url.contains("originator=dial"));
        assert!(requests[0].contains(&format!("client_id={CLIENT_ID}")));
        assert!(
            requests[0].contains("code=fixture-code") && requests[0].contains("code_verifier=")
        );
        assert!(requests[0].contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
        let account_id = "workspace-first-person";
        let state = load(dir.path()).unwrap();
        assert_eq!(state.active.as_deref(), Some(account_id));
        assert_eq!(
            account_access(dir.path(), Arc::new(AtomicBool::new(false)))
                .unwrap()
                .0,
            account_id
        );
        let (result, _, second_url) = login_fixture(dir.path(), Some(account_id), "first-person");
        assert!(result.is_ok());
        assert!(second_url.contains("id_token_hint=") && second_url.contains("login_hint="));
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let (result, _, _) = login_fixture(dir.path(), Some(account_id), "different-person");
        assert!(result.is_err());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    }
    #[test]
    fn codex_authorization_url_matches_pkce_flow() {
        let url = attempt().url(None);
        for text in [
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "response_type=code",
            "code_challenge_method=S256",
            "state=pending-state",
            "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
            "api.connectors.read",
            "api.connectors.invoke",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=dial",
        ] {
            assert!(url.contains(text), "missing {text}: {url}");
        }
        assert!(
            !url.contains("resource=") && !url.contains("nonce=") && !url.contains("test-verifier")
        );
    }
    #[test]
    fn codex_callback_validates_state_and_fixed_client() {
        let attempt = attempt();
        let valid = "/auth/callback?state=pending-state&code=valid-code";
        let (code, client_id) = attempt.callback(valid).unwrap();
        assert_eq!(code.as_str(), "valid-code");
        assert_eq!(client_id, CLIENT_ID);
        for target in [
            "/callback?state=pending-state&code=c",
            "/auth/callback?state=other&code=c",
            "/auth/callback?state=pending-state&code=c&client_id=other-client",
            "/auth/callback?state=pending-state&state=other&code=c",
            "/auth/callback?state=pending-state&error=access_denied&code=c",
        ] {
            assert!(attempt.callback(target).is_err());
        }
    }
    #[test]
    fn identity_signature_issuer_audience_expiry_nonce_and_subject_are_verified() {
        let claims = json!({"iss":ISSUER,"aud":"oaiapp_test","exp":200,"nonce":"n","sub":"s","email":"person@example.com"});
        let (token, keys) = signed(&claims);
        assert_eq!(
            verify_id(&token, &keys, "oaiapp_test", Some("n"), Some("s"), 100).unwrap(),
            ("s".into(), "person@example.com".into())
        );
        for (aud, nonce, subject, time) in [
            ("wrong", "n", "s", 100),
            ("oaiapp_test", "wrong", "s", 100),
            ("oaiapp_test", "n", "wrong", 100),
            ("oaiapp_test", "n", "s", 200),
        ] {
            assert!(verify_id(&token, &keys, aud, Some(nonce), Some(subject), time).is_err());
        }
        let mut wrong = claims.clone();
        wrong["iss"] = json!("https://evil.example");
        let (token, keys) = signed(&wrong);
        assert!(verify_id(&token, &keys, "oaiapp_test", Some("n"), None, 100).is_err());
        let (token, keys) = signed(&claims);
        let mut parts = token.split('.').map(str::to_owned).collect::<Vec<_>>();
        parts[1] = URL_SAFE_NO_PAD.encode(
            json!({"iss":ISSUER,"aud":"oaiapp_test","exp":999,"nonce":"n","sub":"forged"})
                .to_string(),
        );
        assert!(verify_id(&parts.join("."), &keys, "oaiapp_test", Some("n"), None, 100).is_err());
        parts[0] = URL_SAFE_NO_PAD.encode(json!({"alg":"none","kid":"test"}).to_string());
        assert!(verify_id(&parts.join("."), &keys, "oaiapp_test", Some("n"), None, 100).is_err());
    }
    #[test]
    fn plan_scope_and_complete_rotating_token_sets_are_required() {
        let mut response = json!({"access_token":"access","refresh_token":"refresh","id_token":"id","token_type":"Bearer","expires_in":3600,"scope":SCOPE});
        assert!(tokens(&response, None).is_ok());
        response["expires_in"] = json!(31_536_000);
        assert!(
            tokens(&response, None).is_ok(),
            "Codex lifetime is not capped at one day"
        );
        response["expires_in"] = json!(0);
        assert!(
            tokens(&response, None).is_err(),
            "zero-lifetime credentials are invalid"
        );
        response["expires_in"] = json!(3600);
        response["scope"] = json!("openid profile email");
        assert!(tokens(&response, None).is_err());
        response["scope"] = json!(SCOPE);
        response["refresh_token"] = json!(null);
        assert!(tokens(&response, None).is_err());
        response["refresh_token"] = json!("refresh");
        response["access_token"] = json!("with\nnewline");
        assert!(tokens(&response, None).is_err());
    }
    #[test]
    fn resume_refresh_replaces_tokens_atomically_and_reuses_granted_scopes() {
        let dir = tempfile::tempdir().unwrap();
        test_account(dir.path(), true);
        let renewed = test_access("oaiapp_test");
        let server = super::super::fake::serve(vec![super::super::fake::json(
            200,
            &json!({"access_token":renewed.clone(),"refresh_token":"rotated","token_type":"Bearer","expires_in":3600}),
        )]);
        let url = server.url.clone();
        assert_eq!(
            access_token_at(dir.path(), Arc::new(AtomicBool::new(false)), &url)
                .unwrap()
                .as_str(),
            renewed
        );
        assert_eq!(
            access_token_at(dir.path(), Arc::new(AtomicBool::new(false)), &url)
                .unwrap()
                .as_str(),
            renewed,
            "unexpired token makes no second request"
        );
        let state = load(dir.path()).unwrap();
        let t = state.accounts["oaiapp_test"].tokens.as_ref().unwrap();
        assert_eq!(t.refresh_token, "rotated");
        assert_eq!(t.id_token, "test-id");
        assert_eq!(
            crate::platform::fs::mode(&std::fs::metadata(dir.path().join(FILE)).unwrap()) & 0o777,
            0o600
        );
        let requests = server.requests();
        let raw = String::from_utf8_lossy(&requests[0]);
        assert!(
            raw.contains("grant_type=refresh_token")
                && raw.contains(&format!("client_id={CLIENT_ID}"))
                && raw.contains("refresh_token=test-refresh")
        );
        assert!(!raw.contains("scope=") && !raw.contains("dynamic_agent_client"));
    }
    #[test]
    fn concurrent_refreshes_send_a_rotating_token_only_once() {
        let dir = tempfile::tempdir().unwrap();
        test_account(dir.path(), true);
        let renewed = test_access("oaiapp_test");
        let server = super::super::fake::serve(vec![super::super::fake::json(
            200,
            &json!({"access_token":renewed.clone(),"refresh_token":"rotated","token_type":"Bearer","expires_in":3600}),
        )]);
        std::thread::scope(|scope| {
            let jobs = (0..4)
                .map(|_| {
                    let url = &server.url;
                    let dir = dir.path();
                    scope.spawn(move || {
                        access_token_at(dir, Arc::new(AtomicBool::new(false)), url)
                            .unwrap()
                            .as_str()
                            .to_owned()
                    })
                })
                .collect::<Vec<_>>();
            for job in jobs {
                assert_eq!(job.join().unwrap(), renewed);
            }
        });
        assert_eq!(server.requests().len(), 1);
    }
    #[test]
    fn refresh_errors_preserve_credentials_and_do_not_echo_secret_responses() {
        let dir = tempfile::tempdir().unwrap();
        test_account(dir.path(), true);
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let server = super::super::fake::serve(vec![super::super::fake::json(
            400,
            &json!({"error":"invalid_grant","detail":"test-refresh secret"}),
        )]);
        let error =
            access_token_at(dir.path(), Arc::new(AtomicBool::new(false)), &server.url).unwrap_err();
        assert!(!format!("{error:#}").contains("test-refresh"));
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        server.requests();
    }
    #[test]
    fn account_switching_is_isolated_and_corrupt_or_symlink_storage_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        test_account(dir.path(), false);
        {
            let _lock = locked(&dir.path().join(FILE)).unwrap();
            let mut state = load(dir.path()).unwrap();
            state.accounts.insert(
                "oaiapp_two".into(),
                Account {
                    client_id: CLIENT_ID.into(),
                    subject: "second".into(),
                    email: "test@example.com".into(),
                    tokens: None,
                },
            );
            save(dir.path(), &state).unwrap();
        }
        assert!(select_account(dir.path(), "oaiapp_two").is_err());
        assert!(accounts(dir.path()).unwrap()[0].active);
        assert!(
            sign_out(dir.path(), "oaiapp_two", Arc::new(AtomicBool::new(false)))
                .unwrap()
                .contains("Signed out")
        );
        let state = load(dir.path()).unwrap();
        assert_eq!(state.active.as_deref(), Some("oaiapp_test"));
        assert!(state.accounts["oaiapp_test"].tokens.is_some());
        #[cfg(unix)]
        {
            let other = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(dir.path().join(FILE), other.path().join(FILE)).unwrap();
            assert!(accounts(other.path()).is_err());
        }
        std::fs::write(dir.path().join(FILE), "invalid sensitive data").unwrap();
        let err = accounts(dir.path()).unwrap_err();
        assert!(!err.to_string().contains("sensitive"));
    }
    #[test]
    fn catalog_prefers_valid_extended_maximum_over_default_context() {
        let models = parse_models(&json!({"models":[
            {"slug":"gpt-6.1-sol", "context_window":272000, "max_context_window":872000},
            {"slug":"api-size", "context_window":272000, "max_context_window":1050000},
            {"slug":"no-maximum", "context_window":272000},
            {"slug":"invalid-maximum", "context_window":64000, "max_context_window":-1},
            {"slug":"oversized-maximum", "context_window":64000, "max_context_window":4294967296u64},
            {"slug":"bad-default", "context_window":-1, "max_context_window":1000000},
            {"slug":"stale-low-maximum", "context_window":128000, "max_context_window":64000},
            {"slug":"no-valid-window", "context_window":0, "max_context_window":-1}
        ]})).unwrap();
        assert_eq!(
            models
                .iter()
                .map(|entry| entry.selection.context_window)
                .collect::<Vec<_>>(),
            vec![
                Some(872000),
                Some(1050000),
                Some(272000),
                Some(64000),
                Some(64000),
                Some(1000000),
                Some(128000),
                None
            ]
        );
        let home = tempfile::tempdir().unwrap();
        test_account(home.path(), false);
        test_catalog(
            home.path(),
            &json!({"models":[{"slug":"gpt-6.1-sol", "context_window":272000, "max_context_window":872000}]}),
        );
        assert_eq!(
            model_context_window(
                home.path(),
                &ModelSelection {
                    provider: PROVIDER.into(),
                    model: "gpt-6.1-sol".into(),
                    ..Default::default()
                }
            ),
            Some(872000)
        );
        let project = tempfile::tempdir().unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(project.path(), home.path().to_owned())
                .unwrap();
        let auto = crate::config::types::Config::from_table(
            &layout,
            toml::from_str("provider='chatgpt'\nmodel='gpt-6.1-sol'\ncontext_window='auto'\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(auto.context_window, 872000);
        let capped = crate::config::types::Config::from_table(
            &layout,
            toml::from_str("provider='chatgpt'\nmodel='gpt-6.1-sol'\ncloud_context_window=64000\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(capped.context_window, 64000);
    }

    #[test]
    fn account_model_catalog_uses_visibility_slug_and_display_name() {
        let rows=parse_models(&json!({"models":[{"slug":"available-model","display_name":"Friendly","visibility":"list","context_window":131072},{"slug":"hidden","visibility":"hidden"}]})).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].selection.provider, PROVIDER);
        assert_eq!(rows[0].selection.model, "available-model");
        assert_eq!(rows[0].label, "Friendly");
        assert_eq!(rows[0].selection.context_window, Some(131072));
        assert_eq!(
            parse_models(&json!({"data":[{"id":"api-key-model"}]})).unwrap()[0]
                .selection
                .model,
            "api-key-model"
        );
    }
    #[test]
    fn large_model_catalogs_load_without_raising_the_oauth_response_limit() {
        use super::super::fake;
        let catalog = json!({"models":[{"slug":"account-model","display_name":"Account Model","visibility":"list","context_window":131072,"base_instructions":"x".repeat(MAX_JSON as usize+1024)}]});
        let server = fake::serve(vec![fake::json(200, &catalog)]);
        let models = fetch_models(&server.url, "fixture-token").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].selection.model, "account-model");
        let requests = server.requests();
        assert_eq!(
            fake::header(&requests[0], "authorization").as_deref(),
            Some("Bearer fixture-token")
        );
        let server = fake::serve(vec![fake::json(200, &catalog)]);
        let error = get_json(&server.url, Arc::new(AtomicBool::new(false))).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("authorization response exceeds limits")
        );
        server.requests();
        let oversized = json!({"models":[],"instructions":"x".repeat(MAX_CATALOG_JSON as usize)});
        let server = fake::serve(vec![fake::json(200, &oversized)]);
        assert!(
            fetch_models(&server.url, "fixture-token")
                .unwrap_err()
                .to_string()
                .contains("model catalog response exceeds limits")
        );
        server.requests();
    }
    #[test]
    fn unrelated_and_invalid_callbacks_do_not_consume_the_browser_attempt() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let job =
            std::thread::spawn(move || receive(&listener, &attempt(), &AtomicBool::new(false)));
        for target in [
            "/favicon.ico",
            "/auth/callback?state=other&code=fixture-code",
            "/auth/callback?state=pending-state&state=other&code=fixture-code",
            "/auth/callback?state=pending-state&code=fixture-code&client_id=other-client",
        ] {
            let mut browser = TcpStream::connect(addr).unwrap();
            browser
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write!(
                browser,
                "GET {target} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: fixture-browser\r\n\r\n"
            )
            .unwrap();
            let mut reply = String::new();
            browser.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 400"));
        }
        let mut browser = TcpStream::connect(addr).unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(browser,"GET /auth/callback?state=pending-state&code=fixture-code HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: fixture-browser\r\nAccept: text/html\r\nCookie: {}\r\n\r\n","x".repeat(8192)).unwrap();
        let mut reply = String::new();
        browser.read_to_string(&mut reply).unwrap();
        assert!(reply.contains("200 OK"));
        assert_eq!(job.join().unwrap().unwrap().1, CLIENT_ID);
    }
    #[test]
    fn matching_denied_authorization_ends_the_attempt() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let job =
            std::thread::spawn(move || receive(&listener, &attempt(), &AtomicBool::new(false)));
        let mut browser = TcpStream::connect(addr).unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(browser,"GET /auth/callback?state=pending-state&error=access_denied HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
        let mut reply = String::new();
        browser.read_to_string(&mut reply).unwrap();
        assert!(
            job.join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("declined")
        );
        assert!(reply.contains("400 Bad Request"));
    }
    #[test]
    fn idle_browser_preconnection_does_not_end_login() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let job =
            std::thread::spawn(move || receive(&listener, &attempt(), &AtomicBool::new(false)));
        let idle = TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(650));
        drop(idle);
        let result = (|| -> Result<String> {
            let mut browser = TcpStream::connect(addr)?;
            browser.set_read_timeout(Some(Duration::from_secs(3)))?;
            write!(
                browser,
                "GET /auth/callback?state=pending-state&code=fixture-code HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: fixture-browser\r\nAccept: text/html\r\n\r\n"
            )?;
            let mut reply = String::new();
            browser.read_to_string(&mut reply)?;
            Ok(reply)
        })();
        let received = job.join().unwrap();
        assert!(
            received.is_ok(),
            "an idle browser connection must not stop the callback listener"
        );
        assert!(result.unwrap().contains("200 OK"));
    }
    #[test]
    fn waiting_for_the_browser_is_cancellable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _idle = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&cancel);
        let job = std::thread::spawn(move || receive(&listener, &attempt(), &worker));
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        cancel.store(true, Ordering::SeqCst);
        assert!(
            job.join()
                .unwrap()
                .unwrap_err()
                .downcast_ref::<super::super::Interrupted>()
                .is_some()
        );
        assert!(start.elapsed() < Duration::from_millis(200));
    }
}
