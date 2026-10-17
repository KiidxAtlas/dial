use super::super::model_config::ApiProtocol;
use super::{Credentials, Flow, Login, util};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
pub const FLOW: Flow = Flow {
    id: "cursor",
    name: "Cursor",
    base_url: "https://api2.cursor.sh",
    protocol: ApiProtocol::Cursor,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: Some(refresh),
    access,
};
const CLIENT: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
fn login(l: &Login<'_>) -> Result<Credentials> {
    let pk = util::pkce();
    let id = util::uuid_v4();
    let url = util::with_query(
        "https://cursor.com/loginDeepControl",
        &[
            ("challenge", &pk.challenge),
            ("uuid", &id),
            ("mode", "login"),
            ("redirectTarget", "cli"),
        ],
    );
    l.open(&url, None);
    let mut errors = 0;
    let value: Value = util::poll_device(Some(1), Some(300), &l.cancel, || {
        let endpoint = util::with_query(
            "https://api2.cursor.sh/auth/poll",
            &[("uuid", &id), ("verifier", &pk.verifier)],
        );
        let r = util::request("GET", &endpoint, &[], util::Body::Empty, &l.cancel)?;
        if r.status == 404 {
            errors = 0;
            return Ok(util::Poll::Pending);
        }
        if !r.ok() {
            errors += 1;
            if errors >= 3 {
                anyhow::bail!("Cursor authentication polling failed (HTTP {})", r.status)
            }
            return Ok(util::Poll::Pending);
        }
        Ok(util::Poll::Complete(r.json()?))
    })?;
    let mut c = creds(&value);
    c.email = profile_email(&c.access, &l.cancel).ok().flatten();
    Ok(c)
}
fn creds(v: &Value) -> Credentials {
    let access = v["accessToken"]
        .as_str()
        .or_else(|| v["access_token"].as_str())
        .unwrap_or_default()
        .to_owned();
    let claims = util::jwt_claims(&access).unwrap_or(Value::Null);
    Credentials {
        access: access.clone(),
        refresh: v["refreshToken"]
            .as_str()
            .or_else(|| v["refresh_token"].as_str())
            .unwrap_or_default()
            .into(),
        expires_at_ms: util::jwt_expiry_ms(&access, 300_000)
            .unwrap_or_else(|| util::expires_in_ms(None, 300_000, 3_600)),
        email: claims["email"].as_str().map(str::to_owned),
        account_id: user_id(&access),
        ..Credentials::default()
    }
}
fn user_id(access: &str) -> Option<String> {
    let sub = util::jwt_claims(access)?["sub"].as_str()?.to_owned();
    Some(
        sub.split_once('|')
            .map_or(sub.as_str(), |(_, id)| id)
            .to_owned(),
    )
}
fn access(credentials: &Credentials) -> Result<super::Access> {
    let mut shaped = super::bearer(credentials)?;
    if shaped.account_id.is_none() {
        // Without `sub`, the rotating access token must not become replay ownership; email is stable.
        shaped.account_id = user_id(&credentials.access).or_else(|| credentials.email.clone());
    }
    Ok(shaped)
}
fn profile_email(
    access: &str,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Option<String>> {
    let Some(uid) = user_id(access) else {
        return Ok(None);
    };
    let cookie = format!(
        "WorkosCursorSessionToken={}",
        util::encode(&format!("{uid}::{access}"))
    );
    let r = util::request(
        "GET",
        "https://cursor.com/api/auth/me",
        &[("accept", "application/json"), ("cookie", &cookie)],
        util::Body::Empty,
        cancel,
    )?;
    ensure!(r.ok(), "Cursor profile request failed (HTTP {})", r.status);
    let v = r.json()?;
    if v["sub"].as_str() == Some(uid.as_str()) {
        Ok(v["email"].as_str().map(str::to_owned))
    } else {
        Ok(None)
    }
}
fn refresh(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    let r = util::request(
        "POST",
        "https://api2.cursor.sh/oauth/token",
        &[("content-type", "application/json")],
        util::Body::Json(
            &json!({"grant_type":"refresh_token","refresh_token":c.refresh,"client_id":CLIENT}),
        ),
        cancel,
    )?;
    ensure!(r.ok(), "Cursor token refresh failed (HTTP {})", r.status);
    let v = r.json()?;
    ensure!(
        v["shouldLogout"] != true,
        "Cursor ended this session; sign in again"
    );
    let mut n = creds(&v);
    ensure!(
        !n.access.is_empty(),
        "Cursor refresh returned no access token"
    );
    if n.refresh.is_empty() {
        n.refresh = c.refresh.clone()
    }
    if n.email.is_none() {
        n.email = profile_email(&n.access, cancel)
            .ok()
            .flatten()
            .or_else(|| c.email.clone())
    }
    Ok(n)
}
