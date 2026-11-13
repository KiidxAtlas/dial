//! Metadata-only discovery: no subnet scan, inference, or model downloads.
//! Saved credentials are plaintext JSON, protected by owner-only directories/files,
//! not an OS keychain. They are URL-scoped and written only on explicit user action.

mod bundled;
pub use bundled::{
    ModelFact, ThinkingMode, builtin_provider, bundled_model, bundled_models, known_context_window,
    model_capabilities, model_protocol, provider_endpoint,
};

mod cache;
mod credentials;
mod local_models;
mod local_servers;
mod metadata;
mod probe;
mod subscription;

use crate::config::paths::Layout;
use crate::llm::model_config::{ApiProtocol, ModelSelection, ProviderSpec};
pub use cache::saved_listing_models;
use cache::{cached, persist_cache, probe_cache_key, same_model};
pub(crate) use credentials::origin;
pub use credentials::{
    EndpointSettings, credential_token, fill_endpoint_template, known_key_env, remove_credential,
    remove_provider_credentials, resolve_credential, save_credential, set_cli_api_key,
};
use credentials::{bundled_endpoints, saved_credential_urls, validated_url};
use local_models::{annotate_embedding_checkpoints, lora_support, mlx_architectures, snapshot};
pub(crate) use local_models::{chat_template, hf_cache, local_model_entries, read_json};
pub use local_models::{
    downloaded_local_models, is_embedding_checkpoint, local_lora_support, local_model_metadata,
    resolve_local_model,
};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use local_servers::forget_llama_props;
pub use local_servers::{
    LlamaProps, LmStudioModel, OpenAiRuntime, is_llama_server, known_llama_props,
    llama_server_props, loaded_context, openai_runtime,
};
pub(crate) use local_servers::{
    lmstudio_model_with_headers, lmstudio_models_with_headers, local_get, local_request,
    record_llama_props, valid_model_id,
};
pub(crate) use metadata::{CacheWriteSplit, ONE_HOUR_WRITE_INPUT_MULTIPLIER, model_context_window};
pub use metadata::{
    DeclaredReasoning, LoraCheck, ModelEntry, ModelKind, ModelMetadata, TokenPricing,
    declared_reasoning, image_generation_api, list_pricing, model_metadata,
    remembered_declared_reasoning,
};
use probe::{Probe, prepare, probe_models, status};
pub use probe::{UNREACHABLE_DETAIL, verify_and_save_credential};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use subscription::{anthropic_subscription_catalog, oauth_catalog};

const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
const MAX_MODELS: usize = 512;
const MAX_PROVIDERS: usize = 64;
const CONCURRENCY: usize = 8;
const MAX_PAGES: usize = 16;
const LISTING_BUDGET: Duration = Duration::from_secs(5);
const ENRICHMENT_BUDGET: Duration = Duration::from_secs(1);
const MAX_ENRICHMENTS: usize = 16;
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_VALIDATE_URL: &str = "https://openrouter.ai/api/v1/auth/key";
const KNOWN: &[(&str, &str, &str, &str)] = &[
    (
        "gemini",
        "Google Gemini",
        super::gemini::BASE_URL,
        "GEMINI_API_KEY",
    ),
    (
        "openai",
        "OpenAI",
        "https://api.openai.com/v1",
        "OPENAI_API_KEY",
    ),
    (
        "anthropic",
        "Anthropic",
        "https://api.anthropic.com/v1",
        "ANTHROPIC_API_KEY",
    ),
    (
        "openrouter",
        "OpenRouter",
        OPENROUTER_BASE_URL,
        "OPENROUTER_API_KEY",
    ),
    (
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
        "GROQ_API_KEY",
    ),
    (
        "mistral",
        "Mistral",
        "https://api.mistral.ai/v1",
        "MISTRAL_API_KEY",
    ),
    (
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com/v1",
        "DEEPSEEK_API_KEY",
    ),
];
#[derive(Clone, Debug)]
pub struct ProviderStatus {
    pub id: String,
    pub label: String,
    pub base_url: Option<String>,
    pub state: String,
    pub detail: String,
    pub models: usize,
}
#[derive(Clone, Default)]
pub struct Discovery {
    pub models: Vec<ModelEntry>,
    pub providers: Vec<ProviderStatus>,
}
/// Whether `id` names a local provider Dial discovers itself (a local server, downloaded MLX/GGUF
/// weights or Apple's on-device model), so `provider/model` selectors work without configuration.
pub fn is_local_provider(id: &str) -> bool {
    matches!(id, "mlx" | "apple") || LOCAL_SERVERS.iter().any(|&(server, ..)| server == id)
}
/// Local inference servers Dial looks for: id, label, base-URL override variable, default URL.
const LOCAL_SERVERS: &[(&str, &str, &str, &str)] = &[
    (
        "lm-studio",
        "LM Studio",
        "LM_STUDIO_BASE_URL",
        "http://127.0.0.1:1234/v1",
    ),
    (
        "ollama",
        "Ollama",
        "OLLAMA_BASE_URL",
        super::ollama::BASE_URL,
    ),
    (
        "mlx-serve",
        "MLX-Serve",
        "MLX_SERVE_BASE_URL",
        "http://127.0.0.1:11234/v1",
    ),
    (
        "llama-cpp",
        "llama.cpp",
        "LLAMA_CPP_BASE_URL",
        "http://127.0.0.1:8080/v1",
    ),
    (
        "docker-model-runner",
        "Docker Model Runner",
        "DOCKER_MODEL_RUNNER_BASE_URL",
        "http://127.0.0.1:12434/engines/v1",
    ),
    (
        "osaurus",
        "Osaurus",
        "OSAURUS_BASE_URL",
        "http://127.0.0.1:1337/v1",
    ),
];
/// The base URL for one local server: its override variable, `OLLAMA_HOST` for Ollama, else the default.
fn local_server_url(id: &str, variable: &str, default: &str) -> String {
    std::env::var(variable)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if id == "ollama" {
                std::env::var("OLLAMA_HOST")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        if s.contains("://") {
                            s
                        } else {
                            format!("http://{s}")
                        }
                    })
            } else {
                None
            }
        })
        .unwrap_or_else(|| default.into())
}
/// Whether something accepts TCP connections at `base_url` within `timeout`.
fn accepts_connections(base_url: &str, timeout: Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(uri) = crate::url::parse_url(base_url) else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, timeout).is_ok())
}
/// Labels of local inference servers that accept a connection right now, in [`LOCAL_SERVERS`]
/// order. A reachability hint for first-run setup only: it does not list models or check the
/// protocol, and it never selects a model.
pub fn running_local_servers(timeout: Duration) -> Vec<&'static str> {
    std::thread::scope(|scope| {
        let probes: Vec<_> = LOCAL_SERVERS
            .iter()
            .map(|&(id, label, variable, default)| {
                scope.spawn(move || {
                    accepts_connections(&local_server_url(id, variable, default), timeout)
                        .then_some(label)
                })
            })
            .collect();
        probes
            .into_iter()
            .filter_map(|probe| probe.join().ok().flatten())
            .collect()
    })
}
pub fn discover(
    layout: &Layout,
    configured: &BTreeMap<String, ProviderSpec>,
    selections: &[ModelSelection],
) -> Discovery {
    let trainable = mlx_architectures(&layout.user_dir);
    let mut discovery = Discovery {
        models: downloaded_local_models(&layout.user_dir),
        ..Discovery::default()
    };
    discovery.providers.push(ProviderStatus {
        id: "mlx".into(),
        label: "Local models".into(),
        base_url: None,
        state: if discovery.models.is_empty() {
            "unavailable"
        } else {
            "cached"
        }
        .into(),
        detail: "Downloaded MLX and GGUF models, loaded on demand; nothing is downloaded".into(),
        models: discovery.models.len(),
    });
    if let Some((status, entry)) = super::apple::discover() {
        discovery.providers.push(status);
        discovery.models.extend(entry);
    }
    let mut endpoints = BTreeMap::new();
    for &(id, label, variable, default) in LOCAL_SERVERS {
        endpoints.insert(
            id.to_owned(),
            ProviderSpec {
                label: label.into(),
                base_url: local_server_url(id, variable, default),
                protocol: Some(ApiProtocol::builtin(id)),
                ..ProviderSpec::default()
            },
        );
    }
    for &(id, label, base, env) in KNOWN {
        endpoints.insert(
            id.into(),
            ProviderSpec {
                label: label.into(),
                base_url: base.into(),
                api_key_env: Some(env.into()),
                protocol: Some(ApiProtocol::builtin(id)),
                ..ProviderSpec::default()
            },
        );
    }
    let (bundled_probes, not_set_up) = bundled_endpoints(
        |id| endpoints.contains_key(id) || configured.contains_key(id),
        &saved_credential_urls(&layout.user_dir),
        |key| std::env::var_os(key).is_some_and(|value| !value.is_empty()),
    );
    endpoints.extend(bundled_probes);
    for (id, spec) in configured {
        // Keep built-in labels and setup URLs, but never probe a reset provider.
        let endpoint = if spec.is_reset() {
            let mut endpoint = endpoints.get(id).cloned().unwrap_or_else(|| ProviderSpec {
                label: bundled::builtin_provider(id)
                    .map(|p| p.label.clone())
                    .unwrap_or_else(|| id.clone()),
                base_url: provider_endpoint(id)
                    .map(|p| p.0)
                    .unwrap_or_default()
                    .to_owned(),
                ..Default::default()
            });
            endpoint.enabled = false;
            endpoint
        } else {
            spec.clone()
        };
        endpoints.insert(id.clone(), endpoint);
    }
    let mut endpoints: Vec<_> = endpoints.into_iter().collect();
    for selection in selections {
        if let Some(base) = &selection.base_url {
            if configured
                .get(&selection.provider)
                .is_some_and(|spec| !spec.enabled)
            {
                continue;
            }
            let inherited = configured
                .get(&selection.provider)
                .filter(|spec| spec.base_url.trim_end_matches('/') == base.trim_end_matches('/'));
            let headers = selection
                .headers
                .as_ref()
                .or_else(|| inherited.and_then(|spec| spec.headers.as_ref()));
            let api_key_env = selection
                .api_key_env
                .as_ref()
                .or_else(|| inherited.and_then(|spec| spec.api_key_env.as_ref()));
            if endpoints.iter().any(|(id, spec)| {
                id == &selection.provider
                    && &spec.base_url == base
                    && spec.protocol.unwrap_or_else(|| ApiProtocol::builtin(id))
                        == selection
                            .protocol
                            .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider))
                    && spec.headers.as_deref().unwrap_or_default()
                        == headers.map(Vec::as_slice).unwrap_or_default()
                    && spec.api_key_env.as_ref() == api_key_env
            }) {
                continue;
            }
            endpoints.push((
                selection.provider.clone(),
                ProviderSpec {
                    label: selection.provider.clone(),
                    base_url: base.clone(),
                    api_key_env: api_key_env.cloned(),
                    auth: configured
                        .get(&selection.provider)
                        .map(|spec| spec.auth)
                        .unwrap_or_default(),
                    allow_insecure: selection.allow_insecure,
                    protocol: selection.protocol,
                    headers: headers.cloned(),
                    ..ProviderSpec::default()
                },
            ));
        }
    }
    // A signed-in subscription provider that is configured at its canonical endpoint with no API
    // key cannot be probed (the grant is attached only at inference). Leaving it in the probe
    // list reports it unavailable and hides its bundled models, so `oauth_catalog` serves it.
    endpoints.retain(|(id, spec)| {
        !(spec.enabled
            && spec.auth != super::model_config::AuthMethod::ApiKey
            && (spec.auth == super::model_config::AuthMethod::OAuth || spec.api_key_env.is_none())
            && spec.headers.as_ref().is_none_or(|h| h.is_empty())
            && super::oauth::selected_auth(&layout.user_dir, id, spec.auth).unwrap_or(false)
            && super::oauth::flow(id)
                .is_some_and(|flow| super::oauth::same_origin(&spec.base_url, flow.base_url)))
    });
    let storage_safe = layout.ensure_project_outside_storage().is_ok();
    if !storage_safe {
        discovery.providers.push(ProviderStatus {
            id: "storage".into(),
            label: "Private storage".into(),
            base_url: None,
            state: "unavailable".into(),
            detail: "Project must not be inside private storage; endpoint discovery skipped".into(),
            models: 0,
        });
    } else {
        if configured
            .get(super::chatgpt::PROVIDER)
            .is_some_and(|spec| !spec.enabled)
        {
            discovery.providers.push(ProviderStatus {
                id: super::chatgpt::PROVIDER.into(),
                label: "ChatGPT subscription".into(),
                base_url: Some(super::chatgpt::BASE_URL.into()),
                state: "disabled".into(),
                detail: "Disabled in configuration".into(),
                models: 0,
            });
        } else if configured
            .get(super::chatgpt::PROVIDER)
            .is_some_and(|spec| spec.headers.as_ref().is_some_and(|h| !h.is_empty()))
            || selections.iter().any(|selection| {
                selection.provider == super::chatgpt::PROVIDER
                    && selection.headers.as_ref().is_some_and(|h| !h.is_empty())
            })
        {
            discovery.providers.push(ProviderStatus {
                id: super::chatgpt::PROVIDER.into(),
                label: "ChatGPT subscription".into(),
                base_url: Some(super::chatgpt::BASE_URL.into()),
                state: "unavailable".into(),
                detail: "Subscription does not permit custom request headers".into(),
                models: 0,
            });
        } else {
            let (chatgpt, models) = super::chatgpt::discover(&layout.user_dir);
            discovery.providers.push(chatgpt);
            discovery.models.extend(models);
        }
        for (status, models) in oauth_catalog(&layout.user_dir, &endpoints) {
            discovery.providers.push(status);
            discovery.models.extend(models);
        }
        let cache = cached(&layout.user_dir);
        let probes: Vec<_> = endpoints
            .into_iter()
            .filter(|(id, _)| id != super::chatgpt::PROVIDER)
            .enumerate()
            .map(|(index, (id, spec))| {
                if index >= MAX_PROVIDERS {
                    Probe {
                        id,
                        spec,
                        token: None,
                        blocked: Some(("unavailable", "Endpoint probe limit reached")),
                    }
                } else {
                    prepare(id, spec, &layout.user_dir)
                }
            })
            .collect();
        let mut updates = BTreeMap::new();
        // At most eight simultaneous probes, each with a total request deadline.
        for batch in probes.chunks(CONCURRENCY) {
            let results = std::thread::scope(|scope| {
                let jobs: Vec<_> = batch
                    .iter()
                    .map(|probe| scope.spawn(move || probe_models(probe)))
                    .collect();
                jobs.into_iter()
                    .zip(batch)
                    .map(|(job, probe)| {
                        job.join().unwrap_or_else(|_| {
                            (
                                status(probe, "unavailable", "Metadata worker failed", 0),
                                Vec::new(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            });
            for ((mut status, mut models), probe) in results.into_iter().zip(batch) {
                if status.state == "reachable" {
                    updates.insert(probe_cache_key(probe), models.clone());
                } else if status.state == "unavailable" {
                    let saved = cache.get(&probe_cache_key(probe)).map(Vec::as_slice);
                    if let Some(subscription_models) =
                        anthropic_subscription_catalog(probe, &mut status, saved)
                    {
                        models = subscription_models;
                    } else if status.state != "disabled"
                        && let Some(stale) = cache.get(&probe_cache_key(probe))
                    {
                        models = stale
                            .iter()
                            .filter(|entry| {
                                entry.selection.protocol.unwrap_or_else(|| {
                                    ApiProtocol::builtin(&entry.selection.provider)
                                }) == probe
                                    .spec
                                    .protocol
                                    .unwrap_or_else(|| ApiProtocol::builtin(&probe.id))
                            })
                            .take(MAX_MODELS)
                            .cloned()
                            .map(|mut entry| {
                                entry.available = false;
                                entry.source = format!("{} (cached, unverified)", probe.spec.label);
                                entry.selection.provider = probe.id.clone();
                                entry.selection.api_key_env = probe.spec.api_key_env.clone();
                                entry.selection.allow_insecure = probe.spec.allow_insecure;
                                entry.selection.headers = probe.spec.headers.clone();
                                entry
                            })
                            .collect();
                        if !models.is_empty() {
                            status
                                .detail
                                .push_str("; showing cached metadata as unverified");
                            status.models = models.len();
                        }
                    }
                } else if status.state != "disabled"
                    && let Some(stale) = cache.get(&probe_cache_key(probe))
                {
                    models =
                        stale
                            .iter()
                            .filter(|entry| {
                                entry.selection.protocol.unwrap_or_else(|| {
                                    ApiProtocol::builtin(&entry.selection.provider)
                                }) == probe
                                    .spec
                                    .protocol
                                    .unwrap_or_else(|| ApiProtocol::builtin(&probe.id))
                            })
                            .take(MAX_MODELS)
                            .cloned()
                            .map(|mut entry| {
                                entry.available = false;
                                entry.source = format!("{} (cached, unverified)", probe.spec.label);
                                entry.selection.provider = probe.id.clone();
                                entry.selection.api_key_env = probe.spec.api_key_env.clone();
                                entry.selection.allow_insecure = probe.spec.allow_insecure;
                                entry.selection.headers = probe.spec.headers.clone();
                                entry
                            })
                            .collect();
                    if !models.is_empty() {
                        status
                            .detail
                            .push_str("; showing cached metadata as unverified");
                        status.models = models.len();
                    }
                }
                discovery.providers.push(status);
                discovery.models.extend(models);
            }
        }
        if persist_cache(&layout.user_dir, updates).is_err() {
            discovery.providers.push(ProviderStatus {
                id: "catalog-cache".into(),
                label: "Metadata cache".into(),
                base_url: None,
                state: "unavailable".into(),
                detail: "Live metadata could not be saved privately".into(),
                models: 0,
            });
        }
    }
    for status in not_set_up {
        if !discovery.providers.iter().any(|p| p.id == status.id) {
            discovery.providers.push(status);
        }
    }
    if storage_safe {
        let mut accounts = std::collections::BTreeSet::new();
        for selection in selections {
            if selection.provider == super::chatgpt::PROVIDER
                && let Some(id) = selection.account_id.as_deref()
                && accounts.insert(id)
                && let Ok(models) = super::chatgpt::scoped_cached_models(&layout.user_dir, id)
            {
                for entry in models {
                    if !discovery
                        .models
                        .iter()
                        .any(|m| same_model(&m.selection, &entry.selection))
                    {
                        discovery.models.push(entry);
                    }
                }
            }
        }
    }
    for selection in selections {
        if selection.model.trim().is_empty() {
            continue;
        }
        let mut selection = selection.clone();
        if let Some(spec) = configured.get(&selection.provider).filter(|spec| {
            selection.base_url.as_deref().is_some_and(|base| {
                base.trim_end_matches('/') == spec.base_url.trim_end_matches('/')
            })
        }) {
            if selection.headers.is_none() {
                selection.headers = spec.headers.clone();
            }
            if selection.api_key_env.is_none() {
                selection.api_key_env = spec.api_key_env.clone();
            }
        }
        if discovery
            .models
            .iter()
            .any(|m| same_model(&m.selection, &selection))
        {
            continue;
        }
        if selection
            .base_url
            .as_deref()
            .is_some_and(|url| validated_url(url, true).is_err())
        {
            selection.base_url = None;
        }
        let local = selection.provider == "mlx" && selection.base_url.is_none();
        let available = local
            && (snapshot(Path::new(&selection.model)).is_some()
                || super::local_format::gguf_file(Path::new(&selection.model)).is_some());
        let lora = if local {
            lora_support(trainable.as_deref(), Path::new(&selection.model))
        } else {
            None
        };
        let capabilities = if local {
            local_model_metadata(Path::new(&selection.model))
        } else {
            ModelMetadata::default()
        };
        discovery.models.push(ModelEntry {
            label: selection.model.clone(),
            selection,
            source: if available {
                "Configured local model"
            } else {
                "Configured (unverified)"
            }
            .into(),
            available,
            metadata: ModelMetadata {
                lora,
                ..capabilities
            },
        });
    }
    // Explicitly reset providers stay unconfigured even with running local servers,
    // environment credentials, downloaded weights or saved model selections.
    discovery.models.retain(|entry| {
        !configured
            .get(&entry.selection.provider)
            .is_some_and(ProviderSpec::is_reset)
    });
    for status in &mut discovery.providers {
        if configured
            .get(&status.id)
            .is_some_and(ProviderSpec::is_reset)
        {
            status.state = "not-set-up".into();
            status.detail = "Removed from Dial; set up this provider to connect again".into();
            status.models = 0;
        }
    }
    annotate_embedding_checkpoints(&mut discovery.models);
    discovery
}
#[cfg(test)]
pub(crate) mod tests;
