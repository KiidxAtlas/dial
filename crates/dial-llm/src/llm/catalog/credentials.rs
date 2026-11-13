//! Credential storage and resolution: URL-scoped saved tokens, environment and CLI sources, endpoint templates.

use super::bundled;
use super::{KNOWN, LOCAL_SERVERS, ProviderStatus};
use crate::config::settings_store::{locked, write_private};
use crate::llm::model_config::{ApiProtocol, ProviderSpec};
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(super) fn validated_url(base: &str, allow_insecure: bool) -> Result<ureq::http::Uri> {
    ensure!(
        !base.contains('#'),
        "endpoint URL must not contain a fragment"
    );
    // Endpoint validation deliberately does not echo the input, which could be a secret.
    crate::llm::provider::endpoint_url(base, allow_insecure).map_err(|_| {
        anyhow::anyhow!("invalid or insecure endpoint URL; credentials and queries are forbidden")
    })?;
    base.parse()
        .map_err(|_| anyhow::anyhow!("invalid endpoint URL"))
}
pub(super) fn valid_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
        "credential must be nonempty printable ASCII without whitespace"
    );
    Ok(())
}
fn credential_map(user_dir: &Path) -> Result<BTreeMap<String, String>> {
    let bytes = crate::platform::atomic::read_private(
        &crate::llm::credential_store::api_key_path(user_dir),
    )?;
    match bytes {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("credential file is invalid; no changes saved")),
        None => Ok(BTreeMap::new()),
    }
}
pub fn credential_token(user_dir: &Path, base_url: &str) -> Result<Option<String>> {
    validated_url(base_url, true)?;
    let path = crate::llm::credential_store::api_key_path(user_dir);
    let _lock = locked(&path)?;
    let token = credential_map(user_dir)?.remove(base_url);
    if let Some(token) = &token {
        valid_token(token)?;
    }
    Ok(token)
}
/// Base URLs (without a trailing `/`) that have a saved credential, read once per discovery pass.
/// An unreadable store yields none; a probed provider still reports that error from [`prepare`].
pub(super) fn saved_credential_urls(user_dir: &Path) -> BTreeSet<String> {
    if user_dir.as_os_str().is_empty() {
        return BTreeSet::new();
    }
    let Ok(_lock) = locked(&crate::llm::credential_store::api_key_path(user_dir)) else {
        return BTreeSet::new();
    };
    credential_map(user_dir)
        .map(|saved| {
            saved
                .into_keys()
                .map(|url| url.trim_end_matches('/').to_owned())
                .collect()
        })
        .unwrap_or_default()
}
/// Bundled API-key providers that `taken` does not already cover (the always-probed [`KNOWN`]
/// set, local servers and configured providers). One with a credential (an `is_set` variable or a
/// key saved for its base URL) is returned as an endpoint to probe; the rest are listed as not set
/// up, without a network request, so `/providers` can offer them. Sign-in providers stay with
/// [`oauth_catalog`]; templated URLs and non-HTTP agent wires need explicit configuration first.
pub(super) fn bundled_endpoints(
    taken: impl Fn(&str) -> bool,
    saved: &BTreeSet<String>,
    is_set: impl Fn(&str) -> bool,
) -> (Vec<(String, ProviderSpec)>, Vec<ProviderStatus>) {
    let mut probes = Vec::new();
    let mut not_set_up = Vec::new();
    for provider in bundled::builtin_providers() {
        if taken(&provider.id)
            || crate::llm::oauth::flow(&provider.id).is_some()
            || provider.base_url.contains('<')
            || !(bundled::routable(provider.protocol) || provider.protocol == ApiProtocol::Ollama)
        {
            continue;
        }
        let env = provider.key_env(&is_set);
        if env.is_some_and(&is_set) || saved.contains(provider.base_url.trim_end_matches('/')) {
            probes.push((
                provider.id.clone(),
                ProviderSpec {
                    label: provider.label.clone(),
                    base_url: provider.base_url.clone(),
                    api_key_env: env.map(str::to_owned),
                    protocol: Some(provider.protocol),
                    ..ProviderSpec::default()
                },
            ));
        } else {
            not_set_up.push(ProviderStatus {
                id: provider.id.clone(),
                label: provider.label.clone(),
                base_url: Some(provider.base_url.clone()),
                state: "auth-required".into(),
                detail: match env {
                    Some(variable) => format!("Not set up: set {variable} or add an API key"),
                    None => "Not set up: add an API key".into(),
                },
                models: 0,
            });
        }
    }
    (probes, not_set_up)
}
/// The environment variable Dial reads for a built-in provider's base URL, if the URL is one of its known endpoints.
pub fn known_key_env(base_url: &str) -> Option<&'static str> {
    let uri = base_url.parse::<ureq::http::Uri>().ok()?;
    KNOWN
        .iter()
        .find(|(_, _, url, _)| {
            url.parse::<ureq::http::Uri>()
                .is_ok_and(|known| origin(&known) == origin(&uri))
        })
        .map(|(_, _, _, key)| *key)
}
pub fn save_credential(user_dir: &Path, base_url: &str, token: &str) -> Result<()> {
    validated_url(base_url, true)?;
    valid_token(token)?;
    let path = crate::llm::credential_store::api_key_path(user_dir);
    let _lock = locked(&path)?;
    let mut credentials = credential_map(user_dir)?;
    credentials.insert(base_url.to_owned(), token.to_owned());
    write_private(
        &crate::llm::credential_store::api_key_path(user_dir),
        &serde_json::to_vec(&credentials)?,
    )
}
pub fn remove_credential(user_dir: &Path, base_url: &str) -> Result<()> {
    validated_url(base_url, true)?;
    let path = crate::llm::credential_store::api_key_path(user_dir);
    let _lock = locked(&path)?;
    let mut credentials = credential_map(user_dir)?;
    credentials.remove(base_url);
    write_private(
        &crate::llm::credential_store::api_key_path(user_dir),
        &serde_json::to_vec(&credentials)?,
    )
}
/// Forget Dial's URL-scoped keys and sign-ins for a provider, never the host environment.
pub fn remove_provider_credentials(user_dir: &Path, id: &str, urls: &[String]) -> Result<()> {
    let mut urls = urls.to_vec();
    if let Some((_, _, _, base)) = LOCAL_SERVERS.iter().find(|&&(name, ..)| name == id) {
        urls.push((*base).into());
    }
    if let Some((_, _, base, _)) = KNOWN.iter().find(|&&(name, ..)| name == id) {
        urls.push((*base).into());
    }
    if let Some(provider) = bundled::builtin_provider(id) {
        urls.push(provider.base_url.clone());
    }
    {
        let path = crate::llm::credential_store::api_key_path(user_dir);
        let _lock = locked(&path)?;
        let mut credentials = credential_map(user_dir)?;
        credentials.retain(|base, _| {
            !urls
                .iter()
                .any(|url| base.trim_end_matches('/') == url.trim_end_matches('/'))
        });
        write_private(&path, &serde_json::to_vec(&credentials)?)?;
    }
    if id == crate::llm::chatgpt::PROVIDER {
        crate::llm::chatgpt::forget_accounts(user_dir)?;
    } else if id == "anthropic" {
        crate::llm::anthropic_oauth::sign_out(user_dir)?;
    } else if crate::llm::oauth::flow(id).is_some() {
        crate::llm::oauth::sign_out(user_dir, id)?;
    }
    Ok(())
}

// Never derive Debug: this transient object can contain a bearer credential.
pub(crate) fn origin(uri: &ureq::http::Uri) -> (String, String, u16) {
    (
        uri.scheme_str().unwrap_or("").to_ascii_lowercase(),
        uri.host().unwrap_or("").to_ascii_lowercase(),
        uri.port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            }),
    )
}
/// `--api-key` for one endpoint origin. Process memory only; never written to disk or logs.
static CLI_API_KEY: std::sync::LazyLock<parking_lot::RwLock<Option<ScopedKey>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(None));
/// An API key paired with the origin (scheme, host, port) it applies to.
type ScopedKey = ((String, String, u16), String);
/// Sets the process-only API key for `base_url`'s origin. Applies ahead of env and saved keys.
pub fn set_cli_api_key(base_url: &str, key: String) -> Result<()> {
    let uri = validated_url(base_url, true)?;
    valid_token(&key)?;
    *CLI_API_KEY.write() = Some((origin(&uri), key));
    Ok(())
}
/// The `--api-key` for this exact origin, if one was set.
pub(super) fn cli_token(uri: &ureq::http::Uri) -> Option<String> {
    CLI_API_KEY
        .read()
        .as_ref()
        .filter(|(scope, _)| *scope == origin(uri))
        .map(|(_, key)| key.clone())
}
pub(super) fn env_token(name: &str, uri: &ureq::http::Uri) -> Result<Option<String>> {
    if let Some((_, _, url, _)) = KNOWN.iter().find(|(_, _, _, key)| *key == name) {
        let known: ureq::http::Uri = url.parse()?;
        ensure!(
            origin(uri) == origin(&known),
            "provider credential environment variable does not match endpoint origin"
        );
    }
    let token = std::env::var(name).ok().filter(|s| !s.is_empty());
    if let Some(token) = &token {
        valid_token(token)?;
    }
    Ok(token)
}
/// Account and gateway settings that fill templated built-in base URLs (`<account>`, `<gateway>`).
#[derive(Debug, Clone, Copy, Default)]
pub struct EndpointSettings<'a> {
    pub snowflake_account: Option<&'a str>,
    pub cloudflare_account_id: Option<&'a str>,
    pub cloudflare_gateway_id: Option<&'a str>,
}
/// Substitutes the account/gateway placeholders of a built-in base URL. A missing setting leaves its
/// placeholder in place; `provider::endpoint_url` then refuses the URL and names the setting.
pub fn fill_endpoint_template(
    provider_id: &str,
    base: &str,
    settings: EndpointSettings<'_>,
) -> String {
    let (account, gateway) = match provider_id {
        "snowflake" => (settings.snowflake_account, None),
        "cloudflare-ai-gateway" => (
            settings.cloudflare_account_id,
            settings.cloudflare_gateway_id,
        ),
        _ => return base.to_owned(),
    };
    let mut filled = base.to_owned();
    if let Some(account) = account.filter(|value| !value.is_empty()) {
        filled = filled.replace("<account>", account);
    }
    if let Some(gateway) = gateway.filter(|value| !value.is_empty()) {
        filled = filled.replace("<gateway>", gateway);
    }
    filled
}
/// Shared inference/discovery credential precedence, with provider-origin checks for known env keys.
pub fn resolve_credential(
    user_dir: &Path,
    base_url: &str,
    variable: Option<&str>,
) -> Result<Option<String>> {
    let uri = validated_url(base_url, true)?;
    if let Some(token) = cli_token(&uri) {
        return Ok(Some(token));
    }
    let variable = variable.or_else(|| {
        KNOWN
            .iter()
            .find(|(_, _, url, _)| {
                url.parse::<ureq::http::Uri>()
                    .is_ok_and(|known| origin(&known) == origin(&uri))
            })
            .map(|(_, _, _, key)| *key)
    });
    if let Some(variable) = variable
        && let Some(token) = env_token(variable, &uri)?
    {
        return Ok(Some(token));
    }
    let token = if user_dir.as_os_str().is_empty() {
        None
    } else {
        credential_token(user_dir, base_url)?
    };
    ensure!(
        variable.is_none() || token.is_some(),
        "provider credentials are missing; configure an API key in /providers or set its credential environment variable"
    );
    Ok(token)
}
