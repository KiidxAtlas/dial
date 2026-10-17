use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const CLIENT: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
pub const FLOW: Flow = Flow {
    id: "zai",
    name: "Z.AI Coding Plan",
    base_url: "https://api.z.ai/api/anthropic",
    protocol: ApiProtocol::Anthropic,
    needs_terminal: true,
    subscription: true,
    login,
    refresh: None,
    access,
};

fn login(login: &Login<'_>) -> Result<Credentials> {
    let redirect = std::env::var("ZAI_OAUTH_REDIRECT_URI")
        .unwrap_or_else(|_| "zcode://zai-auth/callback".into());
    let state = util::random_urlsafe(24);
    let authorize = std::env::var("ZAI_OAUTH_AUTHORIZE_URL")
        .unwrap_or_else(|_| "https://chat.z.ai/api/oauth/authorize".into());
    let client = std::env::var("ZAI_OAUTH_CLIENT_ID").unwrap_or_else(|_| CLIENT.into());
    login.open(
        &util::with_query(
            &authorize,
            &[
                ("client_id", &client),
                ("redirect_uri", &redirect),
                ("response_type", "code"),
                ("state", &state),
            ],
        ),
        Some("Authorize Z.AI, then paste the final redirect URL or authorization code."),
    );
    let input = login
        .ui
        .prompt("Authorization code or redirect URL", None, false)?;
    let (code, received_state) = util::parse_pasted_code(&input)?;
    if let Some(s) = received_state {
        ensure!(s == state, "Z.AI OAuth state mismatch");
    }
    let token_url = std::env::var("ZAI_OAUTH_TOKEN_URL")
        .unwrap_or_else(|_| "https://zcode.z.ai/api/v1/oauth/token".into());
    let response = util::request(
        "POST",
        &token_url,
        &[],
        util::Body::Json(
            &json!({"provider":"zai","code":code,"redirect_uri":redirect,"state":state}),
        ),
        &login.cancel,
    )?;
    ensure!(
        response.ok(),
        "Z.AI token exchange failed (HTTP {})",
        response.status
    );
    let data = response.json()?;
    let access = util::token_field(&data, "/data/zai/access_token", "Z.AI")?;
    let mut c = Credentials {
        access,
        expires_at_ms: util::NEVER_EXPIRES,
        ..Credentials::default()
    };
    if let Some(email) = data.pointer("/data/email").and_then(Value::as_str) {
        c.email = Some(email.into());
    }
    if let Some(account) = data.pointer("/data/account_id").and_then(Value::as_str) {
        c.account_id = Some(account.into());
    }
    mint_key(&mut c, &login.cancel)?;
    Ok(c)
}
fn mint_key(
    c: &mut Credentials,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let response = util::request(
        "POST",
        "https://api.z.ai/api/auth/z/login",
        &[],
        util::Body::Json(&json!({"token":c.access})),
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Z.AI business login failed (HTTP {})",
        response.status
    );
    let biz = envelope(response.json()?, "business login")?;
    let business_token = biz["access_token"]
        .as_str()
        .or_else(|| biz["accessToken"].as_str())
        .context("Z.AI business login omitted access token")?;
    let authorization = format!("Bearer {business_token}");
    let headers = [("authorization", authorization.as_str())];
    let response = util::request(
        "GET",
        "https://api.z.ai/api/biz/customer/getCustomerInfo",
        &headers,
        util::Body::Empty,
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Z.AI customer lookup failed (HTTP {})",
        response.status
    );
    let customer = envelope(response.json()?, "customer lookup")?;
    let orgs = customer["organizations"]
        .as_array()
        .context("Z.AI customer response omitted organizations")?;
    let org = orgs
        .iter()
        .find(|o| o["isDefault"] == true)
        .or_else(|| orgs.first())
        .context("Z.AI account has no organization")?;
    let projects = org["projects"]
        .as_array()
        .context("Z.AI organization has no projects")?;
    let project = projects
        .iter()
        .find(|p| p["isDefault"] == true)
        .or_else(|| projects.first())
        .context("Z.AI organization has no project")?;
    let org_id = org["organizationId"]
        .as_str()
        .context("Z.AI organization omitted organizationId")?;
    let project_id = project["projectId"]
        .as_str()
        .context("Z.AI project omitted projectId")?;
    let keys_url =
        format!("https://api.z.ai/api/biz/v1/organization/{org_id}/projects/{project_id}/api_keys");
    let response = util::request("GET", &keys_url, &headers, util::Body::Empty, cancel)?;
    ensure!(
        response.ok(),
        "Z.AI API key lookup failed (HTTP {})",
        response.status
    );
    let list = envelope(response.json()?, "api key list")?;
    let keys = list
        .as_array()
        .or_else(|| {
            ["list", "keys", "apiKeys", "records"]
                .iter()
                .find_map(|k| list[*k].as_array())
        })
        .context("Z.AI API key list response has no key array")?;
    let existing = keys.iter().find(|key| key["name"] == "dial");
    let record = if let Some(key) = existing {
        key.clone()
    } else {
        let response = util::request(
            "POST",
            &keys_url,
            &headers,
            util::Body::Json(&json!({"name":"dial"})),
            cancel,
        )?;
        ensure!(
            response.ok(),
            "Z.AI API key creation failed (HTTP {})",
            response.status
        );
        envelope(response.json()?, "api key create")?
    };
    let api_key = record["apiKey"]
        .as_str()
        .context("Z.AI API key record omitted apiKey")?;
    let response = util::request(
        "GET",
        &format!("{keys_url}/copy/{}", util::encode(api_key)),
        &headers,
        util::Body::Empty,
        cancel,
    )?;
    ensure!(
        response.ok(),
        "Z.AI API key copy failed (HTTP {})",
        response.status
    );
    let copied = envelope(response.json()?, "api key copy")?;
    let secret = copied["secretKey"]
        .as_str()
        .context("Z.AI API key copy omitted secretKey")?;
    c.extra.insert(
        "api_key".into(),
        Value::String(format!("{api_key}.{secret}")),
    );
    Ok(())
}
fn envelope(value: Value, operation: &str) -> Result<Value> {
    if value.get("code").is_some() || value.get("success").is_some() {
        ensure!(
            value["success"] != false
                && (value.get("code").is_none()
                    || matches!(value["code"].as_i64(), Some(0 | 200))
                    || matches!(value["code"].as_str(), Some("0" | "200"))),
            "Z.AI {operation} rejected"
        );
        Ok(value.get("data").cloned().unwrap_or(value))
    } else {
        Ok(value)
    }
}
fn access(c: &Credentials) -> Result<Access> {
    let key = c
        .extra_str("api_key")
        .context("Z.AI API key is missing; sign in again")?;
    Ok(Access {
        token: crate::llm::auth::SecretToken::new(key.to_owned()),
        base_url: None,
        headers: vec![],
        project_id: None,
        account_id: c.account_id.clone(),
    })
}
