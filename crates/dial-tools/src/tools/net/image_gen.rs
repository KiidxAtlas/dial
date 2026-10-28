//! `image_gen`: one generated image from an OpenAI-compatible `images/generations` endpoint,
//! written to a new workspace file.
//!
//! Explicit `[image]` overrides win. Otherwise a declared image-capable selected model is used
//! with its own connection and credentials, before the provider image-model fallback.
//! Image-input support alone never enables generation. Automatic offering reads only local state.
//!
//! Request validation, path resolution, destination preflight and mutation authority run before
//! generation. Provider errors or a later filesystem race can still fail a paid request. Returned
//! images are decoded with resource limits; existing files are never overwritten.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::config::image::ImageConfig;
use crate::config::types::Config;
use crate::storage::redact::redact_text;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, tool_schema};

mod selected;

pub const TOOL: &str = "image_gen";
const SIZES: &[&str] = &["auto", "1024x1024", "1536x1024", "1024x1536"];
const DEFAULT_SIZE: &str = "1024x1024";
const MAX_PROMPT_BYTES: usize = 4_000;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;
const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const DESCRIPTION: &str = "Generate one image using the selected model when it declares image output, \
    otherwise the configured image endpoint or provider image model, and write it to a new workspace \
    file. Output must be PNG, JPEG, or WebP, and the file extension must match the format. \
    Existing files are never overwritten. The written image is attached to the result for \
    vision-capable models: inspect it and generate a revised file when it misses the request. \
    Requires approval.";

/// Hosted session providers whose `images/generations` route returns base64 images:
/// (API host, default image model, request shape).
const PROVIDERS: &[(&str, &str, Wire)] = &[
    ("api.openai.com", "gpt-image-1", Wire::OpenAi),
    ("api.x.ai", "grok-imagine-image-2.0", Wire::Xai),
];

/// Registered through `integrations = ["image_gen"]`.
pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ImageGen)]
}

/// The tool when `image.auto` is on and the resolved image endpoint has an API key. Reads only
/// local credentials (environment, saved keys); never contacts the provider. With default `[image]`
/// settings, a missing key simply means the tool is not offered.
///
/// # Errors
/// Returns credential/configuration failures for customized `[image]` settings, so the registry
/// can explain why the configured endpoint is unavailable.
pub fn auto_tools(config: &Config) -> Result<Vec<Arc<dyn Tool>>> {
    if !config.image.auto {
        return Ok(Vec::new());
    }
    match resolve(config) {
        Ok(_) => Ok(tools(config)),
        Err(error) if customized(&config.image) => Err(error),
        Err(_) => Ok(Vec::new()),
    }
}

/// Whether the user changed the image endpoint, model or key variable from the defaults.
fn customized(image: &ImageConfig) -> bool {
    let defaults = ImageConfig::default();
    image.base_url != defaults.base_url
        || image.model != defaults.model
        || image.api_key_env.is_some()
}

/// Request body shape of an `images/generations` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// OpenAI: `size`; DALL·E models need `response_format`, gpt-image models reject it.
    OpenAi,
    /// xAI: `aspect_ratio` instead of `size`, and `response_format` for base64 data.
    Xai,
    Responses,
    ChatCompletions,
}

impl Wire {
    fn of(base_url: &str) -> Self {
        host(base_url)
            .and_then(|host| PROVIDERS.iter().find(|(known, _, _)| *known == host))
            .map_or(Self::OpenAi, |(_, _, wire)| *wire)
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
}

/// Resolves the image endpoint and its key (see the module docs for the order).
fn resolve(config: &Config) -> Result<Target> {
    let image = &config.image;
    let customized = customized(image);
    if !customized && let Some(target) = selected::resolve(config)? {
        return Ok(target);
    }
    let session = config
        .endpoint
        .as_ref()
        .filter(|_| !customized)
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
            })
        });
    if let Some(target) = session {
        return Ok(target);
    }
    let key = crate::llm::catalog::resolve_credential(
        &config.user_dir,
        image.base_url.trim_end_matches('/'),
        image.api_key_env.as_deref(),
    )?
    .ok_or_else(|| {
        anyhow!(
            "no API key for image generation; save one through /providers or set image.api_key_env"
        )
    })?;
    Ok(Target {
        base_url: image.base_url.clone(),
        model: image.model.clone(),
        wire: Wire::of(&image.base_url),
        key,
        account_id: None,
        headers: Vec::new(),
    })
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
                    "description": "Output size (sent as the matching aspect ratio to providers that take one). Defaults to 1024x1024."
                },
                "output": {
                    "type": "string",
                    "description": "Workspace-relative file to create. Its extension must match the returned format: .png, .jpg/.jpeg, or .webp."
                }
            }),
            &["prompt", "output"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match generate(ctx, args) {
            Ok(written) => ToolOutput::ok(format!(
                "Wrote {} image ({} bytes) from {} to {}.",
                written.format.name(),
                written.bytes,
                written.model,
                written.relative
            ))
            .with_changes([written.path]),
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }
}

#[derive(Debug)]
struct Request<'a> {
    prompt: &'a str,
    size: &'static str,
    output: &'a str,
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
}

struct Written {
    relative: String,
    path: PathBuf,
    format: Format,
    bytes: usize,
    model: String,
}

fn generate(ctx: &ToolContext, args: &Value) -> Result<Written> {
    let request = parse_request(args)?;
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
    ensure!(
        [Format::Png, Format::Jpeg, Format::Webp]
            .iter()
            .any(|format| format.extensions().contains(&extension.as_str())),
        "image_gen output extension must be .png, .jpg/.jpeg, or .webp"
    );
    let target = resolve(ctx.config)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("output has no parent directory"))?;
    // Stage in the destination directory before spending quota; RAII removes it on every error.
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("preparing image destination {}", request.output))?;
    let bytes = request_image(&target, ctx.config, &request)?;
    let format = Format::detect(&bytes)
        .ok_or_else(|| anyhow!("image API returned data that is not PNG, JPEG, or WebP"))?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .context("detecting generated image format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().context("decoding generated image")?;
    ensure!(
        format.extensions().contains(&extension.as_str()),
        "{} is {} data; name the output with {}",
        request.output,
        format.name(),
        format
            .extensions()
            .iter()
            .map(|ext| format!(".{ext}"))
            .collect::<Vec<_>>()
            .join(" or ")
    );

    file.write_all(&bytes)
        .with_context(|| format!("writing {}", request.output))?;
    file.persist_noclobber(&path)
        .with_context(|| format!("publishing {} without overwriting", request.output))?;
    Ok(Written {
        relative: request.output.to_owned(),
        path,
        format,
        bytes: bytes.len(),
        model: target.model,
    })
}

fn request_body(target: &Target, request: &Request) -> Value {
    let mut body = json!({
        "model": target.model,
        "prompt": request.prompt,
        "n": 1,
    });
    match target.wire {
        Wire::OpenAi => {
            body["size"] = json!(request.size);
            // DALL·E models default to hosted URLs; gpt-image models always return base64 and
            // reject this field.
            if target.model.starts_with("dall-e") {
                body["response_format"] = json!("b64_json");
            }
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
    }
    body
}

fn request_image(target: &Target, config: &Config, request: &Request) -> Result<Vec<u8>> {
    if target.wire == Wire::Responses {
        return selected::responses(target, config, request);
    }
    let base = target.base_url.trim_end_matches('/');
    let path = if target.wire == Wire::ChatCompletions { "chat/completions" } else { "images/generations" };
    let headers = crate::llm::model_config::resolve_headers(&target.headers)?;
    let agent = crate::llm::agent_with(config.image.timeout_secs, true, None, None);
    let mut builder = agent.post(&format!("{base}/{path}"))
        .header("authorization", format!("Bearer {}", target.key));
    for (name, value) in headers.pairs() {
        builder = builder.header(name, value);
    }
    let mut response = builder.send_json(request_body(target, request))
        .map_err(|error| headers.redact_error(anyhow!(error).context("requesting image generation")))?;
    let status = response.status().as_u16();
    let raw = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .context("reading image generation response")?;
    if target.wire == Wire::ChatCompletions && (200..300).contains(&status) {
        return selected::chat_image(&serde_json::from_slice(&raw).context("invalid image chat response")?);
    }
    parse_response(status, &raw)
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
