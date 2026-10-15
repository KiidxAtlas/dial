use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const CLIENT: &str = "client_01HNM792M5G5G1A2THWPXKFMXB";
const DEVICE: &str = "https://api.workos.com/user_management/authorize/device";
const TOKEN: &str = "https://api.workos.com/user_management/authenticate";
pub const FLOW: Flow = Flow {
    id: "factory-droid",
    name: "Factory Droid",
    base_url: "https://api.factory.ai",
    protocol: ApiProtocol::FactoryDroid,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: Some(refresh),
    access,
};
fn login(l: &Login<'_>) -> Result<Credentials> {
    let r = util::request(
        "POST",
        DEVICE,
        &[("accept", "application/json")],
        util::Body::Json(&json!({"client_id":CLIENT})),
        &l.cancel,
    )?;
    ensure!(
        r.ok(),
        "Factory device authorization failed (HTTP {})",
        r.status
    );
    let v = r.json()?;
    let device = v["device_code"]
        .as_str()
        .context("WorkOS omitted device_code")?
        .to_owned();
    let url = v["verification_uri_complete"]
        .as_str()
        .or_else(|| v["verification_uri"].as_str())
        .context("WorkOS omitted verification URI")?;
    l.open(url, Some(v["user_code"].as_str().unwrap_or("")));
    let token: Value = util::poll_device(
        v["interval"].as_u64(),
        v["expires_in"].as_u64(),
        &l.cancel,
        || {
            let r = util::request(
                "POST",
                TOKEN,
                &[("accept", "application/json")],
                util::Body::Json(
                    &json!({"client_id":CLIENT,"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":device}),
                ),
                &l.cancel,
            )?;
            Ok(util::classify_device_response(&r))
        },
    )?;
    attach_region(credentials(&token), &l.cancel)
}
fn credentials(v: &Value) -> Credentials {
    let access = v["access_token"].as_str().unwrap_or_default().to_owned();
    let claims = util::jwt_claims(&access).unwrap_or(Value::Null);
    Credentials {
        access: access.clone(),
        refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
        expires_at_ms: util::jwt_expiry_ms(&access, 0)
            .unwrap_or_else(|| util::expires_in_ms(v["expires_in"].as_u64(), 0, 86400)),
        email: v
            .pointer("/user/email")
            .and_then(Value::as_str)
            .or_else(|| claims["email"].as_str())
            .map(str::to_owned),
        account_id: v
            .pointer("/user/id")
            .and_then(Value::as_str)
            .or_else(|| claims["sub"].as_str())
            .map(str::to_owned),
        org_id: claims["external_org_id"].as_str().map(str::to_owned),
        ..Credentials::default()
    }
}
fn refresh(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    let r = util::request(
        "POST",
        TOKEN,
        &[("accept", "application/json")],
        util::Body::Json(
            &json!({"client_id":CLIENT,"grant_type":"refresh_token","refresh_token":c.refresh}),
        ),
        cancel,
    )?;
    ensure!(r.ok(), "Factory refresh failed (HTTP {})", r.status);
    let mut n = credentials(&r.json()?);
    if n.refresh.is_empty() {
        n.refresh = c.refresh.clone()
    }
    if n.email.is_none() {
        n.email = c.email.clone()
    }
    if n.org_id.is_none() {
        n.org_id = c.org_id.clone()
    }
    if n.region.is_none() {
        n.region = c.region.clone()
    }
    // The refresh token rotated on success: keep the new tokens even if the identity lookup
    // fails transiently, or the old refresh token is left stored and invalid.
    let rotated = n.clone();
    match attach_region(n, cancel) {
        Ok(refreshed) => Ok(refreshed),
        Err(_) if cancel.load(std::sync::atomic::Ordering::SeqCst) => {
            Err(super::super::Interrupted.into())
        }
        Err(_) => Ok(rotated),
    }
}
fn attach_region(
    mut c: Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    let host = if c.region.as_deref() == Some("eu") {
        "https://api.eu.factory.ai"
    } else {
        "https://api.factory.ai"
    };
    let mut owned = vec![("authorization".to_owned(), format!("Bearer {}", c.access))];
    if let Some(org) = &c.org_id {
        owned.push(("x-factory-org-id".into(), org.clone()))
    }
    let headers: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let r = util::request(
        "GET",
        &format!("{host}/api/cli/whoami"),
        &headers,
        util::Body::Empty,
        cancel,
    )?;
    ensure!(r.ok(), "Factory identity lookup failed (HTTP {})", r.status);
    let v = r.json()?;
    if let Some(org) = v["orgId"].as_str() {
        c.org_id = Some(org.into())
    }
    if let Some(region) = v["region"]
        .as_str()
        .filter(|r| *r == "eu" || *r == "global")
    {
        c.region = Some(region.into())
    }
    if let Some(region) = v["inferenceRegion"]
        .as_str()
        .filter(|r| ["global", "eu", "us"].contains(r))
    {
        c.extra
            .insert("inference_region".into(), Value::String(region.into()));
    }
    ensure!(
        c.org_id.is_some(),
        "Factory identity lookup returned no organization id"
    );
    Ok(c)
}
fn access(c: &Credentials) -> Result<Access> {
    let mut a = super::bearer(c)?;
    a.base_url = Some(
        if c.region.as_deref() == Some("eu") {
            "https://api.eu.factory.ai"
        } else {
            "https://api.factory.ai"
        }
        .into(),
    );
    a.headers.push((
        "X-Factory-Org-Id".into(),
        c.org_id
            .clone()
            .context("Factory grant has no external organization id")?,
    ));
    if let Some(region) = c.extra_str("inference_region") {
        a.headers
            .push(("X-Factory-Inference-Region".into(), region.into()))
    }
    Ok(a)
}
