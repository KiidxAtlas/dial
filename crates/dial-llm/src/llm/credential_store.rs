//! One path and metadata facade for stored logins. Secret documents never escape this module.
use crate::config::paths;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn api_key_path(user_dir: &Path) -> PathBuf {
    paths::auth_file(user_dir, "credentials.json")
}
pub fn subscription_path(dir: &Path, name: &str) -> PathBuf {
    paths::auth_file(dir, name)
}
pub fn oauth_path(user_dir: &Path, id: &str) -> PathBuf {
    paths::auth_file(user_dir, &format!("oauth-{id}.json"))
}

/// Only safe identity and lifecycle fields; never bearer, refresh, ID token or API key values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginInfo {
    pub provider: String,
    pub label: String,
    pub kind: &'static str,
    pub status: &'static str,
    /// Unix seconds; absent for API keys and grants without a finite expiry.
    pub expires_at: Option<u64>,
}

fn read(path: &Path) -> Result<Option<Value>> {
    crate::platform::atomic::read_private(path)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|_| {
                anyhow::anyhow!("credential storage is invalid; existing records preserved")
            })
        })
        .transpose()
}
fn grant(
    provider: &str,
    label: String,
    kind: &'static str,
    tokens: Option<&Value>,
    milliseconds: bool,
) -> LoginInfo {
    let expires_at = tokens.and_then(|tokens| {
        if milliseconds {
            tokens
                .get("expires_at_ms")
                .and_then(Value::as_u64)
                .filter(|expiry| *expiry != super::oauth::util::NEVER_EXPIRES)
                .map(|expiry| expiry / 1000)
        } else {
            tokens.get("expires_at").and_then(Value::as_u64)
        }
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    LoginInfo {
        provider: provider.into(),
        label,
        kind,
        expires_at,
        status: if tokens.is_none() {
            "signed out"
        } else if expires_at.is_some_and(|expiry| expiry <= now) {
            "expired (refresh on use)"
        } else {
            "signed in"
        },
    }
}

/// List persisted logins without refreshing, probing a service or consulting environment secrets.
/// Subscription stores honor the same explicit shared-directory override as request-time auth.
pub fn list(user_dir: &Path) -> Result<Vec<LoginInfo>> {
    list_in(user_dir, &paths::subscription_auth_dir(user_dir)?)
}

fn list_in(user_dir: &Path, subscription_dir: &Path) -> Result<Vec<LoginInfo>> {
    let mut rows = Vec::new();
    if let Some(value) = read(&api_key_path(user_dir))? {
        let keys = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("credential storage is invalid"))?;
        for endpoint in keys.keys() {
            // Render an origin only: URL paths, queries and userinfo can themselves be secrets.
            let label = endpoint
                .parse::<ureq::http::Uri>()
                .ok()
                .and_then(|uri| uri.host().map(str::to_owned))
                .unwrap_or_else(|| "saved endpoint".into());
            rows.push(LoginInfo {
                provider: label.clone(),
                label,
                kind: "api key",
                status: "saved",
                expires_at: None,
            });
        }
    }
    if let Some(value) = read(&subscription_path(
        subscription_dir,
        "chatgpt-accounts.json",
    ))? && let Some(accounts) = value.get("accounts").and_then(Value::as_object)
    {
        for account in accounts.values() {
            let label = account
                .get("email")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("ChatGPT account")
                .to_owned();
            rows.push(grant(
                "chatgpt",
                label,
                "subscription",
                account.get("tokens").filter(|value| !value.is_null()),
                false,
            ));
        }
    }
    if let Some(value) = read(&subscription_path(
        subscription_dir,
        "anthropic-subscription.json",
    ))? {
        rows.push(grant(
            "anthropic",
            "Claude subscription".into(),
            "subscription",
            value.get("tokens").filter(|value| !value.is_null()),
            false,
        ));
    }
    for flow in super::oauth::flows() {
        if let Some(value) = read(&oauth_path(user_dir, flow.id))? {
            let accounts: Vec<&Value> = match value.get("accounts").and_then(Value::as_object) {
                Some(accounts) => accounts.values().collect(),
                None => vec![&value],
            };
            for account in accounts {
                let label = account
                    .get("email")
                    .and_then(Value::as_str)
                    .or_else(|| account.get("account_id").and_then(Value::as_str))
                    .unwrap_or(flow.name)
                    .to_owned();
                rows.push(grant(flow.id, label, "oauth", Some(account), true));
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
