use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::Value;

pub const FLOW: Flow = Flow {
    id: "gitlab-duo",
    name: "GitLab Duo",
    base_url: "https://cloud.gitlab.com/ai/v1/proxy/anthropic",
    protocol: ApiProtocol::GitlabDuo,
    needs_terminal: false,
    subscription: true,
    login,
    refresh: Some(refresh),
    access,
};
/// Split an `http://host[:port]/path` redirect into the loopback listener's bind parts.
fn callback_target(redirect: &str) -> Result<(String, u16, String)> {
    let rest = redirect
        .strip_prefix("http://")
        .context("GITLAB_REDIRECT_URI must be an http:// URL")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .context("GITLAB_REDIRECT_URI has an invalid port")?,
        ),
        None => (authority, 80),
    };
    ensure!(!host.is_empty(), "GITLAB_REDIRECT_URI has no host");
    Ok((host.to_owned(), port, path.to_owned()))
}
fn client() -> String {
    std::env::var("GITLAB_CLIENT_ID").unwrap_or_else(|_| {
        "da4edff2e6ebd2bc3208611e2768bc1c1dd7be791dc5ff26ca34ca9ee44f7d4b".into()
    })
}
fn login(l: &Login<'_>) -> Result<Credentials> {
    let redirect = std::env::var("GITLAB_REDIRECT_URI")
        .unwrap_or_else(|_| "http://localhost:8080/callback".into());
    let (host, port, path) = callback_target(&redirect)?;
    let callback = util::Callback::bind(&host, port, &path)?;
    let pkce = util::pkce();
    let state = util::random_urlsafe(24);
    let auth = util::with_query(
        "https://gitlab.com/oauth/authorize",
        &[
            ("client_id", &client()),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", "api"),
            ("state", &state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
        ],
    );
    l.open(&auth, None);
    let fields = callback.wait(Some(&state), &l.cancel, std::time::Duration::from_secs(300))?;
    let code = fields
        .get("code")
        .context("GitLab callback omitted authorization code")?;
    let r = token(
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &client()),
            ("redirect_uri", &redirect),
            ("code_verifier", &pkce.verifier),
        ],
        &l.cancel,
    )?;
    Ok(creds(&r))
}
fn token(
    pairs: &[(&str, &str)],
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Value> {
    let r = util::request(
        "POST",
        "https://gitlab.com/oauth/token",
        &[],
        util::Body::Form(pairs),
        cancel,
    )?;
    ensure!(r.ok(), "GitLab token request failed (HTTP {})", r.status);
    r.json()
}
fn creds(v: &Value) -> Credentials {
    let access = v["access_token"].as_str().unwrap_or_default().to_owned();
    let refresh = v["refresh_token"].as_str().unwrap_or_default().to_owned();
    Credentials {
        access,
        refresh,
        expires_at_ms: v["created_at"]
            .as_u64()
            .map(|t| {
                t.saturating_add(v["expires_in"].as_u64().unwrap_or(3600))
                    .saturating_mul(1000)
                    .saturating_sub(300_000)
            })
            .unwrap_or_else(|| util::expires_in_ms(v["expires_in"].as_u64(), 300_000, 3600)),
        ..Credentials::default()
    }
}
fn refresh(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    let r = token(
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &c.refresh),
            ("client_id", &client()),
        ],
        cancel,
    )?;
    let mut n = creds(&r);
    if n.refresh.is_empty() {
        n.refresh = c.refresh.clone()
    }
    Ok(n)
}
fn access(c: &Credentials) -> Result<Access> {
    let mut a = super::bearer(c)?;
    a.base_url = Some("https://cloud.gitlab.com/ai/v1/proxy/anthropic".into());
    Ok(a)
}
