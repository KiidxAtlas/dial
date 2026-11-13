//! Provider probing: builds requests, parses model listings and verifies credentials.

use super::bundled;
use super::credentials::{
    cli_token, credential_token, env_token, origin, save_credential, valid_token, validated_url,
};
use super::local_servers::{
    annotate_lmstudio, annotate_show_metadata, declared_kind, lmstudio_models_with_headers,
};
use super::metadata::{
    ModelEntry, ModelKind, ModelMetadata, documented_context_window, documented_image_generation,
    metadata,
};
use super::{
    KNOWN, LISTING_BUDGET, MAX_MODELS, MAX_PAGES, MAX_RESPONSE, OPENROUTER_BASE_URL,
    OPENROUTER_VALIDATE_URL, ProviderStatus,
};
use crate::llm::model_config::{ApiProtocol, ModelSelection, ProviderSpec};
use crate::llm::profiles::ImageGenerationApi;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) struct Probe {
    pub(super) id: String,
    pub(super) spec: ProviderSpec,
    pub(super) token: Option<String>,
    pub(super) blocked: Option<(&'static str, &'static str)>,
}
pub(super) fn prepare(id: String, spec: ProviderSpec, user_dir: &Path) -> Probe {
    let mut probe = Probe {
        id,
        spec,
        token: None,
        blocked: None,
    };
    if !probe.spec.enabled {
        probe.blocked = Some(("disabled", "Disabled in configuration; not probed"));
        return probe;
    }
    let uri = match validated_url(&probe.spec.base_url, probe.spec.allow_insecure) {
        Ok(uri) => uri,
        Err(_) => {
            probe.blocked = Some(("unavailable", "Invalid or insecure endpoint URL"));
            return probe;
        }
    };
    // API-key selection must not read or refresh unrelated sign-in credentials.
    if probe.id == "anthropic"
        && probe.spec.auth == crate::llm::model_config::AuthMethod::OAuth
        && probe.spec.base_url != crate::llm::anthropic::BASE_URL
    {
        probe.blocked = Some((
            "unavailable",
            "Claude subscription requires its official endpoint",
        ));
        return probe;
    }
    if probe.id == "anthropic" && probe.spec.base_url == crate::llm::anthropic::BASE_URL {
        let subscription = crate::llm::oauth::selected_auth(user_dir, &probe.id, probe.spec.auth)
            .and_then(|selected| {
                if selected {
                    crate::config::paths::subscription_auth_dir(user_dir).map(Some)
                } else {
                    Ok(None)
                }
            });
        match subscription {
            Ok(Some(dir)) => {
                if !probe
                    .spec
                    .protocol
                    .is_none_or(|p| p == ApiProtocol::Anthropic)
                {
                    probe.blocked = Some((
                        "unavailable",
                        "Claude subscription requires the Anthropic protocol",
                    ));
                    return probe;
                }
                apply_anthropic_oauth_access(
                    &mut probe,
                    crate::llm::anthropic_oauth::access_token(
                        &dir,
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    ),
                );
                return probe;
            }
            Err(error) => {
                apply_anthropic_oauth_access(&mut probe, Err(error));
                return probe;
            }
            Ok(None) => {}
        }
    }
    if probe.spec.auth == crate::llm::model_config::AuthMethod::OAuth {
        // Generic sign-ins use the bundled catalog and attach grants at request time.
        probe.blocked = Some((
            "auth-required",
            "Sign in from /providers to use the selected authentication method",
        ));
        return probe;
    }
    let env = probe.spec.api_key_env.as_deref().or_else(|| {
        KNOWN
            .iter()
            .find(|(_, _, base, _)| {
                base.parse::<ureq::http::Uri>()
                    .is_ok_and(|known| origin(&known) == origin(&uri))
            })
            .map(|(_, _, _, key)| *key)
    });
    let env_result = match cli_token(&uri) {
        Some(token) => Ok(Some(token)),
        None => env
            .map(|name| env_token(name, &uri))
            .transpose()
            .map(Option::flatten),
    };
    let token = match env_result {
        Ok(token) => token,
        Err(_) => {
            probe.blocked = Some((
                "auth-required",
                "Credential environment variable is invalid or belongs to a different provider origin",
            ));
            return probe;
        }
    };
    probe.token = match token {
        Some(token) => Some(token),
        None => match credential_token(user_dir, &probe.spec.base_url) {
            Ok(token) => token,
            Err(_) => {
                probe.blocked = Some(("auth-required", "Saved credentials cannot be read safely"));
                return probe;
            }
        },
    };
    if env.is_some() && probe.token.is_none() {
        probe.blocked = Some((
            "auth-required",
            "Set the configured credential environment variable or save a URL-scoped token",
        ));
    }
    probe
}
pub(super) fn apply_anthropic_oauth_access(
    probe: &mut Probe,
    access: Result<crate::llm::auth::SecretToken>,
) {
    probe.token = None;
    probe.blocked = None;
    match access {
        Ok(token) => probe.token = Some(token.as_str().to_owned()),
        Err(_) => {
            probe.blocked = Some((
                "auth-required",
                "Claude subscription authentication failed; sign in again in /providers to repair credentials",
            ));
        }
    }
}
pub(super) fn status(probe: &Probe, state: &str, detail: &str, models: usize) -> ProviderStatus {
    ProviderStatus {
        id: probe.id.clone(),
        label: if probe.spec.label.is_empty() {
            probe.id.clone()
        } else {
            probe.spec.label.clone()
        },
        base_url: validated_url(&probe.spec.base_url, true)
            .ok()
            .map(|_| probe.spec.base_url.clone()),
        state: state.into(),
        detail: detail.into(),
        models,
    }
}
pub(super) fn context(value: &Value) -> Option<u32> {
    [
        "/inputTokenLimit",
        "/context_window",
        "/max_input_tokens",
        "/context_length",
        "/max_context_length",
        "/max_position_embeddings",
        "/text_config/max_position_embeddings",
        "/top_provider/context_length",
        "/meta/n_ctx_train",
        "/details/context_length",
    ]
    .iter()
    .filter_map(|key| value.pointer(key))
    .find_map(|v| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    })
}
pub(super) fn parse_models(value: &Value, probe: &Probe) -> Result<Vec<ModelEntry>> {
    let rows = value
        .get(
            if matches!(
                probe
                    .spec
                    .protocol
                    .unwrap_or_else(|| ApiProtocol::builtin(&probe.id)),
                ApiProtocol::Ollama | ApiProtocol::Gemini
            ) {
                "models"
            } else {
                "data"
            },
        )
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("model response has no model array"))?;
    let mut seen = BTreeSet::new();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let model = row
                .get("id")
                .or_else(|| row.get("model"))
                .or_else(|| row.get("name"))?
                .as_str()?
                .trim();
            let gemini = probe
                .spec
                .protocol
                .unwrap_or_else(|| ApiProtocol::builtin(&probe.id))
                == ApiProtocol::Gemini;
            let methods = row["supportedGenerationMethods"].as_array();
            let declares = |method: &str| methods.is_some_and(|m| m.iter().any(|x| x == method));
            // Gemini states purpose only through its generation methods; rows with neither are dropped.
            let declared = if !gemini {
                ModelKind::Unknown
            } else if declares("generateContent") {
                ModelKind::Chat
            } else if declares("embedContent") {
                ModelKind::Embedding
            } else {
                return None;
            };
            let model = if gemini {
                model.strip_prefix("models/").unwrap_or(model)
            } else {
                model
            };
            if model.is_empty()
                || model.len() > 4096
                || model.chars().any(char::is_control)
                || !seen.insert(model.to_owned())
            {
                return None;
            }
            let mut selection = ModelSelection {
                provider: probe.id.clone(),
                model: model.into(),
                base_url: Some(probe.spec.base_url.clone()),
                api_key_env: probe.spec.api_key_env.clone(),
                allow_insecure: probe.spec.allow_insecure,
                protocol: Some(
                    probe
                        .spec
                        .protocol
                        .unwrap_or_else(|| ApiProtocol::builtin(&probe.id)),
                ),
                headers: probe.spec.headers.clone(),
                account_id: None,
                context_window: context(row),
                thinking: None,
            };
            selection.context_window = selection
                .context_window
                .or_else(|| documented_context_window(&selection));
            let reported = if declared == ModelKind::Unknown {
                declared_kind(row)
            } else {
                declared
            };
            let mut kind = bundled::known_kind(&selection, reported);
            // Capabilities the live row left open come from the same bundled record the runtime
            // uses (`model_metadata`), so the picker shows what requests will actually assume.
            let mut live = metadata(row);
            live.image_generation = live
                .image_generation
                .or_else(|| documented_image_generation(&probe.id, &probe.spec.base_url, model));
            // A documented direct image transport with no declared purpose is an image model, not chat.
            if kind == ModelKind::Unknown
                && live.image_generation == Some(ImageGenerationApi::Images)
            {
                kind = ModelKind::Image;
            }
            let metadata =
                bundled::complete_capabilities(&selection, ModelMetadata { kind, ..live });
            Some(ModelEntry {
                selection,
                label: model.into(),
                source: probe.spec.label.clone(),
                available: true,
                metadata,
            })
        })
        .take(MAX_MODELS)
        .collect())
}
/// Detail of the status for an endpoint that refused the connection or did not answer in time. The
/// settings panel shows it as "not running" for a local server, rather than as a failure.
pub const UNREACHABLE_DETAIL: &str = "Connection failed or metadata probe timed out";
pub(super) fn probe_models(probe: &Probe) -> (ProviderStatus, Vec<ModelEntry>) {
    let (mut state, models) = probe_models_inner(probe);
    if state.state == "unavailable" && state.detail != UNREACHABLE_DETAIL {
        state.detail = format!("Model listing incomplete; {}", state.detail);
    }
    (state, models)
}
fn probe_models_inner(probe: &Probe) -> (ProviderStatus, Vec<ModelEntry>) {
    if crate::llm::model_config::validate_headers(probe.spec.headers.as_deref().unwrap_or_default())
        .is_err()
    {
        return (
            status(probe, "unavailable", "Invalid request headers", 0),
            Vec::new(),
        );
    }
    if probe
        .token
        .as_deref()
        .is_some_and(crate::llm::anthropic::subscription_token)
        && probe.spec.headers.as_ref().is_some_and(|h| !h.is_empty())
    {
        return (
            status(
                probe,
                "unavailable",
                "Subscription does not permit request headers",
                0,
            ),
            Vec::new(),
        );
    }
    if let Some((state, detail)) = probe.blocked {
        return (status(probe, state, detail, 0), Vec::new());
    }
    let uri = match validated_url(&probe.spec.base_url, probe.spec.allow_insecure) {
        Ok(uri) => uri,
        Err(_) => {
            return (
                status(probe, "unavailable", "Invalid endpoint URL", 0),
                Vec::new(),
            );
        }
    };
    let local = crate::llm::is_local_host(uri.host().unwrap_or(""));
    let agent = crate::llm::agent_with(if local { 1 } else { 5 }, !local, None, None);
    let protocol = probe
        .spec
        .protocol
        .unwrap_or_else(|| ApiProtocol::builtin(&probe.id));
    let url = if protocol == ApiProtocol::Gemini {
        format!("{}/models", probe.spec.base_url.trim_end_matches('/'))
    } else if protocol == ApiProtocol::Ollama {
        let base = probe
            .spec
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches("/api");
        format!("{base}/api/tags")
    } else {
        let chat = match crate::llm::provider::endpoint_url(
            &probe.spec.base_url,
            probe.spec.allow_insecure,
        ) {
            Ok(url) => url,
            Err(_) => {
                return (
                    status(probe, "unavailable", "Invalid endpoint URL", 0),
                    Vec::new(),
                );
            }
        };
        format!(
            "{}models",
            chat.strip_suffix("chat/completions").unwrap_or(&chat)
        )
    };
    let deadline = Instant::now()
        + if local {
            Duration::from_secs(1)
        } else {
            LISTING_BUDGET
        };
    let mut cursor: Option<String> = None;
    let mut cursors = BTreeSet::new();
    let mut models = Vec::new();
    let mut total_bytes = 0_u64;
    let mut total_rows = 0_usize;
    for page in 0..MAX_PAGES {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return (
                status(probe, "unavailable", "Model listing deadline reached", 0),
                Vec::new(),
            );
        };
        let mut request = agent.get(&url).header("Accept", "application/json");
        if let Some(cursor) = &cursor {
            // Continuations are opaque query values, never provider-controlled URLs. Reuse the
            // validated endpoint and the same authentication on every page.
            request = request.query(
                if protocol == ApiProtocol::Gemini {
                    "pageToken"
                } else {
                    "after_id"
                },
                cursor,
            );
        }
        let resolved_headers = match crate::llm::model_config::resolve_validated_headers(
            probe.spec.headers.as_deref().unwrap_or_default(),
        ) {
            Ok(headers) => headers,
            Err(_) => {
                return (
                    status(
                        probe,
                        "unavailable",
                        "Request header environment is unavailable or invalid",
                        0,
                    ),
                    Vec::new(),
                );
            }
        };
        for (name, value) in resolved_headers.pairs() {
            let field = match crate::llm::model_config::private_header_value(value) {
                Ok(field) => field,
                Err(_) => {
                    return (
                        status(probe, "unavailable", "Invalid request header value", 0),
                        Vec::new(),
                    );
                }
            };
            request = request.header(name, field);
        }
        if let Some(token) = &probe.token {
            if protocol == ApiProtocol::Gemini {
                request = request.header("x-goog-api-key", token);
            } else if protocol == ApiProtocol::Anthropic {
                let headers = match crate::llm::anthropic::auth_headers(&url, token) {
                    Ok(headers) => headers,
                    Err(_) => {
                        return (
                            status(
                                probe,
                                "auth-required",
                                "Subscription credentials require the official Anthropic endpoint",
                                0,
                            ),
                            Vec::new(),
                        );
                    }
                };
                for (name, value) in headers {
                    request = request.header(name, value);
                }
            } else {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
        }
        let mut response = match request
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
        {
            Ok(response) => response,
            Err(_) => {
                return (
                    status(probe, "unavailable", UNREACHABLE_DETAIL, 0),
                    Vec::new(),
                );
            }
        };
        let code = response.status().as_u16();
        if code == 401 || code == 403 {
            return (
                status(
                    probe,
                    "auth-required",
                    "Endpoint requires valid credentials",
                    0,
                ),
                Vec::new(),
            );
        }
        if !(200..300).contains(&code) {
            return (
                status(
                    probe,
                    "unavailable",
                    &format!("Endpoint returned HTTP {code}; redirects are not followed"),
                    0,
                ),
                Vec::new(),
            );
        }
        let mut bytes = Vec::new();
        if response
            .body_mut()
            .as_reader()
            .take(MAX_RESPONSE.saturating_sub(total_bytes) + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_RESPONSE.saturating_sub(total_bytes)
        {
            return (
                status(
                    probe,
                    "unavailable",
                    "Metadata response exceeded limits or could not be read",
                    0,
                ),
                Vec::new(),
            );
        }
        total_bytes += bytes.len() as u64;
        let value = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(_) => {
                return (
                    status(
                        probe,
                        "unavailable",
                        "Endpoint returned malformed model metadata",
                        0,
                    ),
                    Vec::new(),
                );
            }
        };
        let rows = value
            .get(
                if matches!(protocol, ApiProtocol::Gemini | ApiProtocol::Ollama) {
                    "models"
                } else {
                    "data"
                },
            )
            .and_then(Value::as_array);
        total_rows += rows.map_or(0, Vec::len);
        if total_rows > MAX_MODELS {
            return (
                status(
                    probe,
                    "unavailable",
                    "Model listing exceeded model limit",
                    0,
                ),
                Vec::new(),
            );
        }
        let entries = match parse_models(&value, probe) {
            Ok(entries) => entries,
            Err(_) => {
                return (
                    status(
                        probe,
                        "unavailable",
                        "Endpoint returned malformed model metadata",
                        0,
                    ),
                    Vec::new(),
                );
            }
        };
        models.extend(entries);
        let next = match listing_cursor(&value, protocol) {
            Ok(next) => next,
            Err(_) => {
                return (
                    status(
                        probe,
                        "unavailable",
                        "Invalid model listing continuation",
                        0,
                    ),
                    Vec::new(),
                );
            }
        };
        if let Some(next) = next {
            if page + 1 == MAX_PAGES || !cursors.insert(next.clone()) {
                return (
                    status(
                        probe,
                        "unavailable",
                        "Model listing continuation limit reached or repeated cursor",
                        0,
                    ),
                    Vec::new(),
                );
            }
            cursor = Some(next);
            continue;
        }
        let mut seen = BTreeSet::new();
        models.retain(|entry| seen.insert(entry.selection.model.clone()));
        if protocol == ApiProtocol::Ollama
            || (probe.id == "osaurus" && protocol == ApiProtocol::ChatCompletions)
        {
            annotate_show_metadata(
                &mut models,
                &agent,
                url.strip_suffix("/api/tags").unwrap_or_else(|| {
                    url.strip_suffix("/models")
                        .unwrap_or(&url)
                        .trim_end_matches("/api")
                        .trim_end_matches("/v1")
                }),
                probe.token.as_deref(),
                &resolved_headers,
            );
        }
        if local
            && probe.id != "ollama"
            && let Some(info) = lmstudio_models_with_headers(
                &probe.spec.base_url,
                probe.token.as_deref(),
                &resolved_headers,
            )
        {
            annotate_lmstudio(&mut models, &info);
        }
        return (
            status(probe, "reachable", "Live model metadata", models.len()),
            models,
        );
    }
    (
        status(probe, "unavailable", "Model listing page limit reached", 0),
        Vec::new(),
    )
}

// Gemini ListModels uses nextPageToken/pageToken; Anthropic List Models uses
// has_more and last_id/after_id. Other protocols retain their single-response contract.
fn listing_cursor(value: &Value, protocol: ApiProtocol) -> Result<Option<String>> {
    let cursor = match protocol {
        ApiProtocol::Gemini => match value.get("nextPageToken") {
            None => return Ok(None),
            Some(Value::String(token)) if token.is_empty() => return Ok(None),
            Some(token) => token.as_str(),
        },
        ApiProtocol::Anthropic => match value.get("has_more") {
            None | Some(Value::Bool(false)) => return Ok(None),
            Some(Value::Bool(true)) => value.get("last_id").and_then(Value::as_str),
            _ => None,
        },
        _ => return Ok(None),
    };
    let cursor = cursor.filter(|cursor| {
        !cursor.is_empty() && cursor.len() <= 4096 && !cursor.chars().any(char::is_control)
    });
    Ok(Some(
        cursor
            .ok_or_else(|| anyhow::anyhow!("invalid listing cursor"))?
            .to_owned(),
    ))
}
/// Verify credentials with a provider endpoint before persisting them.
/// OpenRouter's `GET /api/v1/auth/key` is an auth check; `/models` is public and cannot validate keys.
pub fn verify_and_save_credential(
    user_dir: &Path,
    provider: &str,
    base_url: &str,
    token: &mut String,
) -> Result<()> {
    verify_and_save_credential_at(user_dir, provider, base_url, token, OPENROUTER_VALIDATE_URL)
}
pub(super) fn verify_and_save_credential_at(
    user_dir: &Path,
    provider: &str,
    base_url: &str,
    token: &mut String,
    openrouter_validate_url: &str,
) -> Result<()> {
    let result = (|| {
        valid_token(token)?;
        match provider {
            "openai" | "anthropic" | "gemini" => {
                let spec = ProviderSpec {
                    label: provider.into(),
                    base_url: base_url.into(),
                    ..ProviderSpec::default()
                };
                let mut probe = Probe {
                    id: provider.into(),
                    spec,
                    token: Some(token.clone()),
                    blocked: None,
                };
                let (status, _) = probe_models(&probe);
                let verification = match status.state.as_str() {
                    "reachable" => Ok(()),
                    "auth-required" => Err(anyhow::anyhow!(
                        "{provider} rejected this credential; check the token and endpoint"
                    )),
                    _ => Err(anyhow::anyhow!(
                        "could not verify {provider} credential: {}",
                        status.detail
                    )),
                };
                if let Some(secret) = probe.token.as_mut() {
                    wipe_token(secret);
                }
                verification?;
            }
            "openrouter" => {
                ensure!(
                    base_url == OPENROUTER_BASE_URL,
                    "OpenRouter sign-in is only valid for its official endpoint"
                );
                verify_openrouter_key_at(openrouter_validate_url, token)?;
            }
            _ => {}
        }
        save_credential(user_dir, base_url, token)
    })();
    wipe_token(token);
    result
}
fn verify_openrouter_key_at(url: &str, token: &str) -> Result<()> {
    let agent = crate::llm::agent_with(5, true, None, None);
    let mut response = agent
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .map_err(|_| anyhow::anyhow!("could not reach OpenRouter's key-validation endpoint"))?;
    match response.status().as_u16() {
        200..=299 => {
            let mut bytes = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| {
                    anyhow::anyhow!("could not read OpenRouter key-validation response")
                })?;
            ensure!(
                bytes.len() <= 64 * 1024,
                "OpenRouter key-validation response exceeded limits"
            );
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| anyhow::anyhow!("OpenRouter key-validation response was invalid"))?;
            Ok(())
        }
        401 | 403 => anyhow::bail!("OpenRouter rejected this API key"),
        status => anyhow::bail!("OpenRouter key validation returned HTTP {status}"),
    }
}
fn wipe_token(token: &mut String) {
    let mut bytes = std::mem::take(token).into_bytes();
    bytes.fill(0);
    std::hint::black_box(&bytes);
}
