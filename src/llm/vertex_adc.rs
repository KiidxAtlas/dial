//! Application Default Credentials resolution for Vertex AI.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const JWT_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
static CACHE: OnceLock<Mutex<Option<(String, String, u64)>>> = OnceLock::new();
pub(super) fn project_id() -> Result<String> {
    let path = adc_path()?.context("No Google ADC file is configured")?;
    read(&path)?["project_id"]
        .as_str()
        .map(str::to_owned)
        .context("Google ADC file has no project_id")
}
pub(super) fn quota_project_id() -> Option<String> {
    let path = adc_path().ok().flatten()?;
    read(&path)
        .ok()?
        .get("quota_project_id")?
        .as_str()
        .map(str::to_owned)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn read(path: &str) -> Result<Value> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("Unable to read Google ADC credentials at {path}"))?;
    serde_json::from_str(&data).context("Google ADC credentials are invalid JSON")
}
fn adc_path() -> Result<Option<String>> {
    if let Some(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .ok()
        .filter(|p| !p.is_empty())
    {
        return Ok(Some(path));
    }
    if let Some(dir) = std::env::var("CLOUDSDK_CONFIG")
        .ok()
        .filter(|d| !d.is_empty())
    {
        let path = format!("{dir}/application_default_credentials.json");
        return Ok(std::path::Path::new(&path).is_file().then_some(path));
    }
    #[cfg(not(windows))]
    let home = std::env::var("HOME").ok();
    #[cfg(windows)]
    let home = std::env::var("APPDATA").ok().map(|p| format!("{p}/gcloud"));
    Ok(home
        .map(|h| {
            if cfg!(windows) {
                format!("{h}/application_default_credentials.json")
            } else {
                format!("{h}/.config/gcloud/application_default_credentials.json")
            }
        })
        .filter(|p| std::path::Path::new(p).is_file()))
}
fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}
fn exchange(
    body: &[(&str, &str)],
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(String, u64)> {
    let response = super::oauth::util::request(
        "POST",
        TOKEN_URL,
        &[("content-type", "application/x-www-form-urlencoded")],
        super::oauth::util::Body::Form(body),
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Google OAuth token exchange failed (HTTP {})",
        response.status
    );
    let value = response.json()?;
    let token = value["access_token"]
        .as_str()
        .context("Google OAuth response omitted access_token")?;
    let expires = value["expires_in"]
        .as_u64()
        .context("Google OAuth response omitted expires_in")?;
    ensure!(
        !token.is_empty() && expires > 0,
        "Google OAuth returned an empty or expired access token"
    );
    Ok((token.to_owned(), now().saturating_add(expires)))
}
fn service_account(
    creds: &Value,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(String, u64)> {
    let email = creds["client_email"]
        .as_str()
        .context("Service-account credentials omit client_email")?;
    let pem = creds["private_key"]
        .as_str()
        .context("Service-account credentials omit private_key")?;
    let der = base64::engine::general_purpose::STANDARD
        .decode(
            pem.lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>(),
        )
        .context("Invalid service-account private key PEM")?;
    let key = RsaKeyPair::from_pkcs8(&der)
        .map_err(|_| anyhow::anyhow!("Invalid service-account PKCS#8 private key"))?;
    let iat = now();
    let header = json!({"alg":"RS256","typ":"JWT","kid":creds["private_key_id"]});
    let claims = json!({"iss":email,"scope":SCOPE,"aud":TOKEN_URL,"iat":iat,"exp":iat+3600});
    let payload = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        payload.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow::anyhow!("Unable to sign service-account JWT"))?;
    let assertion = format!("{payload}.{}", b64(&signature));
    exchange(
        &[("grant_type", JWT_GRANT), ("assertion", &assertion)],
        cancel,
    )
}
fn authorized_user(
    creds: &Value,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(String, u64)> {
    let client = creds["client_id"]
        .as_str()
        .context("authorized_user credentials omit client_id")?;
    let secret = creds["client_secret"]
        .as_str()
        .context("authorized_user credentials omit client_secret")?;
    let refresh = creds["refresh_token"]
        .as_str()
        .context("authorized_user credentials omit refresh_token")?;
    exchange(
        &[
            ("client_id", client),
            ("client_secret", secret),
            ("refresh_token", refresh),
            ("grant_type", "refresh_token"),
        ],
        cancel,
    )
}
pub(super) fn access_token(
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<String> {
    if let Some(token) = std::env::var("GOOGLE_CLOUD_ACCESS_TOKEN")
        .or_else(|_| std::env::var("CLOUDSDK_AUTH_ACCESS_TOKEN"))
        .or_else(|_| std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN"))
        .ok()
        .filter(|v| !v.is_empty())
    {
        return Ok(token);
    }
    let source = if let Some(path) = adc_path()? {
        let creds = read(&path)?;
        format!("file:{path}:{}", creds["type"].as_str().unwrap_or(""))
    } else {
        "metadata".into()
    };
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((key, token, expiry)) = cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && key == &source
        && now().saturating_add(60) < *expiry
    {
        return Ok(token.clone());
    }
    let (token, expiry) = if source == "metadata" {
        let response=super::oauth::util::request("GET","http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token",&[("Metadata-Flavor","Google")],super::oauth::util::Body::Empty,cancel).context("No ADC file found and metadata server is unavailable")?;
        ensure!(
            response.ok(),
            "Google metadata token request failed (HTTP {})",
            response.status
        );
        let value = response.json()?;
        (
            value["access_token"]
                .as_str()
                .context("Metadata response omitted access_token")?
                .to_owned(),
            now().saturating_add(
                value["expires_in"]
                    .as_u64()
                    .context("Metadata response omitted expires_in")?,
            ),
        )
    } else {
        let creds = read(
            source
                .strip_prefix("file:")
                .unwrap()
                .rsplit_once(':')
                .unwrap()
                .0,
        )?;
        match creds["type"].as_str() {
            Some("service_account") => service_account(&creds, cancel)?,
            Some("authorized_user") => authorized_user(&creds, cancel)?,
            Some("impersonated_service_account") => impersonated(&creds, cancel)?,
            Some(other) => bail!("Unsupported Google ADC credential type {other}"),
            None => bail!("Google ADC credentials omit type"),
        }
    };
    ensure!(
        !token.is_empty() && expiry > now(),
        "Google ADC returned an expired access token"
    );
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((source, token.clone(), expiry));
    Ok(token)
}
fn impersonated(
    creds: &Value,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(String, u64)> {
    let source = &creds["source_credentials"];
    let (token, _) = match source["type"].as_str() {
        Some("service_account") => service_account(source, cancel)?,
        Some("authorized_user") => authorized_user(source, cancel)?,
        _ => bail!("Unsupported impersonated ADC source credential type"),
    };
    let url = creds["service_account_impersonation_url"]
        .as_str()
        .context("Impersonated credentials omit service_account_impersonation_url")?;
    let account = url
        .rsplit_once("/serviceAccounts/")
        .and_then(|(_, s)| s.split(':').next())
        .context("Invalid service-account impersonation URL")?;
    let endpoint = format!(
        "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{account}:generateAccessToken"
    );
    let response = super::oauth::util::request(
        "POST",
        &endpoint,
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/json"),
        ],
        super::oauth::util::Body::Json(
            &json!({"delegates":creds["delegates"].as_array().cloned().unwrap_or_default(),"scope":[SCOPE],"lifetime":"3600s"}),
        ),
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Google service-account impersonation failed (HTTP {})",
        response.status
    );
    let value = response.json()?;
    let token = value["accessToken"]
        .as_str()
        .context("Impersonation response omitted accessToken")?;
    let expiry = value["expireTime"]
        .as_str()
        .context("Impersonation response omitted expireTime")?;
    let parsed = expiry
        .get(..19)
        .context("Invalid impersonation expireTime")?;
    let date = chrono_free_timestamp(parsed).context("Invalid impersonation expireTime")?;
    Ok((token.to_owned(), date))
}
fn chrono_free_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 19 {
        return None;
    }
    let n = |i| s.get(i..i + 2)?.parse::<u64>().ok();
    let year = s.get(0..4)?.parse::<i64>().ok()?;
    let month = s.get(5..7)?.parse::<i64>().ok()?;
    let day = s.get(8..10)?.parse::<i64>().ok()?;
    let hour = n(11)? as i64;
    let min = n(14)? as i64;
    let sec = n(17)? as i64;
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146097 + (y - era * 400) * 365 + (y - era * 400) / 4 - (y - era * 400) / 100
        + doy
        - 719468;
    u64::try_from(days * 86400 + hour * 3600 + min * 60 + sec).ok()
}
