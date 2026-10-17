//! Cloudflare AI Gateway sign-in. Not a browser OAuth grant: the user pastes an AI Gateway token
//! plus the account and gateway identifiers. The token is sent as the Anthropic key on the gateway's
//! `/anthropic` route, so no extra headers are required.

use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::sync::atomic::Ordering;

pub const FLOW: Flow = Flow {
    id: "cloudflare-ai-gateway",
    name: "Cloudflare AI Gateway",
    base_url: "https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic",
    protocol: ApiProtocol::Anthropic,
    needs_terminal: true,
    subscription: false,
    login,
    refresh: None,
    access,
};

const AUTH_URL: &str = "https://developers.cloudflare.com/ai-gateway/configuration/authentication/";
const AUTH_INSTRUCTIONS: &str =
    "Create an AI Gateway token with Run permission, then copy it here.";
const ACCOUNT_KEY: &str = "accountId";
const GATEWAY_KEY: &str = "gatewayId";

fn login(l: &Login<'_>) -> Result<Credentials> {
    l.ui.auth(AUTH_URL, Some(AUTH_INSTRUCTIONS));
    let token = prompt_required(
        l,
        "Paste your Cloudflare AI Gateway token/API key",
        "cfut_...",
        "Cloudflare AI Gateway token is required",
    )?;
    let account = prompt_required(
        l,
        "Enter your Cloudflare account ID",
        "32-character account ID",
        "Cloudflare account ID is required",
    )?;
    let gateway = prompt_required(
        l,
        "Enter your Cloudflare AI Gateway ID",
        "default",
        "Cloudflare AI Gateway ID is required",
    )?;
    let mut extra = serde_json::Map::new();
    extra.insert(ACCOUNT_KEY.into(), Value::String(account));
    extra.insert(GATEWAY_KEY.into(), Value::String(gateway));
    Ok(Credentials {
        access: token,
        expires_at_ms: util::NEVER_EXPIRES,
        extra,
        ..Credentials::default()
    })
}

fn prompt_required(
    l: &Login<'_>,
    message: &str,
    placeholder: &str,
    missing: &str,
) -> Result<String> {
    let value = l.ui.prompt(message, Some(placeholder), true)?;
    ensure!(!l.cancel.load(Ordering::SeqCst), "sign-in cancelled");
    let value = value.trim();
    ensure!(!value.is_empty(), "{missing}");
    Ok(value.to_owned())
}

fn access(c: &Credentials) -> Result<Access> {
    let account = c
        .extra_str(ACCOUNT_KEY)
        .context("Cloudflare AI Gateway credentials are missing the account ID; run /login cloudflare-ai-gateway again")?;
    let gateway = c
        .extra_str(GATEWAY_KEY)
        .context("Cloudflare AI Gateway credentials are missing the gateway ID; run /login cloudflare-ai-gateway again")?;
    ensure!(
        is_path_segment(account) && is_path_segment(gateway),
        "Cloudflare AI Gateway account or gateway ID is invalid; run /login cloudflare-ai-gateway again"
    );
    let mut access = super::bearer(c)?;
    access.base_url = Some(format!(
        "https://gateway.ai.cloudflare.com/v1/{account}/{gateway}/anthropic"
    ));
    Ok(access)
}

/// Identifiers are interpolated into a URL path, so they must be a single non-empty segment.
fn is_path_segment(value: &str) -> bool {
    !value.is_empty() && !value.contains(['/', '?', '#']) && !value.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests;
