//! `image_gen`: one generated or edited image from the resolved image service, written to a new
//! workspace file.
//!
//! Explicit image settings and roles win. Otherwise use main's image route, its provider's image
//! default, configured image profiles, then keyed provider defaults. Tool availability is independent
//! of main's vision capability. Automatic offering reads local state only; calls require approval.
//!
//! Per-model settings (`[image.models.*]`, edited by `/image settings`) apply to every request for
//! the matching model; each image API receives only the settings it accepts (`supported`).
//!
//! Edits (`image`): MLX-Serve accepts base64 input on `images/generations`, OpenAI Images uses
//! multipart `images/edits`, Responses gets `input_image`, and Gemini gets `inlineData`.
//! Other routes refuse before any request. The source file is only read.
//!
//! Request validation, path resolution, destination preflight and mutation authority run before
//! generation. Provider errors or a later filesystem race can still fail a paid request. Returned
//! and source images are decoded with resource limits; existing files are never overwritten.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::config::image::{ImageConfig, ImageModelSettings, SIZES};
use crate::config::types::Config;
use crate::storage::redact::redact_text;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, tool_schema};

mod comfyui;
mod google;
mod native;
mod queued;
mod selected;

pub const TOOL: &str = "image_gen";
const DEFAULT_SIZE: &str = "1024x1024";
const MAX_PROMPT_BYTES: usize = 4_000;
/// Largest source image an edit uploads (OpenAI's gpt-image edit limit is 50 MB).
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;
const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const DESCRIPTION: &str = "Generate one image, or edit an existing workspace image passed as \
    `image`, through the resolved image service, independently of the main chat model's \
    image-output capability, and write it to a new workspace file. Explicit image settings/role \
    win; otherwise available configured routes are selected automatically. ChatGPT hosted-image \
    support may be unverified for the model/account; provider rejection is reported without \
    retrying another billing route. Output is PNG, JPEG, or WebP, chosen by the output extension; \
    valid provider images are converted locally when needed (JPEG is lossy and drops transparency). \
    Existing files are never overwritten and the source image is only read. The image is \
    attached for vision-capable models to inspect and revise into a new file. Requires approval.";

/// Hosted session providers whose `images/generations` route returns base64 images:
/// (API host, default image model, request shape).
const PROVIDERS: &[(&str, &str, Wire)] = &[
    ("api.openai.com", "gpt-image-1.5", Wire::OpenAi),
    ("api.x.ai", "grok-imagine-image-2.0", Wire::Xai),
];

/// Registered through `integrations = ["image_gen"]`.
pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ImageGen)]
}

/// Offer image generation independently of the chat model, without network probes.
/// Missing automatic routes stay callable so generation can report the setup requirement.
///
/// # Errors
/// Returns failures for customized image settings, assigned roles and selected image routes.
pub fn auto_tools(config: &Config) -> Result<Vec<Arc<dyn Tool>>> {
    if !config.image.auto {
        return Ok(Vec::new());
    }
    match resolve(config) {
        Ok(_) => Ok(tools(config)),
        Err(error)
            if customized(&config.image)
                || config.role_selection("image").is_some()
                || config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER) =>
        {
            Err(error)
        }
        Err(_) => Ok(tools(config)),
    }
}

/// The default file extension for the resolved route, read from local state only (no generation
/// or network probe). xAI defaults to JPEG; other routes default to PNG, converting locally if needed.
///
/// # Errors
/// Returns the same route failures generation would report.
pub fn output_extension(config: &Config) -> Result<&'static str> {
    Ok(match resolve(config)?.wire {
        Wire::Xai => "jpg",
        _ => "png",
    })
}

/// The image model the next generation would use and the settings its API accepts, read from
/// local state only (no generation or network probe). `/image settings` edits these.
pub struct ImageRoute {
    pub model: String,
    /// Short name of the image API, for display.
    pub api: &'static str,
    /// `ImageModelSettings` fields this API receives; others are kept but not sent.
    pub settings: &'static [&'static str],
    /// Whether this route can edit an existing image.
    pub edits: bool,
}

/// # Errors
/// Returns the same route failures generation would report.
pub fn current_route(config: &Config) -> Result<ImageRoute> {
    let target = resolve(config)?;
    Ok(ImageRoute {
        api: match target.wire {
            Wire::OpenAi => "OpenAI Images",
            Wire::Xai => "xAI Images",
            Wire::MlxServe => "MLX-Serve",
            Wire::Responses => "Responses image tool",
            Wire::ChatCompletions => "Chat image output",
            Wire::Gemini => "Gemini native image",
            Wire::Imagen => "Imagen prediction",
            Wire::BlackForestLabs => "Black Forest Labs",
            Wire::ComfyUi => "ComfyUI workflow",
            Wire::Fal => "fal queue",
            Wire::Replicate => "Replicate prediction",
        },
        settings: supported(target.wire, &target.model),
        edits: edit_unsupported(&target).is_none(),
        model: target.model,
    })
}

/// Why `target` cannot edit an existing image, when it cannot.
fn edit_unsupported(target: &Target) -> Option<String> {
    match target.wire {
        Wire::MlxServe | Wire::Responses | Wire::Gemini => None,
        Wire::OpenAi if target.model.starts_with("dall-e-3") => Some(format!(
            "{} cannot edit images; use gpt-image-1 or dall-e-2",
            target.model
        )),
        Wire::OpenAi => None,
        Wire::Xai
        | Wire::ChatCompletions
        | Wire::Imagen
        | Wire::BlackForestLabs
        | Wire::ComfyUi
        | Wire::Fal
        | Wire::Replicate => Some(format!(
            "image edits are not supported on {}'s image route; use an OpenAI Images, MLX-Serve, Gemini or Responses image model",
            target.model
        )),
    }
}

/// Per-model settings each image API accepts. `size` is the default for calls that omit it.
fn supported(wire: Wire, model: &str) -> &'static [&'static str] {
    match wire {
        Wire::MlxServe => &["size", "steps", "seed", "strength", "loras", "cond_weights"],
        Wire::OpenAi if model.starts_with("dall-e") => &["size", "quality"],
        Wire::OpenAi | Wire::Responses => &["size", "quality", "background", "moderation"],
        Wire::Xai | Wire::ChatCompletions => &["size"],
        Wire::Gemini | Wire::Imagen => &["size"],
        Wire::BlackForestLabs if model.starts_with("flux-2-") => &["size", "seed"],
        Wire::BlackForestLabs => &["seed"],
        Wire::Fal | Wire::Replicate => &["seed", "input"],
        Wire::ComfyUi => &["comfyui"],
    }
}

/// Adds the target's per-model settings that its API accepts (except `size`, which the request
/// carries) to a request object: the body, or the Responses image tool. `strength` applies only
/// to edits.
fn apply_settings(target: &Target, object: &mut Value, edit: bool) {
    let settings = &target.settings;
    for name in supported(target.wire, &target.model) {
        let value = match *name {
            "steps" => settings.steps.map(Value::from),
            "seed" => settings.seed.map(Value::from),
            "strength" => settings.strength.filter(|_| edit).map(Value::from),
            "quality" => settings.quality.clone().map(Value::from),
            "background" => settings.background.clone().map(Value::from),
            "moderation" => settings.moderation.clone().map(Value::from),
            "loras" if !settings.loras.is_empty() => {
                object["lora_paths"] = settings
                    .loras
                    .iter()
                    .map(|lora| {
                        crate::config::image::lora_path(&lora.path)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                object["lora_scales"] = settings.loras.iter().map(|lora| lora.scale).collect();
                None
            }
            "cond_weights" if !settings.cond_weights.is_empty() => {
                object["cond_weights"] = settings.cond_weights.iter().copied().collect();
                None
            }
            _ => None,
        };
        if let Some(value) = value {
            object[*name] = value;
        }
    }
}

/// The configured default size, when the target's API accepts one from the settings.
fn configured_size(target: &Target) -> Option<&'static str> {
    if !supported(target.wire, &target.model).contains(&"size") {
        return None;
    }
    let size = target.settings.size.as_deref()?;
    SIZES.iter().copied().find(|known| *known == size)
}

/// Whether the user changed the image endpoint, model or key variable from the defaults.
fn customized(image: &ImageConfig) -> bool {
    let defaults = ImageConfig::default();
    image.base_url != defaults.base_url
        || image.model != defaults.model
        || image.api.is_some()
        || image.api_key_env.is_some()
}

/// Request body shape of an `images/generations` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// OpenAI: `size`; DALL·E models need `response_format`, gpt-image models reject it.
    OpenAi,
    /// xAI: `aspect_ratio` instead of `size`, and `response_format` for base64 data.
    Xai,
    /// Native MLX-Serve Images API, returning PNG only.
    MlxServe,
    Responses,
    ChatCompletions,
    Gemini,
    Imagen,
    BlackForestLabs,
    ComfyUi,
    Fal,
    Replicate,
}

impl Wire {
    fn of(base_url: &str) -> Self {
        host(base_url)
            .and_then(|host| PROVIDERS.iter().find(|(known, _, _)| *known == host))
            .map_or(Self::OpenAi, |(_, _, wire)| *wire)
    }

    fn configured(api: crate::llm::profiles::ImageGenerationApi, base_url: &str) -> Result<Self> {
        use crate::llm::profiles::ImageGenerationApi as Api;
        Ok(match api {
            Api::Images => Self::of(base_url),
            Api::MlxServe => Self::MlxServe,
            Api::Responses => Self::Responses,
            Api::ChatCompletions => Self::ChatCompletions,
            Api::Gemini => Self::Gemini,
            Api::Imagen => Self::Imagen,
            Api::BlackForestLabs => Self::BlackForestLabs,
            Api::ComfyUi => Self::ComfyUi,
            Api::Fal => Self::Fal,
            Api::Replicate => Self::Replicate,
            Api::Disabled => bail!("image generation is explicitly disabled"),
        })
    }
}

fn host(url: &str) -> Option<String> {
    url.parse::<ureq::http::Uri>()
        .ok()?
        .host()
        .map(str::to_ascii_lowercase)
}

/// The endpoint, model and key one generation uses. No `Debug`: it holds the key.
struct Target {
    base_url: String,
    model: String,
    wire: Wire,
    key: String,
    account_id: Option<String>,
    headers: Vec<crate::llm::model_config::RequestHeader>,
    /// `[image.models.*]` settings for `model`; empty when none are configured.
    settings: ImageModelSettings,
}

/// The resolved route with its per-model settings attached.
fn resolve(config: &Config) -> Result<Target> {
    let mut target = route(config)?;
    native::validate_base(&target.base_url)?;
    if let Some(settings) = config.image.settings_for(&target.model) {
        target.settings = settings.clone();
    }
    if target.settings != ImageModelSettings::default() {
        target.settings.validate()?;
    }
    Ok(target)
}

/// Resolves the image endpoint and its key (see the module docs for the order).
fn route(config: &Config) -> Result<Target> {
    let image = &config.image;
    let customized = customized(image);
    if customized && image.api.is_some() {
        // Native services need not expose a chat endpoint. The explicit API is authoritative.
        let wire = image.api.map_or_else(
            || Ok(Wire::of(&image.base_url)),
            |api| Wire::configured(api, &image.base_url),
        )?;
        let key = crate::llm::catalog::resolve_credential(
            &config.user_dir,
            &image.base_url,
            image.api_key_env.as_deref(),
        )?;
        ensure!(
            key.is_some() || crate::llm::is_local_url(&image.base_url),
            "no API key for image generation; save one through /providers or set image.api_key_env"
        );
        return Ok(Target {
            base_url: image.base_url.clone(),
            model: image.model.clone(),
            wire,
            key: key.unwrap_or_default(),
            account_id: None,
            headers: Vec::new(),
            settings: ImageModelSettings::default(),
        });
    }
    let mut selected_error = None;
    if !customized {
        match selected::resolve(config) {
            Ok(Some(target)) => return Ok(target),
            Err(error) if config.role_selection("image").is_some() => return Err(error),
            Err(error) => selected_error = Some(error),
            Ok(None) => {}
        }
        ensure!(
            config.role_selection("image").is_none(),
            "the assigned image model is unavailable"
        );
    }
    let session = config
        .endpoint
        .as_ref()
        .filter(|_| !customized)
        .filter(|_| provider_enabled(config, config.provider.as_deref().unwrap_or_default()))
        .and_then(|endpoint| {
            let host = host(&endpoint.base_url)?;
            let (_, model, wire) = PROVIDERS.iter().find(|(known, _, _)| *known == host)?;
            let key = credential(config, &endpoint.base_url, endpoint.api_key_env.as_deref())?;
            Some(Target {
                base_url: endpoint.base_url.clone(),
                model: (*model).to_owned(),
                wire: *wire,
                key,
                account_id: None,
                headers: Vec::new(),
                settings: ImageModelSettings::default(),
            })
        });
    if let Some(target) = session {
        return Ok(target);
    }
    if !customized && let Some(target) = configured_target(config) {
        return Ok(target);
    }
    let key = if customized || provider_enabled(config, "openai") {
        crate::llm::catalog::resolve_credential(
            &config.user_dir,
            image.base_url.trim_end_matches('/'),
            image.api_key_env.as_deref(),
        )
    } else {
        Ok(None)
    };
    let key = match key {
        Ok(Some(key)) => key,
        missing if !customized => {
            if provider_enabled(config, "xai")
                && let Some(key) = credential(config, "https://api.x.ai/v1", None)
            {
                return Ok(Target {
                    base_url: "https://api.x.ai/v1".into(),
                    model: "grok-imagine-image-2.0".into(),
                    wire: Wire::Xai,
                    key,
                    account_id: None,
                    headers: Vec::new(),
                    settings: ImageModelSettings::default(),
                });
            }
            if let Some(error) = selected_error {
                return Err(error);
            }
            missing?.context("no image generation route; assign an image role in /models or configure an image provider")?
        }
        missing => missing?.context(
            "no API key for image generation; save one through /providers or set image.api_key_env",
        )?,
    };
    Ok(Target {
        base_url: image.base_url.clone(),
        model: image.model.clone(),
        wire: Wire::of(&image.base_url),
        key,
        account_id: None,
        headers: Vec::new(),
        settings: ImageModelSettings::default(),
    })
}

/// Respect provider opt-outs before inspecting any credentials for automatic routes.
fn provider_enabled(config: &Config, provider: &str) -> bool {
    use crate::llm::{chatgpt, model_config::AuthMethod};
    config.providers.get(provider).is_none_or(|spec| {
        spec.enabled
            && if provider == chatgpt::PROVIDER {
                spec.auth != AuthMethod::ApiKey
            } else {
                spec.auth != AuthMethod::OAuth
            }
    })
}

/// Configured routes are shared by all chat models. Prefer declared image support over unknown
/// ChatGPT hosted-tool support, then use stable profile-name order. Never retry after generation.
fn configured_target(config: &Config) -> Option<Target> {
    use crate::llm::{chatgpt, profiles::ImageGenerationApi};
    for declared in [true, false] {
        for (name, profile) in &config.model_profiles {
            let api = profile.capabilities.image_generation;
            let eligible = if declared {
                api != Some(ImageGenerationApi::Disabled)
                    && (api.is_some() || profile.provider != chatgpt::PROVIDER)
            } else {
                api.is_none() && profile.provider == chatgpt::PROVIDER
            };
            if !eligible || !provider_enabled(config, &profile.provider) {
                continue;
            }
            let Ok(mut candidate) = config.with_named_model_profile(name) else {
                continue;
            };
            // A model fallback must not silently switch a session's pinned subscription account.
            if profile.provider == chatgpt::PROVIDER
                && config.provider.as_deref() == Some(chatgpt::PROVIDER)
                && let Some(endpoint) = &mut candidate.endpoint
            {
                endpoint.account_id = config.endpoint.as_ref().and_then(|e| e.account_id.clone());
            }
            if let Ok(Some(target)) = selected::resolve_current(&candidate) {
                return Some(target);
            }
        }
    }
    None
}

fn credential(config: &Config, base_url: &str, variable: Option<&str>) -> Option<String> {
    crate::llm::catalog::resolve_credential(&config.user_dir, base_url, variable)
        .ok()
        .flatten()
}

struct ImageGen;

impl Tool for ImageGen {
    fn name(&self) -> &str {
        TOOL
    }

    fn schema(&self) -> Value {
        tool_schema(
            TOOL,
            DESCRIPTION,
            json!({
                "prompt": {
                    "type": "string",
                    "description": "Image description: subject, style, composition, lighting, and any text to render."
                },
                "size": {
                    "type": "string",
                    "enum": SIZES,
                    "description": "Output size (sent as the matching aspect ratio to providers that take one). Omit it to use the image model's configured size, else 1024x1024."
                },
                "output": {
                    "type": "string",
                    "description": "Workspace-relative file to create: .png, .jpg/.jpeg, or .webp. Valid provider images are converted locally to this format when needed; JPEG is lossy and drops transparency."
                },
                "image": {
                    "type": "string",
                    "description": "Optional workspace image (PNG, JPEG or WebP) to edit instead of generating from scratch. It is only read; the result goes to output."
                }
            }),
            &["prompt", "output"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.execute(
            ctx,
            args,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutput {
        native::with_cancel(cancel, |cancel| self.execute(ctx, args, cancel))
    }
}

impl ImageGen {
    fn execute(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> ToolOutput {
        match generate(ctx, args, cancel) {
            Ok(written) => {
                let similarity = crate::tools::vision::search::prompt_similarity_line(
                    ctx.config,
                    args["prompt"].as_str().unwrap_or_default(),
                    &written.path,
                );
                ToolOutput::ok(format!(
                    "Wrote {} image ({} bytes) from {} to {}{}.{}",
                    written.format.name(),
                    written.bytes,
                    written.model,
                    written.relative,
                    written
                        .source
                        .map(|source| format!(", edited from {source}"))
                        .unwrap_or_default(),
                    similarity
                        .map(|line| format!("\n{line}"))
                        .unwrap_or_default()
                ))
                .with_changes([written.path])
            }
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }
}

#[derive(Debug)]
struct Request<'a> {
    prompt: &'a str,
    size: &'static str,
    output: &'a str,
    /// The image to edit; `None` generates from the prompt alone.
    source: Option<Source>,
}

/// A workspace image an edit starts from.
struct Source {
    relative: String,
    bytes: Vec<u8>,
    format: Format,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("relative", &self.relative)
            .field("format", &self.format)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Source {
    fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }

    fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.format.mime(), self.base64())
    }
}

/// The `image` argument: the workspace path of the image to edit, if any.
fn source_arg(args: &Value) -> Result<Option<&str>> {
    match args.get("image").filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(Some)
            .ok_or_else(|| anyhow!("image_gen image must be a workspace image path")),
    }
}

/// Reads and checks the source image without following a final symlink.
fn read_source(ctx: &ToolContext, relative: &str) -> Result<Source> {
    let path = ctx
        .workspace
        .resolve(relative)
        .map_err(|error| anyhow!(error))?;
    let file = crate::tools::edit::edit_transaction::open_read(ctx.workspace, &path)
        .map_err(|error| anyhow!("reading source image {relative}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading source image {relative}"))?;
    ensure!(
        bytes.len() as u64 <= MAX_SOURCE_BYTES,
        "source image {relative} is larger than {} MiB",
        MAX_SOURCE_BYTES / (1024 * 1024)
    );
    let format = Format::detect(&bytes)
        .ok_or_else(|| anyhow!("source image {relative} is not a PNG, JPEG, or WebP file"))?;
    decode_bounded(&bytes).with_context(|| format!("decoding source image {relative}"))?;
    Ok(Source {
        relative: relative.to_owned(),
        bytes,
        format,
    })
}

/// Decodes an image fully under dimension and allocation limits.
fn decode_bounded(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .context("detecting image format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().context("decoding image")
}

fn parse_request(args: &Value) -> Result<Request<'_>> {
    let prompt = args["prompt"].as_str().map_or("", str::trim);
    ensure!(!prompt.is_empty(), "image_gen requires a non-empty prompt");
    ensure!(
        prompt.len() <= MAX_PROMPT_BYTES,
        "image_gen prompt exceeds {MAX_PROMPT_BYTES} bytes"
    );
    let size = match args.get("size").filter(|value| !value.is_null()) {
        None => DEFAULT_SIZE,
        Some(value) => {
            let requested = value
                .as_str()
                .ok_or_else(|| anyhow!("image_gen size must be a string"))?;
            SIZES
                .iter()
                .copied()
                .find(|size| *size == requested)
                .ok_or_else(|| anyhow!("image_gen size must be one of {}", SIZES.join(", ")))?
        }
    };
    let output = args["output"].as_str().map_or("", str::trim);
    ensure!(!output.is_empty(), "image_gen requires an output path");
    Ok(Request {
        prompt,
        size,
        output,
        source: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Png,
    Jpeg,
    Webp,
}

impl Format {
    fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(PNG_SIGNATURE) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Webp => "WebP",
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Png => &["png"],
            Self::Jpeg => &["jpg", "jpeg"],
            Self::Webp => &["webp"],
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

impl From<Format> for image::ImageFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Png => Self::Png,
            Format::Jpeg => Self::Jpeg,
            Format::Webp => Self::WebP,
        }
    }
}

struct Written {
    relative: String,
    path: PathBuf,
    format: Format,
    bytes: u64,
    model: String,
    source: Option<String>,
}

fn generate(
    ctx: &ToolContext,
    args: &Value,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<Written> {
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::SeqCst),
        "image generation interrupted"
    );
    let mut request = parse_request(args)?;
    let path = ctx
        .workspace
        .resolve(request.output)
        .map_err(|error| anyhow!(error))?;
    ensure!(
        !path.exists(),
        "{} already exists; image_gen never overwrites files",
        request.output
    );
    ctx.authorize_mutation(TOOL, std::slice::from_ref(&path), false)
        .map_err(|error| anyhow!(error))?;

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let format = [Format::Png, Format::Jpeg, Format::Webp]
        .into_iter()
        .find(|format| format.extensions().contains(&extension.as_str()))
        .context("image_gen output extension must be .png, .jpg/.jpeg, or .webp")?;
    let target = resolve(ctx.config)?;
    if let Some(source) = source_arg(args)? {
        if let Some(reason) = edit_unsupported(&target) {
            bail!("{reason}");
        }
        request.source = Some(read_source(ctx, source)?);
    }
    // An explicit size wins; otherwise the model's configured default replaces the built-in one,
    // and edits keep the source's shape.
    if args.get("size").is_none_or(Value::is_null) {
        if let Some(size) = configured_size(&target) {
            request.size = size;
        } else if request.source.is_some()
            || !supported(target.wire, &target.model).contains(&"size")
        {
            request.size = "auto";
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("output has no parent directory"))?;
    // Stage in the destination directory before spending quota; RAII removes it on every error.
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("preparing image destination {}", request.output))?;
    let client = native::Client::new(&target, ctx.config, cancel);
    client.check()?;
    let bytes = request_image(&target, ctx.config, &request, &client)
        .map_err(|error| client.redact(error))?;
    let native_format = Format::detect(&bytes)
        .ok_or_else(|| anyhow!("image API returned data that is not PNG, JPEG, or WebP"))?;
    let decoded = decode_bounded(&bytes).context("decoding generated image")?;
    if native_format == format {
        file.write_all(&bytes)
            .with_context(|| format!("writing {}", request.output))?;
    } else {
        // JPEG cannot encode alpha; the caller chose that loss of transparency via the extension.
        let decoded = if format == Format::Jpeg {
            image::DynamicImage::ImageRgb8(decoded.into_rgb8())
        } else {
            decoded
        };
        decoded
            .write_to(file.as_file_mut(), format.into())
            .with_context(|| format!("converting generated image to {}", format.name()))?;
    }
    let written_bytes = file.as_file().metadata()?.len();
    ensure!(
        written_bytes <= MAX_RESPONSE_BYTES,
        "generated image output exceeds {} MiB",
        MAX_RESPONSE_BYTES / (1024 * 1024)
    );
    client.check()?;
    crate::tools::exec::execution::before_commit().map_err(|error| anyhow!(error))?;
    file.persist_noclobber(&path)
        .with_context(|| format!("publishing {} without overwriting", request.output))?;
    Ok(Written {
        relative: request.output.to_owned(),
        path,
        format,
        bytes: written_bytes,
        model: target.model,
        source: request.source.map(|source| source.relative),
    })
}

fn request_body(target: &Target, request: &Request) -> Value {
    let mut body = json!({
        "model": target.model,
        "prompt": request.prompt,
        "n": 1,
    });
    match target.wire {
        Wire::OpenAi | Wire::MlxServe => {
            // MLX-Serve edits size the result from the source unless a size was chosen.
            if !(target.wire == Wire::MlxServe
                && request.source.is_some()
                && request.size == "auto")
            {
                body["size"] = json!(request.size);
            }
            // DALL·E models default to hosted URLs; gpt-image models always return base64 and
            // reject this field.
            if target.model.starts_with("dall-e") {
                body["response_format"] = json!("b64_json");
            }
            if target.wire == Wire::MlxServe
                && let Some(source) = &request.source
            {
                body["image"] = json!(source.base64());
            }
            apply_settings(target, &mut body, request.source.is_some());
        }
        Wire::Xai => {
            body["aspect_ratio"] = json!(match request.size {
                "1536x1024" => "3:2",
                "1024x1536" => "2:3",
                "auto" => "auto",
                _ => "1:1",
            });
            body["response_format"] = json!("b64_json");
        }
        Wire::Responses | Wire::ChatCompletions => {
            return selected::body(target, request);
        }
        Wire::Gemini
        | Wire::Imagen
        | Wire::BlackForestLabs
        | Wire::ComfyUi
        | Wire::Fal
        | Wire::Replicate => unreachable!("native image services own their request bodies"),
    }
    body
}

fn request_image(
    target: &Target,
    config: &Config,
    request: &Request,
    client: &native::Client<'_>,
) -> Result<Vec<u8>> {
    match target.wire {
        Wire::Gemini | Wire::Imagen => return google::request(client, target, request),
        Wire::BlackForestLabs | Wire::Fal | Wire::Replicate => {
            return queued::request(client, target, request);
        }
        Wire::ComfyUi => return comfyui::request(client, target, request),
        _ => {}
    }
    if target.wire == Wire::Responses {
        return selected::responses(target, config, request);
    }
    let base = target.base_url.trim_end_matches('/');
    let edit = match (target.wire, &request.source) {
        (Wire::OpenAi, Some(source)) => Some(source),
        _ => None,
    };
    let path = match (target.wire, edit) {
        (Wire::ChatCompletions, _) => "chat/completions",
        (_, Some(_)) => "images/edits",
        _ => "images/generations",
    };
    let headers = crate::llm::model_config::resolve_headers(&target.headers)?;
    let agent = client.agent()?;
    let mut builder = agent.post(&format!("{base}/{path}"));
    if !target.key.is_empty() {
        builder = builder.header("authorization", format!("Bearer {}", target.key));
    }
    for (name, value) in headers.pairs() {
        builder = builder.header(name, value);
    }
    let body = request_body(target, request);
    let sent = match edit {
        Some(source) => {
            let boundary = format!("dial-image-{:016x}", rand::random::<u64>());
            builder
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .send(&multipart(&boundary, &body, source)[..])
        }
        None => builder.send_json(body),
    };
    let mut response = sent
        .map_err(|error| headers.redact(anyhow!(error).context("requesting image generation")))?;
    let status = response.status().as_u16();
    let raw = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .context("reading image generation response")?;
    if target.wire == Wire::ChatCompletions && (200..300).contains(&status) {
        return selected::chat_image(
            &serde_json::from_slice(&raw).context("invalid image chat response")?,
        );
    }
    parse_response(status, &raw).map_err(|error| headers.redact(error))
}

/// The `images/edits` form: every body field as text, then the source image as the `image` part.
fn multipart(boundary: &str, body: &Value, source: &Source) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.bytes.len() + 1024);
    for (name, value) in body.as_object().into_iter().flatten() {
        let text = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{text}\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"source.{}\"\r\nContent-Type: {}\r\n\r\n",
            source.format.extensions()[0],
            source.format.mime()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&source.bytes);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

fn parse_response(status: u16, raw: &[u8]) -> Result<Vec<u8>> {
    let value: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let detail = value["error"]["message"]
            .as_str()
            .map(|message| {
                let clipped: String = message.chars().take(MAX_ERROR_CHARS).collect();
                format!(": {}", redact_text(&clipped))
            })
            .unwrap_or_default();
        bail!("image API failed (HTTP {status}){detail}");
    }
    let encoded = value["data"][0]["b64_json"]
        .as_str()
        .ok_or_else(|| anyhow!("image API response contained no b64_json image data"))?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("image API returned invalid base64 image data")
}

#[cfg(test)]
mod tests;
