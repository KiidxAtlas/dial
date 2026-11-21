//! Selected-model routing. Capability, endpoint and authentication must agree before spending quota.
use super::{Request, Target, Wire};
use crate::config::{paths::Layout, types::Config};
use crate::llm::{self, profiles::ImageGenerationApi};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, atomic::AtomicBool};

/// ChatGPT sign-ins whose account refused the hosted image tool in this process, keyed by
/// subscription sign-in directory and account id. Free plans can lack the tool while the model
/// catalog says nothing, so the first refusal withdraws `image_gen` for that account until Dial
/// restarts instead of failing every later call the same way.
static REFUSED: LazyLock<Mutex<HashSet<(PathBuf, String)>>> = LazyLock::new(Default::default);

/// ChatGPT refused its hosted image tool for the signed-in account. Context on the provider
/// error when first seen; the resolution error afterwards. `auto_tools` stops offering on it.
#[derive(Debug, Clone, Copy)]
pub(super) struct HostedImageRefused;

impl std::fmt::Display for HostedImageRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this ChatGPT account does not offer the hosted image tool (free plans may not include \
             it); image_gen is withdrawn for this account until Dial restarts. Assign an image role \
             in /models or configure an image provider in /providers to generate images",
        )
    }
}

impl std::error::Error for HostedImageRefused {}

fn refused(dir: &Path, account: &str) -> bool {
    REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&(dir.to_path_buf(), account.to_owned()))
        || llm::catalog::image_capabilities::account_refused(dir, llm::chatgpt::PROVIDER, account)
}

pub(super) fn record_refusal(dir: &Path, account: &str) {
    REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((dir.to_path_buf(), account.to_owned()));
    let _ = llm::catalog::image_capabilities::record_account_refused(
        dir,
        llm::chatgpt::PROVIDER,
        account,
        "ChatGPT account refused the hosted image_generation tool",
    );
}

/// Clear both in-process and persisted hosted-image refusals so a user-requested re-check
/// can evaluate accounts and models again.
pub fn clear_refusals(user_dir: &Path) {
    let auth_dir = crate::config::paths::subscription_auth_dir(user_dir).ok();
    {
        let mut guard = REFUSED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .retain(|(dir, _)| dir != user_dir && auth_dir.as_ref().is_none_or(|auth| dir != auth));
    }
    let _ = llm::catalog::image_capabilities::clear_refusals(user_dir);
    if let Some(auth) = auth_dir
        && auth != user_dir
    {
        let _ = llm::catalog::image_capabilities::clear_refusals(&auth);
    }
}

/// The Responses API's 400 for a forced `tool_choice` whose hosted tool the account lacks:
/// "Tool choice 'image_generation' not found in 'tools' parameter."
pub(super) fn refuses_hosted_tool(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("http 400")
        && text.contains("tool choice")
        && text.contains("image_generation")
        && text.contains("not found")
}

pub(super) fn resolve(config: &Config) -> Result<Option<Target>> {
    let role_config = config.for_role("image");
    resolve_current(&role_config)
}

/// A route to a server Dial starts for one call ([`super::LAUNCHED_BY_DIAL`]).
fn launched(model: String, wire: Wire) -> Target {
    Target {
        base_url: super::LAUNCHED_BY_DIAL.into(),
        model,
        wire,
        key: String::new(),
        account_id: None,
        headers: Vec::new(),
        settings: Default::default(),
    }
}

/// Resolve a concrete candidate without reapplying the image role.
pub(super) fn resolve_current(config: &Config) -> Result<Option<Target>> {
    let selection = config.selection();
    // A downloaded model (a path, no endpoint) runs on a server Dial launches for each call: MLX
    // image packs on MLX-Serve (Apple Silicon only), other checkpoints on stable-diffusion.cpp.
    if selection.provider == "mlx" && selection.base_url.is_none() {
        let model = Path::new(&selection.model);
        if llm::server::image::runs_on_mlx_serve(model) {
            ensure!(
                llm::server::image::mlx_serve_supported(),
                "{} is an MLX image pack: it runs on MLX-Serve, which needs macOS with Apple Silicon. Download the original safetensors of this model to run it on stable-diffusion.cpp",
                model.display()
            );
            return Ok(Some(launched(selection.model, Wire::MlxServe)));
        }
        if llm::server::image::is_checkpoint_path(model) {
            llm::server::image::components(model)?;
            return Ok(Some(launched(selection.model, Wire::SdCpp)));
        }
    }
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
        if refused(&user_dir, &account) {
            return Err(HostedImageRefused.into());
        }
        return Ok(Some(Target {
            base_url: endpoint.base_url.clone(),
            model: selection.model,
            wire: Wire::Responses,
            key: String::new(),
            account_id: Some(account),
            headers,
            settings: Default::default(),
        }));
    }
    let base_url = if matches!(
        api,
        ImageGenerationApi::Images
            | ImageGenerationApi::MlxServe
            | ImageGenerationApi::Responses
            | ImageGenerationApi::ChatCompletions
    ) {
        let base = llm::provider::endpoint_url(&endpoint.base_url, endpoint.allow_insecure)?;
        base.strip_suffix("/chat/completions")
            .context("invalid selected image endpoint")?
            .to_owned()
    } else {
        super::native::validate_base(&endpoint.base_url)?;
        endpoint.base_url.trim_end_matches('/').to_owned()
    };
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
        ImageGenerationApi::Gemini
        | ImageGenerationApi::Imagen
        | ImageGenerationApi::BlackForestLabs
        | ImageGenerationApi::ComfyUi
        | ImageGenerationApi::Fal
        | ImageGenerationApi::Replicate => Wire::configured(api, &base_url)?,
        ImageGenerationApi::Disabled => unreachable!("disabled capability was filtered"),
    };
    Ok(Some(Target {
        base_url,
        model: selection.model,
        wire,
        key,
        account_id: None,
        headers,
        settings: Default::default(),
    }))
}

pub(super) fn body(target: &Target, request: &Request) -> Value {
    match target.wire {
        Wire::Responses => {
            let mut tool = json!({"type":"image_generation", "size":request.size, "output_format":
            match std::path::Path::new(request.output).extension().and_then(|s| s.to_str()).map(str::to_ascii_lowercase).as_deref() {
                Some("jpg" | "jpeg") => "jpeg", Some("webp") => "webp", _ => "png"
            }});
            super::apply_settings(target, &mut tool, request.source.is_some());
            if let Some(animation) = request.animation {
                // The hosted tool picks its own image model unless told; GIF needs GPT Image 2.6.
                tool["model"] = json!(super::GIF_MODEL);
                animation.apply(&mut tool);
            }
            let mut content = vec![json!({"type":"input_text", "text":request.prompt})];
            if let Some(source) = &request.source {
                content.push(json!({"type":"input_image", "image_url":source.data_url()}));
            }
            let instructions = match (&request.source, request.animation) {
                (Some(_), Some(_)) => {
                    "Animate the provided image as the user requests and return exactly one animated GIF."
                }
                (None, Some(_)) => "Generate exactly one animated GIF matching the user's request.",
                (Some(_), None) => {
                    "Edit the provided image as the user requests and return exactly one image."
                }
                (None, None) => "Generate exactly one image matching the user's request.",
            };
            json!({
                "model": target.model,
                "input": [{"role":"user", "content":content}],
                "instructions": instructions,
                "tools": [tool],
                "tool_choice": {"type":"image_generation"},
                "stream": true,
                "store": false
            })
        }
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

pub(super) fn responses(
    target: &Target,
    config: &Config,
    request: &Request,
) -> Result<Vec<Vec<u8>>> {
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
    .map_err(|error| headers.redact(error));
    let encoded = match (encoded, &target.account_id) {
        (Err(error), Some(account)) if refuses_hosted_tool(&error) => {
            let dir = crate::config::paths::subscription_auth_dir(&config.user_dir)?;
            record_refusal(&dir, account);
            return Err(error.context(HostedImageRefused));
        }
        (encoded, _) => encoded?,
    };
    encoded
        .iter()
        .map(|image| {
            base64::engine::general_purpose::STANDARD
                .decode(image)
                .context("Responses returned invalid base64 image data")
        })
        .collect()
}

pub(super) fn chat_image(value: &Value) -> Result<Vec<Vec<u8>>> {
    use base64::Engine as _;
    let choice = &value["choices"][0];
    ensure!(
        choice["finish_reason"] == "stop",
        "image chat completion did not finish successfully"
    );
    let images = choice["message"]["images"]
        .as_array()
        .filter(|images| !images.is_empty())
        .context("chat completion returned no images")?;
    images
        .iter()
        .map(|image| {
            let url = image["image_url"]["url"]
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
        })
        .collect()
}
