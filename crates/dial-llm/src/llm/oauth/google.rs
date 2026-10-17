use super::super::model_config::ApiProtocol;
use super::{Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const CLI_CLIENT: &str = "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const CLI_SECRET: &str = "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";
const ANTI_CLIENT: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const ANTI_SECRET: &str = "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";
pub const GEMINI_CLI: Flow = Flow {
    id: "google-gemini-cli",
    name: "Google Gemini CLI",
    base_url: "https://cloudcode-pa.googleapis.com",
    protocol: ApiProtocol::CloudCodeAssist,
    needs_terminal: false,
    subscription: true,
    login: login_cli,
    refresh: Some(refresh_cli),
    access: super::bearer,
};
pub const ANTIGRAVITY: Flow = Flow {
    id: "google-antigravity",
    name: "Google Antigravity",
    base_url: "https://daily-cloudcode-pa.googleapis.com",
    protocol: ApiProtocol::Antigravity,
    needs_terminal: false,
    subscription: true,
    login: login_anti,
    refresh: Some(refresh_anti),
    access: super::bearer,
};
fn login_cli(l: &Login<'_>) -> Result<Credentials> {
    login(
        l,
        CLI_CLIENT,
        CLI_SECRET,
        8085,
        "/oauth2callback",
        "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile",
        "https://cloudcode-pa.googleapis.com",
    )
}
fn login_anti(l: &Login<'_>) -> Result<Credentials> {
    login(
        l,
        ANTI_CLIENT,
        ANTI_SECRET,
        51121,
        "/oauth-callback",
        "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs",
        "https://daily-cloudcode-pa.googleapis.com",
    )
}
fn login(
    l: &Login<'_>,
    client: &str,
    secret: &str,
    port: u16,
    path: &str,
    scope: &str,
    endpoint: &str,
) -> Result<Credentials> {
    let callback = util::Callback::bind("127.0.0.1", port, path)?;
    let redirect = callback.redirect_uri();
    let pk = util::pkce();
    let state = util::random_urlsafe(24);
    let url = util::with_query(
        "https://accounts.google.com/o/oauth2/v2/auth",
        &[
            ("client_id", client),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", scope),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", &state),
            ("code_challenge", &pk.challenge),
            ("code_challenge_method", "S256"),
        ],
    );
    l.open(&url, None);
    let fields = callback.wait(Some(&state), &l.cancel, std::time::Duration::from_secs(300))?;
    let code = fields.get("code").context("Google callback omitted code")?;
    let r = util::request(
        "POST",
        "https://oauth2.googleapis.com/token",
        &[],
        util::Body::Form(&[
            ("client_id", client),
            ("client_secret", secret),
            ("code", code),
            ("code_verifier", &pk.verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &redirect),
        ]),
        &l.cancel,
    )?;
    ensure!(r.ok(), "Google token exchange failed (HTTP {})", r.status);
    let mut c = creds(&r.json()?);
    c.project_id = Some(discover_project(&c.access, &l.cancel, endpoint)?);
    Ok(c)
}
fn creds(v: &Value) -> Credentials {
    let access = v["access_token"].as_str().unwrap_or_default().to_owned();
    let claims = util::jwt_claims(&access).unwrap_or(Value::Null);
    Credentials {
        access: access.clone(),
        refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
        expires_at_ms: util::jwt_expiry_ms(&access, 300_000)
            .unwrap_or_else(|| util::expires_in_ms(v["expires_in"].as_u64(), 300_000, 3600)),
        email: claims["email"].as_str().map(str::to_owned),
        account_id: claims["sub"].as_str().map(str::to_owned),
        ..Credentials::default()
    }
}
fn discover_project(
    access: &str,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    endpoint: &str,
) -> Result<String> {
    let env_project = std::env::var("GOOGLE_CLOUD_PROJECT")
        .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT_ID"))
        .ok();
    let antigravity = endpoint.contains("daily-cloudcode");
    let metadata = if antigravity {
        json!({"ideType":"ANTIGRAVITY"})
    } else {
        json!({"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":env_project})
    };
    let headers = [
        ("authorization".to_owned(), format!("Bearer {access}")),
        ("content-type".into(), "application/json".into()),
        (
            "user-agent".into(),
            if antigravity {
                "antigravity/hub/2.19.1 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)"
                    .into()
            } else {
                format!(
                    "GeminiCLI/0.46.0/gemini-3.1-pro-preview ({}; {}; terminal)",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )
            },
        ),
        (
            "client-metadata".into(),
            if antigravity {
                "ideType=ANTIGRAVITY".into()
            } else {
                "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI".into()
            },
        ),
    ];
    let h: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut loaded = util::request(
        "POST",
        &format!("{endpoint}/v1internal:loadCodeAssist"),
        &h,
        util::Body::Json(&if antigravity {
            json!({"metadata":metadata})
        } else {
            json!({"cloudaicompanionProject":env_project,"metadata":metadata})
        }),
        cancel,
    )?;
    ensure!(
        loaded.ok(),
        "Google loadCodeAssist failed (HTTP {})",
        loaded.status
    );
    let mut data = loaded.json()?;
    if antigravity && data["paidTier"].is_null() && data["cloudaicompanionProject"].is_string() {
        let project = data["cloudaicompanionProject"].clone();
        loaded = util::request(
            "POST",
            &format!("{endpoint}/v1internal:loadCodeAssist"),
            &h,
            util::Body::Json(
                &json!({"cloudaicompanionProject":project,"metadata":{"ideType":"ANTIGRAVITY"}}),
            ),
            cancel,
        )?;
        ensure!(
            loaded.ok(),
            "Google loadCodeAssist project lookup failed (HTTP {})",
            loaded.status
        );
        data = loaded.json()?;
    }
    fn project_id(v: &Value) -> Option<&str> {
        v["cloudaicompanionProject"]["id"]
            .as_str()
            .or_else(|| v["cloudaicompanionProject"].as_str())
    }
    if data["currentTier"].is_object() || data["paidTier"].is_object() {
        if let Some(id) = project_id(&data) {
            return Ok(id.to_owned());
        }
        return env_project.context(
            "This Google account requires GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID",
        );
    }
    let allowed = data["allowedTiers"].as_array().cloned().unwrap_or_default();
    let tier = if antigravity {
        "free-tier"
    } else {
        allowed
            .iter()
            .find(|t| t["isDefault"] == true)
            .and_then(|t| t["id"].as_str())
            .unwrap_or("legacy-tier")
    };
    if tier != "free-tier" && env_project.is_none() {
        anyhow::bail!(
            "This Google account requires GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID"
        )
    }
    let onboard_body = if antigravity {
        json!({"tierId":tier,"metadata":{"ideType":"ANTIGRAVITY"}})
    } else {
        json!({"tierId":tier,"cloudaicompanionProject":env_project,"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":env_project}})
    };
    let onboard = util::request(
        "POST",
        &format!("{endpoint}/v1internal:onboardUser"),
        &h,
        util::Body::Json(&onboard_body),
        cancel,
    )?;
    ensure!(
        onboard.ok(),
        "Google onboardUser failed (HTTP {})",
        onboard.status
    );
    let mut operation = onboard.json()?;
    let max_attempts = if antigravity { 30 } else { 24 };
    for attempt in 0..max_attempts {
        if operation["done"] == true {
            break;
        }
        ensure!(
            attempt + 1 < max_attempts,
            "Google project provisioning did not complete after {max_attempts} polls"
        );
        let name = operation["name"]
            .as_str()
            .context("Google onboardUser returned no operation name")?;
        util::sleep_cancellable(
            std::time::Duration::from_secs(if antigravity { 1 } else { 5 }),
            cancel,
        )?;
        let polled = util::request(
            "GET",
            &format!("{endpoint}/v1internal/{name}"),
            &h,
            util::Body::Empty,
            cancel,
        )?;
        ensure!(
            polled.ok(),
            "Google project operation polling failed (HTTP {})",
            polled.status
        );
        operation = polled.json()?;
    }
    if let Some(id) = operation
        .pointer("/response/cloudaicompanionProject/id")
        .and_then(Value::as_str)
    {
        return Ok(id.to_owned());
    }
    if let Some(id) = operation
        .pointer("/response/cloudaicompanionProject")
        .and_then(Value::as_str)
    {
        return Ok(id.to_owned());
    }
    env_project.context("Google project provisioning returned no cloudaicompanionProject")
}
fn refresh_cli(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    refresh(c, cancel, CLI_CLIENT, CLI_SECRET)
}
fn refresh_anti(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Credentials> {
    refresh(c, cancel, ANTI_CLIENT, ANTI_SECRET)
}
fn refresh(
    c: &Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    client: &str,
    secret: &str,
) -> Result<Credentials> {
    let r = util::request(
        "POST",
        "https://oauth2.googleapis.com/token",
        &[],
        util::Body::Form(&[
            ("client_id", client),
            ("client_secret", secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", &c.refresh),
        ]),
        cancel,
    )?;
    ensure!(r.ok(), "Google token refresh failed (HTTP {})", r.status);
    let mut n = creds(&r.json()?);
    if n.refresh.is_empty() {
        n.refresh = c.refresh.clone();
    }
    n.project_id = c.project_id.clone();
    Ok(n)
}
