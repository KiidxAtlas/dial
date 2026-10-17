//! OAuth sign-in for remote MCP servers: protected-resource metadata (RFC 9728) found through the
//! 401 `WWW-Authenticate` challenge, authorization-server metadata (RFC 8414, OpenID fallback),
//! dynamic client registration (RFC 7591) unless `oauth.client_id` is set, PKCE S256 (RFC 7636)
//! and resource indicators (RFC 8707). Grants are stored per server in the private user
//! directory and refreshed with their refresh token. [`login`] is the `/mcp login <server>` entry.

use crate::config::settings_store::{locked, write_private};
use crate::config::types::McpServerConfig;
use crate::llm::oauth::util::{self, Body, Callback, Response};
use crate::llm::oauth::{Interaction, Login};
use anyhow::{Context, Result, bail, ensure};
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Seconds shaved off every access token's lifetime so requests never race its expiry.
const EXPIRY_SKEW_SECS: u64 = 30;

/// One server's stored grant. The client secret itself is never stored, only the variable name.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Grant {
    access: String,
    refresh: String,
    /// Unix milliseconds; [`util::NEVER_EXPIRES`] when the server did not state a lifetime.
    expires_at_ms: u64,
    token_endpoint: String,
    client_id: String,
    client_secret_env: Option<String>,
    resource: String,
}

/// The stored OAuth grant of one remote server, shared by its requests and refreshed in place.
pub struct TokenSource {
    path: PathBuf,
    grant: Mutex<Option<Grant>>,
    /// Held across a refresh request so concurrent refreshes do not spend one rotating refresh
    /// token twice.
    refreshing: Mutex<()>,
}

fn grant_path(user_dir: &Path, server: &str) -> PathBuf {
    user_dir.join(format!("mcp-oauth-{}.json", util::encode(server)))
}

/// Deletes the stored grant of `server` (`/mcp logout`). `Ok(false)` when no grant was stored.
/// Live sessions keep their in-memory token until they reconnect.
pub fn logout(user_dir: &Path, server: &str) -> Result<bool> {
    let path = grant_path(user_dir, server);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("removing MCP grant {}", path.display())),
    }
}

fn lock(grant: &Mutex<Option<Grant>>) -> MutexGuard<'_, Option<Grant>> {
    grant.lock()
}

impl TokenSource {
    /// No grant yet; a grant obtained later is stored at `path`.
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            grant: Mutex::new(None),
            refreshing: Mutex::new(()),
        }
    }

    /// The grant stored for `server`, or none when it never signed in. Without a user directory
    /// nothing can be stored, so the source stays empty.
    pub fn load(user_dir: &Path, server: &str) -> Result<Self> {
        if user_dir.as_os_str().is_empty() {
            return Ok(Self::empty(PathBuf::new()));
        }
        let path = grant_path(user_dir, server);
        let grant = match crate::platform::atomic::read_private(&path)? {
            Some(bytes) => Some(serde_json::from_slice::<Grant>(&bytes).with_context(|| {
                format!("stored OAuth grant for MCP server `{server}` is corrupt; sign in again")
            })?),
            None => None,
        };
        Ok(Self {
            path,
            grant: Mutex::new(grant),
            refreshing: Mutex::new(()),
        })
    }

    /// The access token to send now. An expired grant with a refresh token is refreshed first,
    /// so the request is not sent unauthenticated. None when no usable token exists.
    pub fn bearer(&self) -> Option<String> {
        let grant = lock(&self.grant).clone()?;
        if util::now_ms() < grant.expires_at_ms {
            return Some(grant.access);
        }
        self.refresh(&grant.access)
            .then(|| lock(&self.grant).as_ref().map(|fresh| fresh.access.clone()))
            .flatten()
    }

    /// The stored access token as it is, without refreshing. For best-effort cleanup that runs on
    /// drop, where a network refresh would block the caller.
    pub fn stored_access(&self) -> Option<String> {
        lock(&self.grant).as_ref().map(|grant| grant.access.clone())
    }

    /// Refreshes the grant after the server rejected `stale`, the access token that was sent.
    /// Refresh tokens rotate, so refreshes are serialized: when the stored token is no longer
    /// `stale`, another request already refreshed it and that result is reused. False when there
    /// is no refresh token or the token endpoint refuses it; the stored grant is then unchanged.
    pub fn refresh(&self, stale: &str) -> bool {
        let _turn = self.refreshing.lock();
        let Some(grant) = lock(&self.grant).clone() else {
            return false;
        };
        if grant.access != stale {
            return true;
        }
        if grant.refresh.is_empty() {
            return false;
        }
        let secret = grant
            .client_secret_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok());
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", grant.refresh.as_str()),
            ("client_id", grant.client_id.as_str()),
            ("resource", grant.resource.as_str()),
        ];
        if let Some(secret) = secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let Ok(response) = util::request(
            "POST",
            &grant.token_endpoint,
            &[],
            Body::Form(&form[..]),
            &cancel,
        ) else {
            return false;
        };
        let Ok(fresh) = token_grant(&response, &grant) else {
            return false;
        };
        // The fresh grant is usable now even if writing it to disk failed; that failure only
        // means the next start asks for a fresh sign-in, so it does not fail this request.
        let _ = persist(&self.path, &fresh);
        *lock(&self.grant) = Some(fresh);
        true
    }

    /// The protected-resource metadata URL named by a `WWW-Authenticate` challenge, and the
    /// resource it protects.
    pub fn discover_metadata(resource: &str, challenge: &str) -> Result<(String, String)> {
        let Some(metadata) = challenge_param(challenge, "resource_metadata") else {
            bail!("OAuth challenge for {resource:?} does not contain resource_metadata")
        };
        Ok((metadata, resource.to_owned()))
    }
}

/// A `key="value"` or `key=value` parameter of a `WWW-Authenticate` header.
fn challenge_param(challenge: &str, key: &str) -> Option<String> {
    let start = challenge.find(&format!("{key}="))? + key.len() + 1;
    let rest = challenge[start..].trim_start();
    match rest.strip_prefix('"') {
        Some(quoted) => Some(quoted.split('"').next()?.to_owned()),
        None => {
            Some(rest.split([',', ' ', '\t']).next()?.to_owned()).filter(|value| !value.is_empty())
        }
    }
}

/// Signs `name` in to the remote server `server` (its `[mcp.<name>]` entry) and stores the grant
/// under `user_dir`. Opens the browser through `ui`, waits for the loopback redirect (or `cancel`),
/// and returns a status line. The new grant applies when the server next connects.
pub fn login(
    user_dir: &Path,
    name: &str,
    server: &McpServerConfig,
    ui: &dyn Interaction,
    cancel: Arc<AtomicBool>,
) -> Result<String> {
    ensure!(
        !user_dir.as_os_str().is_empty(),
        "no user directory is configured, so the MCP sign-in for `{name}` cannot be stored"
    );
    let Some(url) = server.url.as_deref() else {
        bail!("MCP server `{name}` is a local program; only remote servers sign in with OAuth")
    };
    ensure!(
        server.bearer_env.is_none()
            && !server
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization")),
        "MCP server `{name}` uses a fixed credential (bearer_env or an Authorization header); remove it to sign in with OAuth"
    );
    let Some(challenge) = probe(url, &cancel)? else {
        return Ok(format!("MCP server `{name}` does not require sign-in"));
    };

    let callback = Callback::bind("127.0.0.1", server.oauth.callback_port, CALLBACK_PATH)?;
    let redirect = callback.redirect_uri();
    let metadata_url = match TokenSource::discover_metadata(url, &challenge) {
        Ok((metadata_url, _)) => metadata_url,
        Err(_) => well_known(url, "oauth-protected-resource")?,
    };
    let resource = get_json(&metadata_url, &cancel)?;
    let issuer = resource
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .context("protected resource metadata names no authorization server")?;
    let metadata = authorization_metadata(issuer, &cancel)?;
    let field = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("authorization server metadata has no {key}"))
    };
    let authorize = field("authorization_endpoint")?;
    let token_endpoint = field("token_endpoint")?;
    if let Some(methods) = metadata
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
    {
        ensure!(
            methods.iter().any(|m| m == "S256"),
            "authorization server does not support PKCE S256"
        );
    }

    let client_id = match server
        .oauth
        .client_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_owned(),
        None => register(
            metadata.get("registration_endpoint"),
            &redirect,
            name,
            &cancel,
        )?,
    };
    let scope = server
        .oauth
        .scopes
        .clone()
        .filter(|scope| !scope.is_empty())
        .or_else(|| {
            resource
                .get("scopes_supported")
                .and_then(Value::as_array)
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
        })
        .unwrap_or_default();

    let pkce = util::pkce();
    let state = util::random_urlsafe(16);
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("resource", url),
    ];
    if !scope.is_empty() {
        params.push(("scope", scope.as_str()));
    }
    let auth_url = util::with_query(&authorize, &params);
    Login {
        ui,
        cancel: cancel.clone(),
    }
    .open(
        &auth_url,
        Some(&format!(
            "Sign in to MCP server `{name}`, then return here."
        )),
    );
    let fields = callback.wait(Some(&state), &cancel, LOGIN_TIMEOUT)?;
    if let Some(error) = fields.get("error") {
        bail!("sign-in was declined: {error}");
    }
    let code = fields
        .get("code")
        .context("the sign-in redirect carried no authorization code")?;

    let secret = server
        .oauth
        .client_secret_env
        .as_deref()
        .map(|var| {
            std::env::var(var).with_context(|| {
                format!("environment variable {var} (client_secret_env) is not set")
            })
        })
        .transpose()?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", pkce.verifier.as_str()),
        ("resource", url),
    ];
    if let Some(secret) = secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let response = util::request("POST", &token_endpoint, &[], Body::Form(&form[..]), &cancel)?;
    let base = Grant {
        token_endpoint,
        client_id,
        client_secret_env: server.oauth.client_secret_env.clone(),
        resource: url.to_owned(),
        ..Grant::default()
    };
    let grant = token_grant(&response, &base)?;
    persist(&grant_path(user_dir, name), &grant)?;
    Ok(format!(
        "MCP server `{name}` signed in; the new grant applies when it next connects"
    ))
}

/// Unauthenticated `initialize`: the `WWW-Authenticate` challenge of a 401, or `None` when the
/// server does not ask for sign-in.
fn probe(url: &str, cancel: &Arc<AtomicBool>) -> Result<Option<String>> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "dial", "version": env!("CARGO_PKG_VERSION")},
        },
    });
    let response = util::request(
        "POST",
        url,
        &[("Accept", "application/json, text/event-stream")],
        Body::Json(&body),
        cancel,
    )?;
    if response.status != 401 {
        return Ok(None);
    }
    Ok(Some(
        response.header("www-authenticate").unwrap_or("").to_owned(),
    ))
}

fn get_json(url: &str, cancel: &Arc<AtomicBool>) -> Result<Value> {
    let response = util::request(
        "GET",
        url,
        &[("Accept", "application/json")],
        Body::Empty,
        cancel,
    )?;
    ensure!(
        response.ok(),
        "OAuth metadata request failed (HTTP {})",
        response.status
    );
    response.json()
}

/// Authorization-server metadata: RFC 8414 first, then the OpenID configuration document.
fn authorization_metadata(issuer: &str, cancel: &Arc<AtomicBool>) -> Result<Value> {
    let issuer = issuer.trim_end_matches('/');
    let candidates = [
        well_known(issuer, "oauth-authorization-server")?,
        format!("{issuer}/.well-known/openid-configuration"),
    ];
    for candidate in &candidates {
        if let Ok(metadata) = get_json(candidate, cancel) {
            return Ok(metadata);
        }
    }
    bail!("no authorization server metadata found for {issuer}")
}

/// RFC 8615 well-known URI for `url`: the origin, `/.well-known/<suffix>`, then the URL's path.
fn well_known(url: &str, suffix: &str) -> Result<String> {
    let uri: ureq::http::Uri = url.parse().context("invalid MCP server URL")?;
    let scheme = uri.scheme_str().context("MCP server URL has no scheme")?;
    let authority = uri.authority().context("MCP server URL has no host")?;
    let path = uri.path().trim_end_matches('/');
    Ok(format!("{scheme}://{authority}/.well-known/{suffix}{path}"))
}

/// RFC 7591 dynamic client registration for a public client.
fn register(
    endpoint: Option<&Value>,
    redirect: &str,
    name: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<String> {
    let endpoint = endpoint
        .and_then(Value::as_str)
        .context("the server has no client registration endpoint; set oauth.client_id")?;
    let body = json!({
        "client_name": format!("dial ({name})"),
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = util::request("POST", endpoint, &[], Body::Json(&body), cancel)?;
    ensure!(
        response.ok(),
        "client registration refused (HTTP {})",
        response.status
    );
    response
        .json()?
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .context("client registration response has no client_id")
}

/// Builds the grant from a token response, keeping `base`'s identity fields. A refresh token the
/// server omits is kept from `base`.
fn token_grant(response: &Response, base: &Grant) -> Result<Grant> {
    ensure!(
        response.ok(),
        "token endpoint refused the request (HTTP {})",
        response.status
    );
    let body = response.json()?;
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .context("token response has no access_token")?;
    let expires_at_ms = match body.get("expires_in").and_then(Value::as_u64) {
        // Short lifetimes keep at least half their span; a flat skew would make them unusable.
        Some(secs) => {
            let skew = EXPIRY_SKEW_SECS.min(secs / 2);
            util::now_ms().saturating_add((secs - skew).saturating_mul(1000))
        }
        None => util::NEVER_EXPIRES,
    };
    Ok(Grant {
        access: access.to_owned(),
        refresh: body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| base.refresh.clone()),
        expires_at_ms,
        ..base.clone()
    })
}

/// Writes the grant under the per-file lock with private permissions.
fn persist(path: &Path, grant: &Grant) -> Result<()> {
    let guard = locked(path)?;
    write_private(path, &serde_json::to_vec(grant)?)?;
    drop(guard);
    Ok(())
}
