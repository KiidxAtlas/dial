//! Alibaba Coding Plan sign-in.
//!
//! This is an API-key grant, not a browser redirect: the user picks an endpoint (international,
//! China, or a custom base URL), pastes the key, and the key is validated against that endpoint
//! before it is stored. The key is the request credential (there is nothing to refresh), and the
//! chosen base URL is kept in `enterprise_url` so it overrides the catalog base URL per request.
use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Interaction, Login, util};
use anyhow::{Result, ensure};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const DEFAULT_AUTH_URL: &str = "https://modelstudio.console.alibabacloud.com/";
const CHINA_AUTH_URL: &str = "https://bailian.console.aliyun.com/?tab=model#/api-key";
const DEFAULT_API_BASE_URL: &str = "https://coding-intl.dashscope.aliyuncs.com/v1";
const CHINA_API_BASE_URL: &str = "https://coding.dashscope.aliyuncs.com/v1";
const VALIDATION_MODEL: &str = "qwen3.5-plus";
const VALIDATION_TIMEOUT_SECS: u64 = 15;

pub const FLOW: Flow = Flow {
    id: "alibaba-coding-plan",
    name: "Alibaba Coding Plan",
    base_url: DEFAULT_API_BASE_URL,
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: true,
    subscription: true,
    login,
    refresh: None,
    access,
};

/// Endpoints the region choices resolve to. Production passes the published URLs; tests pass a
/// loopback server so the same sign-in path runs offline.
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
            international: DEFAULT_API_BASE_URL,
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
        "Select Alibaba Coding Plan endpoint: 1=International (default), 2=China, 3=Custom — enter 1, 2, or 3",
        Some("1"),
        false,
    )?;
    check_cancelled(cancel)?;
    let (base_url, auth_url, instructions, custom) = match choice.trim() {
        "2" => (
            endpoints.china.to_owned(),
            CHINA_AUTH_URL,
            "Copy your API key from the Alibaba Cloud Bailian console (China mainland)",
            false,
        ),
        "3" => {
            let custom_url = ui.prompt(
                "Enter custom base URL",
                Some("https://your-proxy.com/v1"),
                false,
            )?;
            let trimmed = custom_url.trim().trim_end_matches('/');
            ensure!(!trimmed.is_empty(), "Custom URL is required for option 3");
            (
                trimmed.to_owned(),
                DEFAULT_AUTH_URL,
                "Copy your API key from the Alibaba Cloud DashScope console",
                true,
            )
        }
        _ => (
            endpoints.international.to_owned(),
            DEFAULT_AUTH_URL,
            "Copy your API key from the Alibaba Cloud DashScope console (International)",
            false,
        ),
    };

    open(auth_url, Some(instructions));
    let answer = ui.prompt(
        "Paste your Alibaba Coding Plan API key",
        Some("sk-..."),
        false,
    )?;
    check_cancelled(cancel)?;
    let key = answer.trim();
    ensure!(!key.is_empty(), "Alibaba Coding Plan API key is required");

    ui.progress("Validating API key...");
    if custom {
        validate_models(key, &base_url, cancel)?;
    } else {
        validate_chat_completions(key, &base_url, cancel)?;
    }

    Ok(Credentials {
        access: key.to_owned(),
        refresh: String::new(),
        expires_at_ms: util::NEVER_EXPIRES,
        enterprise_url: Some(base_url),
        ..Credentials::default()
    })
}

/// One minimal chat-completions request, the same probe used for the built-in endpoints.
fn validate_chat_completions(key: &str, base_url: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
    let bearer = format!("Bearer {key}");
    let response = util::request_with_timeout(
        "POST",
        &format!("{base_url}/chat/completions"),
        &[("Authorization", bearer.as_str())],
        util::Body::Json(&json!({
            "model": VALIDATION_MODEL,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
            "temperature": 0,
        })),
        cancel,
        VALIDATION_TIMEOUT_SECS,
    )?;
    ensure!(
        response.ok(),
        "Alibaba Coding Plan API key validation failed (HTTP {})",
        response.status
    );
    Ok(())
}

/// Model-list probe for custom endpoints, where only `/models` is known to exist.
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
        "Alibaba Coding Plan API key validation failed (HTTP {})",
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
    access.base_url = credentials.enterprise_url.clone();
    Ok(access)
}

#[cfg(test)]
mod tests;
