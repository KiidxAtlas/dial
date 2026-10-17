use super::super::model_config::ApiProtocol;
use super::{Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::Value;

pub const FLOW: Flow = Flow {
    id: "copilot",
    name: "GitHub Copilot",
    base_url: "https://api.githubcopilot.com",
    protocol: ApiProtocol::Copilot,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: None,
    access: super::bearer,
};
fn login(login: &Login<'_>) -> Result<Credentials> {
    let response = util::request(
        "POST",
        "https://github.com/login/device/code",
        &[
            ("accept", "application/json"),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        util::Body::Form(&[
            ("client_id", "Ov23li8tweQw6odWQebz"),
            ("scope", "read:user"),
        ]),
        &login.cancel,
    )?;
    ensure!(
        response.ok(),
        "GitHub device authorization failed (HTTP {})",
        response.status
    );
    let v = response.json()?;
    let device = v["device_code"]
        .as_str()
        .context("GitHub response omitted device_code")?;
    let user = v["user_code"]
        .as_str()
        .context("GitHub response omitted user_code")?;
    let url = v["verification_uri"]
        .as_str()
        .unwrap_or("https://github.com/login/device");
    login.open(url, Some(&format!("Enter code {user}")));
    let d = device.to_owned();
    let token: Value = util::poll_device(
        v["interval"].as_u64(),
        v["expires_in"].as_u64(),
        &login.cancel,
        || {
            let r = util::request(
                "POST",
                "https://github.com/login/oauth/access_token",
                &[
                    ("accept", "application/json"),
                    ("content-type", "application/x-www-form-urlencoded"),
                ],
                util::Body::Form(&[
                    ("client_id", "Ov23li8tweQw6odWQebz"),
                    ("device_code", &d),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]),
                &login.cancel,
            )?;
            Ok(util::classify_device_response(&r))
        },
    )?;
    Ok(Credentials {
        access: util::token_field(&token, "/access_token", "GitHub")?,
        expires_at_ms: util::NEVER_EXPIRES,
        ..Credentials::default()
    })
}
