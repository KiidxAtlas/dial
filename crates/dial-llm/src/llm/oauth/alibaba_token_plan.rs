//! QwenCloud Token Plan sign-in.
//!
//! An API-key grant: the user picks a region (international, China Beijing, or a custom base URL),
//! pastes the key, and the key is validated against `GET {base}/models` before it is stored. An
//! optional browser Cookie header enables quota reporting for the usage module.
//!
//! Storage shape (read by `usage_report`):
//! - `access`: the Token Plan API key.
//! - `api_endpoint`: the region base URL, only when it differs from the international default.
//!   Requests use it as `Access.base_url`.
//! - `extra["cookie"]`: the normalized Cookie header value, when one was supplied.
use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Interaction, Login, util};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const INTERNATIONAL_AUTH_URL: &str =
    "https://home.qwencloud.com/billing/subscription/token-plan-individual";
const CHINA_AUTH_URL: &str = "https://www.aliyun.com/benefit/scene/tokenplan";
const INTERNATIONAL_API_BASE_URL: &str =
    "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1";
const CHINA_API_BASE_URL: &str =
    "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1";
const CHINA_COOKIE_HOST: &str = "bailian-cs.console.aliyun.com";
const INTERNATIONAL_COOKIE_HOST: &str = "cs-data.qwencloud.com";
const VALIDATION_TIMEOUT_SECS: u64 = 15;

pub const FLOW: Flow = Flow {
    id: "alibaba-token-plan",
    name: "QwenCloud Token Plan",
    base_url: INTERNATIONAL_API_BASE_URL,
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: true,
    subscription: true,
    login,
    refresh: None,
    access,
};

/// Region endpoints. Production passes the published URLs; tests pass a loopback server.
struct Endpoints<'a> {
    international: &'a str,
    china: &'a str,
}

fn login(login: &Login<'_>) -> Result<Credentials> {
    sign_in(
        login.ui,
        &login.cancel,
        &|url: &str, instructions: Option<&str>| login.open(url, instructions),
        &Endpoints {
            international: INTERNATIONAL_API_BASE_URL,
            china: CHINA_API_BASE_URL,
        },
    )
}

fn sign_in(
    ui: &dyn Interaction,
    cancel: &Arc<AtomicBool>,
    open: &dyn Fn(&str, Option<&str>),
    endpoints: &Endpoints<'_>,
) -> Result<Credentials> {
    let choice = ui.prompt(
        "Select QwenCloud Token Plan region: 1=International (default), 2=China (Beijing), 3=Custom — enter 1, 2, or 3",
        Some("1"),
        false,
    )?;
    check_cancelled(cancel)?;
    let (base_url, auth_url, instructions) = match choice.trim() {
        "2" => (
            endpoints.china.to_owned(),
            CHINA_AUTH_URL,
            "Subscribe to the China (Beijing) 百炼 Token Plan and copy its dedicated API key",
        ),
        "3" => {
            let custom_url = ui.prompt(
                "Enter custom Token Plan base URL",
                Some("https://token-plan.<region>.maas.aliyuncs.com/compatible-mode/v1"),
                false,
            )?;
            let trimmed = custom_url.trim().trim_end_matches('/');
            ensure!(!trimmed.is_empty(), "Custom URL is required for option 3");
            (
                trimmed.to_owned(),
                INTERNATIONAL_AUTH_URL,
                "Copy your Token Plan API key for the custom endpoint",
            )
        }
        _ => (
            endpoints.international.to_owned(),
            INTERNATIONAL_AUTH_URL,
            "Subscribe to Token Plan Individual and copy its dedicated API key. Keep this page open; the next prompt explains how to enable optional quota reporting.",
        ),
    };

    open(auth_url, Some(instructions));
    let answer = ui.prompt(
        "Paste your QwenCloud Token Plan API key",
        Some("sk-sp-..."),
        false,
    )?;
    check_cancelled(cancel)?;
    let key = answer.trim();
    ensure!(!key.is_empty(), "QwenCloud Token Plan API key is required");

    ui.progress("Validating API key...");
    validate_models(key, &base_url, cancel)?;

    let cookie = prompt_cookie(ui, cancel, &base_url, endpoints)?;

    let mut extra = serde_json::Map::new();
    if !cookie.is_empty() {
        extra.insert("cookie".to_owned(), cookie.into());
    }
    Ok(Credentials {
        access: key.to_owned(),
        refresh: String::new(),
        expires_at_ms: util::NEVER_EXPIRES,
        api_endpoint: (base_url != endpoints.international).then_some(base_url),
        extra,
        ..Credentials::default()
    })
}

/// Optional usage-reporting cookie. Empty input skips quota reporting; anything else must be a
/// `name=value` Cookie header so a lone cookie value pasted by mistake is rejected here.
fn prompt_cookie(
    ui: &dyn Interaction,
    cancel: &Arc<AtomicBool>,
    base_url: &str,
    endpoints: &Endpoints<'_>,
) -> Result<String> {
    let china = base_url == endpoints.china;
    let (message, host) = if china {
        (CHINA_COOKIE_PROMPT, CHINA_COOKIE_HOST)
    } else {
        (INTERNATIONAL_COOKIE_PROMPT, INTERNATIONAL_COOKIE_HOST)
    };
    let answer = ui.prompt(message, Some("name=value; name=value; ..."), true)?;
    check_cancelled(cancel)?;
    let cookie = normalize_cookie(&answer);
    ensure!(
        cookie.is_empty() || is_cookie_header(&cookie),
        "Invalid QwenCloud Cookie header. Copy the complete Cookie request header from the {host} usage request, not a single cookie value."
    );
    Ok(cookie)
}

/// Trims whitespace and a leading case-insensitive `Cookie:` label.
fn normalize_cookie(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
        .map_or(trimmed, |_| &trimmed[7..]);
    stripped.trim().to_owned()
}

/// True when at least one `;`-separated segment is `name=value` with both sides non-empty.
fn is_cookie_header(cookie: &str) -> bool {
    cookie.split(';').any(|segment| {
        segment
            .trim()
            .split_once('=')
            .is_some_and(|(name, value)| !name.trim().is_empty() && !value.trim().is_empty())
    })
}

fn validate_models(key: &str, base_url: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
    let bearer = format!("Bearer {key}");
    let response = util::request_with_timeout(
        "GET",
        &format!("{base_url}/models"),
        &[("Authorization", bearer.as_str())],
        util::Body::Empty,
        cancel,
        VALIDATION_TIMEOUT_SECS,
    )?;
    ensure!(
        response.ok(),
        "QwenCloud Token Plan API key validation failed (HTTP {})",
        response.status
    );
    Ok(())
}

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
    Ok(())
}

fn access(credentials: &Credentials) -> Result<Access> {
    let mut access = super::bearer(credentials)?;
    access.base_url = credentials.api_endpoint.clone();
    Ok(access)
}

const CHINA_COOKIE_PROMPT: &str = "Optional: paste the Cookie header from the China Bailian usage request (bailian-cs.console.aliyun.com) to enable quota reporting. Leave empty to skip.";
const INTERNATIONAL_COOKIE_PROMPT: &str = "Optional: paste the Cookie header from the QwenCloud usage request (cs-data.qwencloud.com) to enable quota reporting. Leave empty to skip.";

#[cfg(test)]
mod tests;
