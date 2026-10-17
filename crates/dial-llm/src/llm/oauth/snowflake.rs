//! Snowflake Cortex sign-in: OAuth authorization-code flow with PKCE against the account's own
//! `/oauth/authorize` and `/oauth/token-request` endpoints.

use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const FLOW: Flow = Flow {
    id: "snowflake",
    name: "Snowflake Cortex",
    base_url: "https://snowflake-account.invalid/api/v2/cortex",
    protocol: ApiProtocol::Anthropic,
    needs_terminal: true,
    subscription: false,
    login,
    refresh: Some(refresh),
    access,
};

const CLIENT_ID: &str = "LOCAL_APPLICATION";
const CALLBACK_HOST: &str = "127.0.0.1";
/// Preferred loopback port registered for Snowflake's local-app integration.
const CALLBACK_PORT: u16 = 54551;
const CALLBACK_PATH: &str = "/";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const INSTRUCTIONS: &str = "Sign in to Snowflake in your browser and approve access for Dial.";
const INVALID_ACCOUNT: &str =
    "Paste your Snowflake account identifier (orgname-accountname) or account URL";
const INVALID_TOKEN_RESPONSE: &str = "Snowflake returned an invalid token response";
const SNOWFLAKE_SUFFIX: &str = ".snowflakecomputing.com";

fn login(l: &Login<'_>) -> Result<Credentials> {
    let account = l.ui.prompt(
        "Enter your Snowflake account identifier or URL",
        Some("myorg-myaccount"),
        true,
    )?;
    ensure!(!l.cancel.load(Ordering::SeqCst), "sign-in cancelled");
    let account_url = normalize_account_url(&account)?;
    let callback = bind_callback(l)?;
    let redirect = callback.redirect_uri();
    let pkce = util::pkce();
    let state = util::random_urlsafe(24);
    let url = authorize_url(&account_url, &redirect, &state, &pkce.challenge);
    l.open(&url, Some(INSTRUCTIONS));
    let fields = callback.wait(Some(&state), &l.cancel, LOGIN_TIMEOUT)?;
    let code = fields
        .get("code")
        .context("Snowflake callback omitted authorization code")?;
    let tokens = fetch_tokens(
        &account_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
            ("client_id", CLIENT_ID),
        ],
        "exchange",
        &l.cancel,
    )?;
    Ok(Credentials {
        access: tokens.access,
        refresh: tokens.refresh.unwrap_or_default(),
        expires_at_ms: tokens.expires_at_ms,
        enterprise_url: Some(account_url),
        ..Credentials::default()
    })
}

/// Prefer the registered port; fall back to a free port when another process holds it.
fn bind_callback(l: &Login<'_>) -> Result<util::Callback> {
    match util::Callback::bind(CALLBACK_HOST, CALLBACK_PORT, CALLBACK_PATH) {
        Ok(callback) => Ok(callback),
        Err(_) => {
            let callback = util::Callback::bind(CALLBACK_HOST, 0, CALLBACK_PATH)?;
            l.ui.progress(&format!(
                "Preferred port {CALLBACK_PORT} unavailable, using port {}",
                callback.port()
            ));
            Ok(callback)
        }
    }
}

fn authorize_url(account_url: &str, redirect_uri: &str, state: &str, challenge: &str) -> String {
    util::with_query(
        &format!("{account_url}/oauth/authorize"),
        &[
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", "refresh_token"),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    )
}

struct Tokens {
    access: String,
    refresh: Option<String>,
    expires_at_ms: u64,
}

/// POST a form to the account's token endpoint and validate the response.
fn fetch_tokens(
    account_url: &str,
    form: &[(&str, &str)],
    operation: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<Tokens> {
    let response = util::request(
        "POST",
        &format!("{account_url}/oauth/token-request"),
        &[],
        util::Body::Form(form),
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Snowflake token {operation} failed ({})",
        response.status
    );
    let body: Value = response.json().context(INVALID_TOKEN_RESPONSE)?;
    parse_tokens(&body, util::now_ms())
}

fn parse_tokens(body: &Value, now_ms: u64) -> Result<Tokens> {
    let access = body["access_token"]
        .as_str()
        .filter(|token| !token.trim().is_empty());
    let expires_in = body["expires_in"]
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0);
    let refresh = match body.get("refresh_token") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|token| !token.trim().is_empty())
                .with_context(|| INVALID_TOKEN_RESPONSE.to_owned())?
                .to_owned(),
        ),
    };
    let (Some(access), Some(expires_in)) = (access, expires_in) else {
        bail!("{}", INVALID_TOKEN_RESPONSE);
    };
    let lifetime_ms = expires_in * 1000.0;
    ensure!(
        lifetime_ms.is_finite() && lifetime_ms > 0.0,
        "Snowflake returned an invalid token lifetime"
    );
    Ok(Tokens {
        access: access.to_owned(),
        refresh,
        expires_at_ms: now_ms.saturating_add(lifetime_ms as u64),
    })
}

fn refresh(c: &Credentials, cancel: &Arc<AtomicBool>) -> Result<Credentials> {
    if c.refresh.trim().is_empty() {
        // Without a refresh token the shared refresh skew would discard a still-valid token.
        if util::now_ms() < c.expires_at_ms {
            return Ok(c.clone());
        }
        bail!(
            "Snowflake did not issue a refresh token; run /login snowflake again or use SNOWFLAKE_ACCOUNT and SNOWFLAKE_PAT"
        );
    }
    let account_url = c
        .enterprise_url
        .as_deref()
        .and_then(|url| normalize_account_url(url).ok())
        .context("Invalid Snowflake account; run /login snowflake again")?;
    let tokens = fetch_tokens(
        &account_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", c.refresh.as_str()),
            ("client_id", CLIENT_ID),
        ],
        "refresh",
        cancel,
    )?;
    Ok(Credentials {
        access: tokens.access,
        expires_at_ms: tokens.expires_at_ms,
        refresh: tokens.refresh.unwrap_or_else(|| c.refresh.clone()),
        ..c.clone()
    })
}

fn access(c: &Credentials) -> Result<Access> {
    let url = c
        .enterprise_url
        .as_deref()
        .context("Snowflake credentials are missing the account URL; run /login snowflake again")?;
    let account_url = normalize_account_url(url)
        .context("Invalid Snowflake account; run /login snowflake again")?;
    let mut access = super::bearer(c)?;
    access.base_url = Some(format!("{account_url}/api/v2/cortex"));
    Ok(access)
}

/// Normalize an account identifier, Snowflake URL, or Snowsight link to `https://<host>`.
/// Only `*.snowflakecomputing.com` hosts are accepted so credentials never reach arbitrary hosts.
pub fn normalize_account_url(input: &str) -> Result<String> {
    let value = input.trim();
    ensure!(
        !value.is_empty(),
        "Snowflake account is required: run /login snowflake or set SNOWFLAKE_ACCOUNT"
    );
    let scheme = url_scheme(value);
    if let Some(scheme) = scheme {
        ensure!(
            scheme.eq_ignore_ascii_case("https"),
            "Snowflake account URL must use https"
        );
    }
    ensure!(!value.contains('\\'), INVALID_ACCOUNT);

    let host = if let Some(scheme) = scheme {
        let rest = &value[scheme.len() + "://".len()..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        ensure!(!authority.is_empty(), INVALID_ACCOUNT);
        let host = normalize_host(authority);
        if host == "app.snowflake.com" {
            snowsight_host(path)?
        } else {
            host
        }
    } else {
        ensure!(!value.contains(['/', '?', '#']), INVALID_ACCOUNT);
        let mut host = normalize_host(value);
        if !host.ends_with(SNOWFLAKE_SUFFIX) && !host.ends_with(".snowflakecomputing.cn") {
            host.push_str(SNOWFLAKE_SUFFIX);
        }
        host
    };

    if host.ends_with(".snowflakecomputing.cn") {
        bail!(
            "Snowflake Cortex REST API is not available in China-region accounts (.snowflakecomputing.cn)"
        );
    }
    ensure!(
        host.len() > SNOWFLAKE_SUFFIX.len()
            && host.ends_with(SNOWFLAKE_SUFFIX)
            && host.split('.').all(is_dns_label),
        INVALID_ACCOUNT
    );
    Ok(format!("https://{host}"))
}

/// `<scheme>` of a leading `<scheme>://`, when the prefix is a syntactically valid scheme.
fn url_scheme(value: &str) -> Option<&str> {
    let (scheme, _) = value.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    let valid = first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

fn normalize_label(value: &str) -> String {
    value.to_ascii_lowercase().replace('_', "-")
}

fn normalize_host(authority: &str) -> String {
    let host = normalize_label(authority);
    if let Some(stripped) = host.strip_suffix(":443") {
        return stripped.to_owned();
    }
    host
}

/// `app.snowflake.com/<org>/<account>/...` links map to `<org>-<account>.snowflakecomputing.com`.
fn snowsight_host(path: &str) -> Result<String> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut segments = path.split('/');
    segments.next();
    let org = normalize_label(segments.next().unwrap_or_default());
    let account = normalize_label(segments.next().unwrap_or_default());
    ensure!(
        is_dns_label(&org) && is_dns_label(&account),
        INVALID_ACCOUNT
    );
    ensure!(
        !is_legacy_region(&org),
        "Legacy Snowsight links don't identify the account host; paste your account identifier (orgname-accountname) or account URL"
    );
    Ok(format!("{org}-{account}.snowflakecomputing.com"))
}

/// Legacy Snowsight region segments such as `us-east-1` or `us-gov-west-1`.
fn is_legacy_region(value: &str) -> bool {
    let mut parts: Vec<&str> = value.split('-').collect();
    if parts.len() == 4 && parts[1] == "gov" {
        parts.remove(1);
    }
    matches!(parts.as_slice(), [area, name, number]
        if area.len() == 2
            && area.bytes().all(|b| b.is_ascii_lowercase())
            && !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_lowercase())
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit()))
}

fn is_dns_label(label: &str) -> bool {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !label.is_empty()
        && label.len() <= 63
        && label.bytes().all(|b| alnum(b) || b == b'-')
        && label.bytes().next().is_some_and(alnum)
        && label.bytes().last().is_some_and(alnum)
}

#[cfg(test)]
mod tests;
