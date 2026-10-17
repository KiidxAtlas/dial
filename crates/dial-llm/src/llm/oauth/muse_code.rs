//! Muse Code (Meta): device-code sign-in followed by a minted API key. The Meta account token is the grant's
//! `access`; the key minted from it at `KEY_URL` is kept in `extra["api_key"]` and is what
//! inference uses. Neither the grant nor the key expires, and there is no refresh grant.

use std::sync::{Arc, atomic::AtomicBool};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::auth::SecretToken;
use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};

pub(crate) const CLIENT_ID: &str = "1031625952748946";
pub(crate) const KEY_URL: &str = "https://api.meta.ai/muse-code/key";
pub(crate) const API_VERSION: &str = "1.0.0";
const DEVICE_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const BASE_URL: &str = "https://api.meta.ai/v1";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const KEY_TIMEOUT_SECS: u64 = 20;
const HEADERS: [(&str, &str); 2] = [
    ("Accept", "application/json"),
    ("x-api-version", API_VERSION),
];

pub const FLOW: Flow = Flow {
    id: "muse-code",
    name: "Muse Code",
    base_url: BASE_URL,
    protocol: ApiProtocol::Responses,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: None,
    access,
};

/// Device-authorization, token, and key endpoints; injectable so tests can use loopback.
struct Endpoints<'a> {
    device: &'a str,
    token: &'a str,
    key: &'a str,
}

const LIVE: Endpoints<'static> = Endpoints {
    device: DEVICE_URL,
    token: TOKEN_URL,
    key: KEY_URL,
};

fn login(login: &Login<'_>) -> Result<Credentials> {
    sign_in(
        &login.cancel,
        &|url: &str, instructions: Option<&str>| login.open(url, instructions),
        &LIVE,
    )
}

/// Run the device flow: show the code, poll for the account token, then mint the key.
fn sign_in(
    cancel: &Arc<AtomicBool>,
    open: &dyn Fn(&str, Option<&str>),
    endpoints: &Endpoints<'_>,
) -> Result<Credentials> {
    let device = util::request(
        "POST",
        endpoints.device,
        &HEADERS,
        util::Body::Form(&[("client_id", CLIENT_ID)]),
        cancel,
    )?
    .require_ok("Muse Code device authorization")?
    .json()?;
    let user_code =
        field(&device, "user_code").context("Muse Code device response is missing user_code")?;
    let device_code = field(&device, "device_code")
        .context("Muse Code device response is missing device_code")?;
    let verification = field(&device, "verification_uri_complete")
        .or_else(|| field(&device, "verification_uri"))
        .context("Muse Code device response is missing verification_uri")?;
    open(&verification, Some(&format!("Enter code: {user_code}")));

    let tokens = util::poll_device(
        device["interval"].as_u64(),
        device["expires_in"].as_u64(),
        cancel,
        || {
            let response = util::request(
                "POST",
                endpoints.token,
                &HEADERS,
                util::Body::Form(&[
                    ("grant_type", DEVICE_GRANT),
                    ("client_id", CLIENT_ID),
                    ("device_code", device_code.as_str()),
                ]),
                cancel,
            )?;
            Ok(util::classify_device_response(&response))
        },
    )?;
    let account_token = field(&tokens, "access_token")
        .context("Muse Code token response is missing access_token")?;
    let refresh = field(&tokens, "refresh_token").unwrap_or_default();

    let key = request_key(endpoints.key, &account_token, true, cancel)?;
    credentials(account_token, refresh, key)
}

/// Response of `POST {KEY_URL}`. Deliberately not `Debug`: it carries the minted key.
#[derive(Default, Deserialize)]
#[serde(default)]
pub(crate) struct KeyResponse {
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub is_subs_active: Option<bool>,
    pub action_url: Option<String>,
    pub require_payment_action_url: Option<String>,
    pub require_payment: Option<bool>,
    /// Subscription usage as reported by the service; interpreted by the usage reader.
    pub subs_usage: Option<Value>,
}

/// Exchange a Meta account token for the Muse Code key. Redirects are not followed; the error
/// names only the status, never the response body.
pub(crate) fn request_key(
    url: &str,
    account_token: &str,
    onboard: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<KeyResponse> {
    let auth = format!("Bearer {account_token}");
    let body = if onboard {
        json!({"onboard": true})
    } else {
        json!({})
    };
    let headers = [
        ("Accept", "application/json"),
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
        ("x-api-version", API_VERSION),
    ];
    let response = util::request_with_timeout(
        "POST",
        url,
        &headers,
        util::Body::Json(&body),
        cancel,
        KEY_TIMEOUT_SECS,
    )?;
    if !response.ok() {
        bail!("Muse Code key exchange failed: {}", response.status);
    }
    let value = response
        .json()
        .map_err(|_| anyhow!("Muse Code key exchange returned invalid JSON"))?;
    parse_key_response(value)
}

/// Parse a key response body (pure; used by the exchange and by fixture tests).
pub(crate) fn parse_key_response(value: Value) -> Result<KeyResponse> {
    serde_json::from_value(value).context("Muse Code key response is malformed")
}

/// Turn the account token and key response into the stored grant, applying these rules:
/// an inactive subscription, a payment requirement, a missing key, and a missing identity
/// each fail with a distinct error.
fn credentials(account_token: String, refresh: String, key: KeyResponse) -> Result<Credentials> {
    if key.is_subs_active == Some(false) {
        bail!("invalid_grant: Muse Code subscription is inactive");
    }
    let api_key = key
        .api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    if api_key.is_empty() {
        let action = key
            .require_payment_action_url
            .or(key.action_url)
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty());
        match action {
            Some(url) => bail!("Muse Code subscription is required: {url}"),
            None if key.require_payment == Some(true) => {
                bail!("Muse Code subscription is required")
            }
            None => bail!("Muse Code key response is missing api_key"),
        }
    }
    let email = key
        .user_email
        .as_deref()
        .map(|email| email.trim().to_lowercase())
        .filter(|email| !email.is_empty());
    let user_id = key
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let account_id = user_id
        .or_else(|| email.clone())
        .context("Muse Code key response is missing a stable account identity")?;

    let mut extra = serde_json::Map::new();
    extra.insert("api_key".to_owned(), Value::String(api_key));
    Ok(Credentials {
        access: account_token,
        refresh,
        expires_at_ms: util::NEVER_EXPIRES,
        email,
        account_id: Some(account_id),
        extra,
        ..Default::default()
    })
}

/// Requests use the minted key as the bearer, with the API version header.
fn access(credentials: &Credentials) -> Result<Access> {
    let key = credentials
        .extra_str("api_key")
        .filter(|key| !key.is_empty())
        .context("Muse Code credential has no API key; sign in again")?;
    ensure!(
        key.bytes().all(|b| b.is_ascii_graphic()),
        "Muse Code credential has an invalid API key; sign in again"
    );
    Ok(Access {
        token: SecretToken::new(key.to_owned()),
        base_url: None,
        headers: vec![("x-api-version".to_owned(), API_VERSION.to_owned())],
        project_id: None,
        account_id: credentials.account_id.clone(),
    })
}

/// A trimmed, non-empty string field.
fn field(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests;
