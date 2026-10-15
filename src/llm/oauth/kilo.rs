use super::super::model_config::ApiProtocol;
use super::{Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const BASE: &str = "https://api.kilo.ai/api/device-auth";

pub const FLOW: Flow = Flow {
    id: "kilo",
    name: "Kilo",
    base_url: "https://api.kilo.ai/api/gateway",
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: None,
    access: super::bearer,
};

fn login(login: &Login<'_>) -> Result<Credentials> {
    let cancel = &login.cancel;
    let response = util::request(
        "POST",
        &format!("{BASE}/codes"),
        &[],
        util::Body::Json(&json!({})),
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Kilo device authorization failed (HTTP {})",
        response.status
    );
    let value = response.json_or_null();
    let code = value["code"]
        .as_str()
        .context("Kilo device response omitted code")?;
    let url = value["verificationUrl"]
        .as_str()
        .context("Kilo device response omitted verificationUrl")?;
    let expires = value["expiresIn"]
        .as_u64()
        .context("Kilo device response omitted expiresIn")?;
    ensure!(expires > 0, "Kilo device authorization has expired");
    login.open(url, Some(&format!("Enter code {code}")));
    let device_code = code.to_owned();
    let token: Value = util::poll_device(Some(5), Some(expires), &login.cancel, || {
        let response = util::request(
            "GET",
            &format!("{BASE}/codes/{}", util::encode(&device_code)),
            &[],
            util::Body::Empty,
            cancel,
        )?;
        if response.status == 403 {
            anyhow::bail!("Kilo authorization was denied")
        }
        if response.status == 410 {
            anyhow::bail!("Kilo authorization code expired; start sign-in again")
        }
        ensure!(
            response.ok(),
            "Kilo device polling failed (HTTP {})",
            response.status
        );
        let data = response.json_or_null();
        match data["status"].as_str() {
            Some("denied") => anyhow::bail!("Kilo authorization was denied"),
            Some("expired") => anyhow::bail!("Kilo authorization code expired"),
            Some("approved") if data["token"].as_str().is_some() => Ok(util::Poll::Complete(data)),
            _ => Ok(util::Poll::Pending),
        }
    })?;
    let access = token["token"]
        .as_str()
        .or_else(|| token["access_token"].as_str())
        .context("Kilo response omitted access token")?;
    Ok(Credentials {
        access: access.into(),
        expires_at_ms: util::now_ms().saturating_add(365 * 24 * 60 * 60 * 1000),
        ..Credentials::default()
    })
}
