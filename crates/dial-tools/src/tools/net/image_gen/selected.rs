//! Selected-model routing. Capability, endpoint and authentication must agree before spending quota.
use super::{Request, Target, Wire};
use crate::config::{paths::Layout, types::Config};
use crate::llm::{self, profiles::ImageGenerationApi};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) fn resolve(config: &Config) -> Result<Option<Target>> {
    let role_config = config.for_role("image");
    resolve_current(&role_config)
}

/// Resolve a concrete candidate without reapplying the image role.
pub(super) fn resolve_current(config: &Config) -> Result<Option<Target>> {
    let selection = config.selection();
    if !super::provider_enabled(config, &selection.provider) {
        ensure!(
            config.role_selection("image").is_none(),
            "selected image provider is disabled or its authentication method is incompatible"
        );
        return Ok(None);
    }
    let user_dir = if selection.provider == llm::chatgpt::PROVIDER {
        crate::config::paths::subscription_auth_dir(&config.user_dir)?
    } else {
        config.user_dir.clone()
    };
    let layout = Layout {
        project_root: config.project_root.clone(),
        project_dir: config.state_dir.clone(),
        state_dir: config.state_dir.clone(),
        user_dir: user_dir.clone(),
    };
    let api = config
        .active_model_profile()
        .and_then(|p| p.capabilities.image_generation)
        .or_else(|| llm::catalog::model_metadata(&layout, &selection).image_generation)
        // A hosted tool is not native image output. ChatGPT catalogs can omit this field;
        // expose an approved attempt, without rewriting model metadata or inferring from vision.
        .or_else(|| {
            (selection.provider == llm::chatgpt::PROVIDER).then_some(ImageGenerationApi::Responses)
        });
    let Some(api) = api.filter(|api| *api != ImageGenerationApi::Disabled) else {
        ensure!(
            config.role_selection("image").is_none(),
            "the assigned image model does not declare a supported image-generation capability; select an image model or clear the image role to follow main"
        );
        return Ok(None);
    };
    let endpoint = config
        .endpoint
        .as_ref()
        .context("selected image model needs an endpoint")?;
    let headers = endpoint.headers.clone().unwrap_or_default();
    llm::model_config::validate_headers(&headers)?;
    if selection.provider == llm::chatgpt::PROVIDER {
        ensure!(
            api == ImageGenerationApi::Responses,
            "ChatGPT image generation requires the Responses hosted tool"
        );
        ensure!(
            endpoint.base_url.trim_end_matches('/') == llm::chatgpt::BASE_URL && headers.is_empty(),
            "ChatGPT image generation requires its official endpoint without custom headers"
        );
        let account = llm::chatgpt::resolve_account(&user_dir, selection.account_id.as_deref())?;
        ensure!(
            config
                .chatgpt_allowed_accounts
                .as_ref()
                .is_none_or(|ids| ids.contains(&account)),
            "ChatGPT account is restricted by private global policy"
        );
        return Ok(Some(Target {
            base_url: endpoint.base_url.clone(),
            model: selection.model,
            wire: Wire::Responses,
            key: String::new(),
            account_id: Some(account),
            headers,
        }));
    }
    let base = llm::provider::endpoint_url(&endpoint.base_url, endpoint.allow_insecure)?;
    let base_url = base
        .strip_suffix("/chat/completions")
        .context("invalid selected image endpoint")?
        .to_owned();
    let key = llm::catalog::resolve_credential(
        &config.user_dir,
        &endpoint.base_url,
        endpoint.api_key_env.as_deref(),
    )?;
    let key = match key {
        Some(key) => key,
        None if llm::is_local_url(&base_url) => String::new(),
        None => anyhow::bail!(
            "selected image model has no API credential; configure its provider in /providers"
        ),
    };
    let wire = match api {
        ImageGenerationApi::Images => Wire::of(&base_url),
        ImageGenerationApi::MlxServe => Wire::MlxServe,
        ImageGenerationApi::Responses => Wire::Responses,
        ImageGenerationApi::ChatCompletions => {
            ensure!(
                matches!(
                    endpoint
                        .protocol
                        .unwrap_or_else(|| llm::model_config::ApiProtocol::builtin(
                            &selection.provider
                        )),
                    llm::model_config::ApiProtocol::ChatCompletions
                ),
                "selected model advertises image output but its native image transport is not supported; declare a supported image_generation protocol in its model profile"
            );
            Wire::ChatCompletions
        }
        ImageGenerationApi::Disabled => unreachable!("disabled capability was filtered"),
    };
    Ok(Some(Target {
        base_url,
        model: selection.model,
        wire,
        key,
        account_id: None,
        headers,
    }))
}

pub(super) fn body(target: &Target, request: &Request) -> Value {
    match target.wire {
        Wire::Responses => json!({
            "model": target.model,
            "input": [{"role":"user", "content":[{"type":"input_text", "text":request.prompt}]}],
            "instructions": "Generate exactly one image matching the user's request.",
            "tools": [{"type":"image_generation", "size":request.size, "output_format":
                match std::path::Path::new(request.output).extension().and_then(|s| s.to_str()).map(str::to_ascii_lowercase).as_deref() {
                    Some("jpg" | "jpeg") => "jpeg", Some("webp") => "webp", _ => "png"
                }}],
            "tool_choice": {"type":"image_generation"},
            "stream": true,
            "store": false
        }),
        Wire::ChatCompletions => json!({
            "model": target.model,
            "messages": [{"role":"user", "content":request.prompt}],
            "modalities": ["image", "text"],
            "image_config": {"aspect_ratio": match request.size {
                "1536x1024" => "3:2", "1024x1536" => "2:3", _ => "1:1"
            }}
        }),
        _ => unreachable!("selected body only handles Responses and chat image output"),
    }
}

pub(super) fn responses(target: &Target, config: &Config, request: &Request) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let headers = llm::model_config::resolve_headers(&target.headers)?;
    let pairs: Vec<_> = headers.pairs().collect();
    let access = if let Some(account) = &target.account_id {
        let dir = crate::config::paths::subscription_auth_dir(&config.user_dir)?;
        Some(llm::chatgpt::scoped_account_access(
            &dir,
            account,
            Arc::new(AtomicBool::new(false)),
        )?)
    } else {
        None
    };
    let token = access
        .as_ref()
        .map_or(target.key.as_str(), |(_, token)| token.as_str());
    let path = if target.account_id.is_some() {
        "codex/responses"
    } else {
        "responses"
    };
    let url = format!("{}/{path}", target.base_url.trim_end_matches('/'));
    let encoded = llm::responses::generate_image(
        llm::responses::Endpoint {
            url: &url,
            token,
            account_id: target.account_id.as_deref(),
            model: &target.model,
            headers: &pairs,
        },
        &body(target, request),
        config.image.timeout_secs,
    )
    .map_err(|error| headers.redact(error))?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Responses returned invalid base64 image data")
}

pub(super) fn chat_image(value: &Value) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let choice = &value["choices"][0];
    ensure!(
        choice["finish_reason"] == "stop",
        "image chat completion did not finish successfully"
    );
    let images = choice["message"]["images"]
        .as_array()
        .context("chat completion returned no images")?;
    ensure!(
        images.len() == 1,
        "image chat completion must return exactly one image"
    );
    let url = images[0]["image_url"]["url"]
        .as_str()
        .context("chat completion returned no inline image")?;
    let (prefix, data) = url.split_once(',').context("invalid image data URL")?;
    ensure!(
        [
            "data:image/png;base64",
            "data:image/jpeg;base64",
            "data:image/webp;base64"
        ]
        .contains(&prefix),
        "chat image must be inline PNG, JPEG or WebP base64; remote image URLs are not fetched"
    );
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("chat completion returned invalid image base64")
}
