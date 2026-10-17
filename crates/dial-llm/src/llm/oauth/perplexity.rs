use super::super::model_config::ApiProtocol;
use super::{Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
const UA: &str = "Perplexity/641 CFNetwork/1568 Darwin/25.2.0";
const VER: &str = "2.18";
pub const FLOW: Flow = Flow {
    id: "perplexity",
    name: "Perplexity",
    base_url: "https://www.perplexity.ai",
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: true,
    subscription: true,
    login,
    refresh: None,
    access: super::bearer,
};
fn login(l: &Login<'_>) -> Result<Credentials> {
    let email = l.ui.prompt(
        "Enter your Perplexity email address",
        Some("user@example.com"),
        false,
    )?;
    let mut cookies = String::new();
    let csrf = request(
        "GET",
        "https://www.perplexity.ai/api/auth/csrf",
        None,
        &cookies,
        &l.cancel,
    )?;
    ensure!(
        csrf.ok(),
        "Perplexity CSRF request failed (HTTP {})",
        csrf.status
    );
    remember(&csrf, &mut cookies);
    let csrf_token = csrf.json()?["csrfToken"]
        .as_str()
        .context("Perplexity CSRF response omitted csrfToken")?
        .to_owned();
    let sent = request(
        "POST",
        "https://www.perplexity.ai/api/auth/signin-email",
        Some(&json!({"email":email,"csrfToken":csrf_token})),
        &cookies,
        &l.cancel,
    )?;
    ensure!(
        sent.ok(),
        "Perplexity email login failed (HTTP {})",
        sent.status
    );
    remember(&sent, &mut cookies);
    let otp =
        l.ui.prompt("Enter the code sent to your email", Some("123456"), false)?;
    let verified = request(
        "POST",
        "https://www.perplexity.ai/api/auth/signin-otp",
        Some(&json!({"email":email,"otp":otp,"csrfToken":csrf_token})),
        &cookies,
        &l.cancel,
    )?;
    remember(&verified, &mut cookies);
    ensure!(
        verified.ok(),
        "Perplexity OTP verification failed (HTTP {})",
        verified.status
    );
    let mut value = verified.json()?;
    if value["status"] == "totp_challenge_required" {
        let challenge = value["challenge_token"]
            .as_str()
            .context("Perplexity TOTP challenge omitted token")?;
        let code = l.ui.prompt(
            "Enter the code from your authenticator app",
            Some("123456"),
            false,
        )?;
        let response = request(
            "POST",
            "https://www.perplexity.ai/api/auth/totp/challenge-verify",
            Some(&json!({"token":challenge,"code":code})),
            &cookies,
            &l.cancel,
        )?;
        remember(&response, &mut cookies);
        ensure!(
            response.ok(),
            "Perplexity TOTP verification failed (HTTP {})",
            response.status
        );
        value = response.json()?;
    }
    let token = value["token"]
        .as_str()
        .or_else(|| value["challenge_token"].as_str())
        .map(str::to_owned)
        .or_else(|| cookie_token(&cookies));
    let token = token.context("Perplexity sign-in completed without a session token")?;
    let expiry = util::jwt_expiry_ms(&token, 300_000).unwrap_or(util::NEVER_EXPIRES);
    Ok(Credentials {
        access: token.clone(),
        refresh: token,
        expires_at_ms: expiry,
        email: Some(email),
        ..Credentials::default()
    })
}
fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    cookie: &str,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<util::Response> {
    let mut headers = vec![("user-agent", UA), ("x-app-apiversion", VER)];
    if !cookie.is_empty() {
        headers.push(("cookie", cookie));
    }
    if body.is_some() {
        headers.push(("content-type", "application/json"));
    }
    util::request(
        method,
        url,
        &headers,
        body.map_or(util::Body::Empty, util::Body::Json),
        cancel,
    )
}
fn remember(response: &util::Response, cookie: &mut String) {
    for value in response.headers("set-cookie") {
        let pair = value.split(';').next().unwrap_or_default();
        if let Some((name, val)) = pair.split_once('=') {
            let name = name.trim();
            let val = val.trim();
            let expired =
                value.contains("Max-Age=0") || value.contains("max-age=0") || val.is_empty();
            let mut entries: Vec<String> = cookie
                .split("; ")
                .filter(|s| !s.starts_with(&format!("{name}=")))
                .map(str::to_owned)
                .collect();
            if !expired {
                entries.push(format!("{name}={val}"));
            }
            *cookie = entries.join("; ");
        }
    }
}
fn cookie_token(cookie: &str) -> Option<String> {
    cookie.split("; ").find_map(|s| {
        s.strip_prefix("__Secure-next-auth.session-token=")
            .or_else(|| s.strip_prefix("next-auth.session-token="))
            .map(str::to_owned)
    })
}
