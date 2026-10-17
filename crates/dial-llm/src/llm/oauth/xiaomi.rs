//! Xiaomi MiMo: API-key sign-in. The key is pasted, then
//! validated with a one-token chat completion. Token-plan keys (`tp-`) are probed across the
//! regional clusters in a fixed order; the cluster that accepts the key becomes the base URL
//! stored with the credential, because the key only works on that cluster.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Interaction, Login, bearer, util};

const AUTH_URL: &str = "https://platform.xiaomimimo.com/#/console/api-keys";
const STANDARD_BASE: &str = "https://api.xiaomimimo.com/v1";
const TOKEN_PLAN_BASES: [&str; 3] = [
    "https://token-plan-sgp.xiaomimimo.com/v1",
    "https://token-plan-ams.xiaomimimo.com/v1",
    "https://token-plan-cn.xiaomimimo.com/v1",
];
const MODEL: &str = "mimo-v2.5";
const VALIDATE_TIMEOUT_SECS: u64 = 15;

pub const FLOW: Flow = Flow {
    id: "xiaomi",
    name: "Xiaomi MiMo",
    base_url: STANDARD_BASE,
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: true,
    subscription: false,
    login,
    refresh: None,
    access,
};

fn login(login: &Login<'_>) -> Result<Credentials> {
    login.open(
        AUTH_URL,
        Some("Copy your API key from the Xiaomi MiMo console"),
    );
    sign_in(login.ui, &login.cancel, &|key: &str| {
        candidate_bases(key)
            .into_iter()
            .map(str::to_owned)
            .collect()
    })
}

/// Base URLs to probe for `key`, in probe order.
fn candidate_bases(key: &str) -> Vec<&'static str> {
    if key.starts_with("tp-") {
        TOKEN_PLAN_BASES.to_vec()
    } else {
        vec![STANDARD_BASE]
    }
}

/// Prompt for the key, validate it against `bases_for(key)`, and build the credential.
fn sign_in(
    ui: &dyn Interaction,
    cancel: &Arc<AtomicBool>,
    bases_for: &dyn Fn(&str) -> Vec<String>,
) -> Result<Credentials> {
    let input = ui.prompt(
        "Paste your Xiaomi API key (sk-... or token-plan tp-...)",
        Some("sk-... or tp-..."),
        false,
    )?;
    let key = input.trim();
    ensure!(!key.is_empty(), "Xiaomi MiMo API key is required");
    ui.progress("Validating xiaomi API key...");
    let base = validate(key, &bases_for(key), cancel)?;
    Ok(credential(key, &base))
}

/// Send a one-token completion to each base in order. 401 and transport failures move on to the
/// next base; any other status fails immediately. The returned error names only the status.
fn validate(key: &str, bases: &[String], cancel: &Arc<AtomicBool>) -> Result<String> {
    let body = json!({
        "model": MODEL,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let auth = format!("Bearer {key}");
    let mut last_status = None;
    for base in bases {
        let url = format!("{base}/chat/completions");
        let headers = [
            ("Content-Type", "application/json"),
            ("Authorization", auth.as_str()),
        ];
        match util::request_with_timeout(
            "POST",
            &url,
            &headers,
            util::Body::Json(&body),
            cancel,
            VALIDATE_TIMEOUT_SECS,
        ) {
            Ok(response) if response.ok() => return Ok(base.clone()),
            Ok(response) if response.status == 401 => last_status = Some(401),
            Ok(response) => bail!(
                "Xiaomi MiMo API key validation failed ({})",
                response.status
            ),
            Err(_) => {
                ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
            }
        }
    }
    match last_status {
        Some(status) => bail!("Xiaomi MiMo API key validation failed ({status})"),
        None => bail!("Xiaomi MiMo API key validation failed (unreachable)"),
    }
}

/// The stored grant: the key is the access token, and `base_url` records the validated cluster.
fn credential(key: &str, base: &str) -> Credentials {
    let mut extra = serde_json::Map::new();
    extra.insert("base_url".to_owned(), Value::String(base.to_owned()));
    Credentials {
        access: key.to_owned(),
        refresh: String::new(),
        expires_at_ms: util::NEVER_EXPIRES,
        extra,
        ..Default::default()
    }
}

fn access(credentials: &Credentials) -> Result<Access> {
    let mut access = bearer(credentials)?;
    access.base_url = credentials.extra_str("base_url").map(str::to_owned);
    Ok(access)
}

#[cfg(test)]
mod tests;
