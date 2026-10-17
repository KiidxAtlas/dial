use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub const FLOW: Flow = Flow {
    id: "kimi-code",
    name: "Kimi Code",
    base_url: "https://api.kimi.com/coding/v1",
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: Some(refresh),
    access,
};
fn host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| "https://auth.kimi.com".into())
        .trim_end_matches('/')
        .to_owned()
}
fn login(login: &Login<'_>) -> Result<Credentials> {
    let base = host();
    let r = util::request(
        "POST",
        &format!("{base}/api/oauth/device_authorization"),
        &[],
        util::Body::Json(&json!({"client_id":"17e5f671-d194-4dfb-9706-5516cb48c098"})),
        &login.cancel,
    )?;
    ensure!(
        r.ok(),
        "Kimi device authorization failed (HTTP {})",
        r.status
    );
    let v = r.json()?;
    let d = v["device_code"]
        .as_str()
        .context("Kimi response omitted device_code")?
        .to_owned();
    let url = v["verification_uri_complete"]
        .as_str()
        .or_else(|| v["verification_uri"].as_str())
        .context("Kimi response omitted verification URI")?;
    login.open(
        url,
        Some(&format!(
            "Enter code {}",
            v["user_code"].as_str().unwrap_or("")
        )),
    );
    let token: Value = util::poll_device(
        v["interval"].as_u64(),
        v["expires_in"].as_u64(),
        &login.cancel,
        || {
            let r = util::request(
                "POST",
                &format!("{base}/api/oauth/token"),
                &[],
                util::Body::Json(
                    &json!({"client_id":"17e5f671-d194-4dfb-9706-5516cb48c098","device_code":d,"grant_type":"urn:ietf:params:oauth:grant-type:device_code"}),
                ),
                &login.cancel,
            )?;
            Ok(util::classify_device_response(&r))
        },
    )?;
    Ok(creds(&token))
}
fn creds(v: &Value) -> Credentials {
    let access = v["access_token"].as_str().unwrap_or_default().to_owned();
    let refresh = v["refresh_token"].as_str().unwrap_or_default().to_owned();
    let claims = util::jwt_claims(&access).unwrap_or(Value::Null);
    let mut c = Credentials {
        access,
        refresh,
        expires_at_ms: util::jwt_expiry_ms(v["access_token"].as_str().unwrap_or_default(), 60_000)
            .unwrap_or_else(|| util::expires_in_ms(v["expires_in"].as_u64(), 60_000, 3600)),
        account_id: claims["user_id"]
            .as_str()
            .or_else(|| claims["sub"].as_str())
            .map(str::to_owned),
        ..Credentials::default()
    };
    c.extra
        .insert("device_id".into(), Value::String(util::random_hex(16)));
    c
}
fn refresh(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    ensure!(!c.refresh.is_empty(), "Kimi refresh token is missing");
    let base = host();
    let r = util::request(
        "POST",
        &format!("{base}/api/oauth/token"),
        &[],
        util::Body::Json(
            &json!({"client_id":"17e5f671-d194-4dfb-9706-5516cb48c098","refresh_token":c.refresh,"grant_type":"refresh_token"}),
        ),
        cancel,
    )?;
    ensure!(r.ok(), "Kimi token refresh failed (HTTP {})", r.status);
    let mut n = creds(&r.json()?);
    if n.refresh.is_empty() {
        n.refresh = c.refresh.clone()
    }
    n.extra.insert(
        "device_id".into(),
        Value::String(
            c.extra_str("device_id")
                .map(str::to_owned)
                .unwrap_or_else(|| util::random_hex(16)),
        ),
    );
    Ok(n)
}
fn access(c: &Credentials) -> Result<Access> {
    let mut a = super::bearer(c)?;
    let id = c.extra_str("device_id").unwrap_or("dial");
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    a.headers = vec![
        ("User-Agent".into(), "KimiCLI/1.0".into()),
        ("X-Msh-Platform".into(), "kimi_cli".into()),
        ("X-Msh-Version".into(), "1.0".into()),
        ("X-Msh-Device-Name".into(), host),
        (
            "X-Msh-Device-Model".into(),
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        ("X-Msh-Os-Version".into(), std::env::consts::OS.into()),
        ("X-Msh-Device-Id".into(), id.into()),
    ];
    Ok(a)
}
