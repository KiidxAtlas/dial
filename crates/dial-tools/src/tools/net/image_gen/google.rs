//! Google image transports.
//!
//! * Gemini native image generation: `POST {base}/models/{model}:generateContent` with the prompt,
//!   an optional source image as `inlineData`, and `responseModalities` `TEXT` and `IMAGE`.
//! * Imagen `:predict` on an explicitly configured Vertex AI (or compatible) base. Imagen is shut
//!   down on the Gemini Developer API (`generativelanguage.googleapis.com`), so that host fails
//!   before any request is sent. Imagen editing is not supported.
//!
//! Authentication headers are applied by the native client from the wire; this module never
//! builds credentials and never puts them in an error.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use serde_json::{Value, json};

use super::native::Client;
use super::{MAX_RESPONSE_BYTES, Request, Target, Wire, host};

/// The Gemini Developer API host, where Imagen no longer exists.
const GEMINI_API_HOST: &str = "generativelanguage.googleapis.com";
/// Upper bound for a model ID after the optional `models/` prefix is removed.
const MAX_MODEL_CHARS: usize = 128;

/// Runs one Google image request for `target.wire` and returns the single image's bytes.
pub(super) fn request(
    client: &Client<'_>,
    target: &Target,
    request: &Request<'_>,
) -> Result<Vec<u8>> {
    match target.wire {
        Wire::Gemini => gemini(client, target, request),
        Wire::Imagen => imagen(client, target, request),
        _ => bail!("image_gen Google transport received a non-Google route"),
    }
}

fn gemini(client: &Client<'_>, target: &Target, request: &Request<'_>) -> Result<Vec<u8>> {
    let model = model_id(&target.model)?;
    let url = client.endpoint(&format!("models/{model}:generateContent"));
    let response = client.post(&url, &gemini_body(request))?;
    gemini_image(&response)
        .map_err(|error| client.withhold(error, "Gemini returned no usable image"))
}

fn imagen(client: &Client<'_>, target: &Target, request: &Request<'_>) -> Result<Vec<u8>> {
    ensure_no_edit(request)?;
    check_imagen_base(&target.base_url)?;
    let model = model_id(&target.model)?;
    let url = client.endpoint(&format!("models/{model}:predict"));
    let response = client.post(&url, &imagen_body(request)?)?;
    imagen_image(&response)
        .map_err(|error| client.withhold(error, "Imagen returned no usable image"))
}

/// Validates a model ID, removing a single leading `models/` prefix first.
fn model_id(raw: &str) -> Result<&str> {
    let id = raw.strip_prefix("models/").unwrap_or(raw);
    ensure!(
        !id.is_empty()
            && id.len() <= MAX_MODEL_CHARS
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
        "image model id {raw:?} is not a valid Google model id"
    );
    Ok(id)
}

/// Gemini aspect ratio for a generation size; `auto` and unknown sizes omit the field.
fn gemini_aspect_ratio(size: &str) -> Option<&'static str> {
    match size {
        "1024x1024" => Some("1:1"),
        "1536x1024" => Some("3:2"),
        "1024x1536" => Some("2:3"),
        _ => None,
    }
}

/// Imagen aspect ratio for a generation size. Imagen has no 3:2 or 2:3, so the nearest
/// supported ratios are used. `auto` omits the field.
fn imagen_aspect_ratio(size: &str) -> Option<&'static str> {
    match size {
        "1024x1024" => Some("1:1"),
        "1536x1024" => Some("4:3"),
        "1024x1536" => Some("3:4"),
        _ => None,
    }
}

fn gemini_body(request: &Request<'_>) -> Value {
    let mut parts = vec![json!({"text": request.prompt})];
    if let Some(source) = &request.source {
        parts.push(json!({
            "inlineData": {"mimeType": source.format.mime(), "data": source.base64()}
        }));
    }
    let mut config = json!({"responseModalities": ["TEXT", "IMAGE"]});
    if let Some(ratio) = gemini_aspect_ratio(request.size) {
        config["imageConfig"] = json!({"aspectRatio": ratio});
    }
    json!({
        "contents": [{"role": "user", "parts": parts}],
        "generationConfig": config,
    })
}

/// Extracts exactly one non-thought image from a `generateContent` response.
fn gemini_image(response: &Value) -> Result<Vec<u8>> {
    if let Some(reason) = response["promptFeedback"]["blockReason"].as_str() {
        bail!("Gemini blocked the image prompt (blockReason {reason})");
    }
    let candidate = response["candidates"]
        .get(0)
        .ok_or_else(|| anyhow!("Gemini returned no image candidate"))?;
    let finish = candidate["finishReason"].as_str().unwrap_or("STOP");
    match finish {
        "STOP" => {}
        "NO_IMAGE" => bail!("Gemini returned no image (finishReason NO_IMAGE)"),
        "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "IMAGE_SAFETY" => {
            bail!("Gemini refused to produce the image (finishReason {finish})")
        }
        other => bail!("Gemini stopped without an image (finishReason {other})"),
    }
    let images: Vec<&Value> = candidate["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| !part["thought"].as_bool().unwrap_or(false))
        .filter(|part| part.get("inlineData").is_some())
        .collect();
    match images.as_slice() {
        [] => bail!("Gemini returned no image data"),
        [image] => {
            let data = image["inlineData"]["data"]
                .as_str()
                .ok_or_else(|| anyhow!("Gemini image part has no data"))?;
            decode_bounded(data)
        }
        many => bail!(
            "Gemini returned {} images; image_gen requires exactly one",
            many.len()
        ),
    }
}

/// Imagen editing is not implemented; refuses a source image before any request is built.
fn ensure_no_edit(request: &Request<'_>) -> Result<()> {
    ensure!(
        request.source.is_none(),
        "Imagen image editing is not supported; use a Gemini image model to edit"
    );
    Ok(())
}

fn check_imagen_base(base_url: &str) -> Result<()> {
    let host_name = host(base_url).ok_or_else(|| {
        anyhow!("Imagen requires an explicit Vertex AI base URL (publishers/google)")
    })?;
    ensure!(
        host_name != GEMINI_API_HOST,
        "Imagen is shut down on the Gemini API ({GEMINI_API_HOST}); migrate to a Gemini image model, or configure a Vertex AI publishers/google base URL"
    );
    Ok(())
}

/// The output MIME type Imagen should encode, taken from the requested output extension.
fn output_mime(output: &str) -> Result<&'static str> {
    let extension = Path::new(output)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        _ => bail!("Imagen output must be .png or .jpg/.jpeg; WebP is not supported"),
    }
}

fn imagen_body(request: &Request<'_>) -> Result<Value> {
    let mut parameters = json!({
        "sampleCount": 1,
        "outputOptions": {"mimeType": output_mime(request.output)?},
    });
    if let Some(ratio) = imagen_aspect_ratio(request.size) {
        parameters["aspectRatio"] = json!(ratio);
    }
    Ok(json!({
        "instances": [{"prompt": request.prompt}],
        "parameters": parameters,
    }))
}

/// Extracts exactly one image from a `:predict` response.
fn imagen_image(response: &Value) -> Result<Vec<u8>> {
    let predictions = response["predictions"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    match predictions {
        [] => bail!("Imagen returned no image predictions"),
        [prediction] => match prediction["bytesBase64Encoded"].as_str() {
            Some(data) => decode_bounded(data),
            None => match prediction["raiFilteredReason"].as_str() {
                Some(reason) => bail!("Imagen filtered the image (raiFilteredReason {reason})"),
                None => bail!("Imagen prediction contained no image bytes"),
            },
        },
        many => bail!(
            "Imagen returned {} images; image_gen requires exactly one",
            many.len()
        ),
    }
}

/// Decodes base64 image data, refusing anything that would exceed the response budget.
fn decode_bounded(encoded: &str) -> Result<Vec<u8>> {
    let max = MAX_RESPONSE_BYTES;
    ensure!(
        (encoded.len() as u64).div_ceil(4) * 3 <= max,
        "Google image data exceeds {} MiB",
        max / (1024 * 1024)
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Google returned invalid base64 image data")?;
    ensure!(
        bytes.len() as u64 <= max,
        "Google image data exceeds {} MiB",
        max / (1024 * 1024)
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests;
