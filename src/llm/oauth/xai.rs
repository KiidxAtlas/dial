use super::super::model_config::ApiProtocol;
use super::{Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
const CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const FLOW: Flow = Flow {
    id: "xai-oauth",
    name: "xAI Grok",
    base_url: "https://api.x.ai/v1",
    protocol: ApiProtocol::Responses,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: Some(refresh),
    access: super::bearer,
};
fn endpoint(cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<String> {
    let r = util::request(
        "GET",
        "https://auth.x.ai/.well-known/openid-configuration",
        &[],
        util::Body::Empty,
        cancel,
    )?;
    ensure!(r.ok(), "xAI OAuth discovery failed");
    let v = r.json()?;
    let e = v["token_endpoint"]
        .as_str()
        .context("xAI discovery omitted token_endpoint")?;
    let uri: ureq::http::Uri = e.parse().context("invalid xAI token endpoint")?;
    let host = uri.host().unwrap_or_default();
    ensure!(
        uri.scheme_str() == Some("https") && (host == "x.ai" || host.ends_with(".x.ai")),
        "xAI returned an unsafe token endpoint"
    );
    Ok(e.into())
}
fn login(l: &Login<'_>) -> Result<Credentials> {
    if let Ok(token) = std::env::var("XAI_OAUTH_TOKEN")
        && !token.is_empty()
    {
        let mut c = Credentials {
            access: token,
            expires_at_ms: util::NEVER_EXPIRES,
            ..Credentials::default()
        };
        identify(&mut c, &l.cancel);
        return Ok(c);
    }
    let scopes = "openid profile email offline_access grok-cli:access api:access";
    let r = util::request(
        "POST",
        "https://auth.x.ai/oauth2/device/code",
        &[("accept", "application/json")],
        util::Body::Form(&[("client_id", CLIENT), ("scope", scopes)]),
        &l.cancel,
    )?;
    ensure!(
        r.ok(),
        "xAI device authorization failed (HTTP {})",
        r.status
    );
    let v = r.json()?;
    let d = v["device_code"]
        .as_str()
        .context("xAI response omitted device_code")?
        .to_owned();
    let url = v["verification_uri_complete"]
        .as_str()
        .or_else(|| v["verification_uri"].as_str())
        .context("xAI response omitted verification URI")?;
    l.open(
        url,
        Some(&format!(
            "Enter code {}",
            v["user_code"].as_str().unwrap_or("")
        )),
    );
    let ep = endpoint(&l.cancel)?;
    let t: Value = util::poll_device(
        v["interval"].as_u64(),
        v["expires_in"].as_u64(),
        &l.cancel,
        || {
            let r = util::request(
                "POST",
                &ep,
                &[("accept", "application/json")],
                util::Body::Form(&[
                    ("client_id", CLIENT),
                    ("device_code", &d),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]),
                &l.cancel,
            )?;
            Ok(util::classify_device_response(&r))
        },
    )?;
    let mut c = creds(&t);
    identify(&mut c, &l.cancel);
    Ok(c)
}
fn creds(v: &Value) -> Credentials {
    let access = v["access_token"].as_str().unwrap_or_default().to_owned();
    let claims = util::jwt_claims(&access).unwrap_or(Value::Null);
    Credentials {
        access: access.clone(),
        refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
        expires_at_ms: util::jwt_expiry_ms(&access, 60_000)
            .unwrap_or_else(|| util::expires_in_ms(v["expires_in"].as_u64(), 60_000, 3600)),
        email: claims["email"].as_str().map(str::to_owned),
        account_id: claims["sub"].as_str().map(str::to_owned),
        ..Credentials::default()
    }
}
fn identify(c: &mut Credentials, cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
    if let Ok(r) = util::request(
        "GET",
        "https://auth.x.ai/oauth2/userinfo",
        &[("authorization", &format!("Bearer {}", c.access))],
        util::Body::Empty,
        cancel,
    ) && r.ok()
        && let Ok(v) = r.json()
    {
        c.email = v["email"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| c.email.clone());
        c.account_id = v["sub"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| c.account_id.clone());
    }
}
fn refresh(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    let ep = endpoint(cancel)?;
    let r = util::request(
        "POST",
        &ep,
        &[("accept", "application/json")],
        util::Body::Form(&[
            ("client_id", CLIENT),
            ("grant_type", "refresh_token"),
            ("refresh_token", &c.refresh),
        ]),
        cancel,
    )?;
    ensure!(r.ok(), "xAI token refresh failed (HTTP {})", r.status);
    let mut n = creds(&r.json()?);
    if n.refresh.is_empty() {
        n.refresh = c.refresh.clone()
    }
    identify(&mut n, cancel);
    Ok(n)
}
