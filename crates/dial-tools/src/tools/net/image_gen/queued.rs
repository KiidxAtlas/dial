//! Queued image transports: Black Forest Labs, fal and Replicate.
//!
//! Each provider accepts one submission and then reports progress on a status URL. Submission is
//! never repeated and no other billing route is tried. Exactly one image is downloaded, through
//! [`Client::download`], which enforces the delivery-host rules. Polling errors and deadlines name
//! the remote task, which may still be running: this module does not cancel it.
//!
//! Requests carry only what each route documents. Dimensions are sent for BFL's FLUX.2 family
//! only. For other models an explicit size fails instead of being silently ignored. Configured
//! `input` on fal and Replicate is preserved, except the keys this adapter owns.

use super::native::Client;
use super::{Request, Target, Wire};
use crate::storage::redact::redact_text;
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};

/// Polls before giving up: about ten minutes at the native client's half-second pause.
const POLL_LIMIT: usize = 1_200;
const MAX_MODEL_CHARS: usize = 256;
const MAX_ERROR_CHARS: usize = 300;
/// Input keys this adapter owns; configured values for them are never sent.
const ADAPTER_OWNED_INPUT: &[&str] = &["prompt", "n", "size", "output_format"];

/// Runs one queued image request for `target.wire` and returns the single image's bytes.
pub(super) fn request(
    client: &Client<'_>,
    target: &Target,
    request: &Request<'_>,
) -> Result<Vec<u8>> {
    ensure!(
        request.source.is_none(),
        "image edits are not supported on {}'s queued image route; use an OpenAI Images, MLX-Serve or Responses image model",
        target.model
    );
    match target.wire {
        Wire::BlackForestLabs => black_forest_labs(client, target, request),
        Wire::Fal => fal(client, target, request),
        Wire::Replicate => replicate(client, target, request),
        _ => bail!("image_gen queued transport received a non-queued route"),
    }
}

/// `POST {base}/{model}` with `polling_url`; poll that URL until `Ready`, then download `result.sample`.
fn black_forest_labs(
    client: &Client<'_>,
    target: &Target,
    request: &Request<'_>,
) -> Result<Vec<u8>> {
    let model = bfl_model(&target.model)?;
    let body = bfl_body(target, request)?;
    let submitted = client.post(&client.endpoint(model), &body)?;
    let task = required(&submitted, "id", "BFL")?;
    let outcome = (|| {
        let polling = required(&submitted, "polling_url", "BFL")?;
        for _ in 0..POLL_LIMIT {
            client.pause()?;
            let status = client.get(polling)?;
            match status["status"].as_str().unwrap_or_default() {
                "Ready" => {
                    let sample = field(&status["result"], "sample").ok_or_else(|| {
                        anyhow!("BFL task {task} is Ready but returned no result.sample")
                    })?;
                    return client.download(sample);
                }
                state @ ("Error" | "Failed" | "Task not found" | "Request Moderated"
                | "Content Moderated" | "No Card Generated") => {
                    bail!(
                        "BFL image task {task} ended as {state}: {}",
                        client.provider_text(remote_detail(&status))
                    )
                }
                "Pending" | "Reasoning" | "Generating" => {}
                other => bail!(
                    "BFL image task {task} reported unexpected status {}",
                    client.provider_text(clip(other))
                ),
            }
        }
        bail!(
            "BFL image task {task} did not finish within {POLL_LIMIT} polls; it may still be running remotely and was not cancelled"
        )
    })();
    outcome.map_err(|error| interrupted(task, error))
}

/// `POST {base}/{model}`, then poll `status_url` until `COMPLETED`, then fetch `response_url`.
fn fal(client: &Client<'_>, target: &Target, request: &Request<'_>) -> Result<Vec<u8>> {
    let model = fal_model(&target.model)?;
    let body = fal_body(target, request)?;
    let submitted = client.post(&client.endpoint(model), &body)?;
    let task = required(&submitted, "request_id", "fal")?;
    let outcome = (|| {
        let status_url = required(&submitted, "status_url", "fal")?;
        let response_url = required(&submitted, "response_url", "fal")?;
        for _ in 0..POLL_LIMIT {
            client.pause()?;
            let status = client.get(status_url)?;
            match status["status"].as_str().unwrap_or_default() {
                "COMPLETED" => {
                    let failed = ["error", "error_type"]
                        .iter()
                        .any(|key| status.get(*key).is_some_and(|value| !value.is_null()));
                    ensure!(
                        !failed,
                        "fal request {task} completed with an error: {}",
                        client.provider_text(remote_detail(&status))
                    );
                    let result = client.get(response_url)?;
                    let url = fal_image_url(&result)?;
                    return client.download(url);
                }
                "IN_QUEUE" | "IN_PROGRESS" => {}
                other => bail!(
                    "fal request {task} reported unexpected status {}",
                    client.provider_text(clip(other))
                ),
            }
        }
        bail!(
            "fal request {task} did not complete within {POLL_LIMIT} polls; it may still be running remotely and was not cancelled"
        )
    })();
    outcome.map_err(|error| interrupted(task, error))
}

/// `POST /models/{owner}/{name}/predictions` or `POST /predictions` (version form), then poll
/// `urls.get` until `succeeded`.
fn replicate(client: &Client<'_>, target: &Target, request: &Request<'_>) -> Result<Vec<u8>> {
    let route = replicate_route(&target.model)?;
    let body = replicate_body(target, request, &route)?;
    let path = match route {
        ReplicateRoute::Model { owner, name } => format!("models/{owner}/{name}/predictions"),
        ReplicateRoute::Version(_) => "predictions".to_owned(),
    };
    let submitted = client.post(&client.endpoint(&path), &body)?;
    let task = required(&submitted, "id", "Replicate")?;
    let outcome = (|| {
        let poll = required(&submitted["urls"], "get", "Replicate")?;
        for _ in 0..POLL_LIMIT {
            client.pause()?;
            let status = client.get(poll)?;
            match status["status"].as_str().unwrap_or_default() {
                "succeeded" => return client.download(replicate_output(&status["output"])?),
                "starting" | "processing" => {}
                state @ ("failed" | "canceled") => bail!(
                    "Replicate prediction {task} ended as {state}: {}",
                    client.provider_text(remote_detail(&status))
                ),
                other => bail!(
                    "Replicate prediction {task} reported unexpected status {}",
                    client.provider_text(clip(other))
                ),
            }
        }
        bail!(
            "Replicate prediction {task} did not finish within {POLL_LIMIT} polls; it may still be running remotely and was not cancelled"
        )
    })();
    outcome.map_err(|error| interrupted(task, error))
}

/// BFL endpoint names: lowercase letters, digits, `.` and `-`, one path segment.
fn bfl_model(model: &str) -> Result<&str> {
    ensure!(
        model.len() <= 64
            && model.starts_with(|c: char| c.is_ascii_alphanumeric())
            && model.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-')),
        "BFL model {model:?} is not a valid endpoint name"
    );
    Ok(model)
}

/// fal endpoint IDs: `owner/app` with an optional further endpoint segment (2 to 5 segments).
fn fal_model(model: &str) -> Result<&str> {
    let segments: Vec<&str> = model.split('/').collect();
    ensure!(
        model.len() <= MAX_MODEL_CHARS
            && (2..=5).contains(&segments.len())
            && segments.iter().all(|segment| segment_ok(segment)),
        "fal model {model:?} must be owner/app or owner/app/endpoint"
    );
    Ok(model)
}

/// A Replicate target: an official `owner/name` model, `owner/name:version`, or a bare version ID.
#[derive(Debug, PartialEq, Eq)]
enum ReplicateRoute<'a> {
    Model { owner: &'a str, name: &'a str },
    Version(&'a str),
}

fn replicate_route(model: &str) -> Result<ReplicateRoute<'_>> {
    if let Some((owner_name, version)) = model.split_once(':') {
        replicate_owner_name(owner_name)?;
        ensure!(
            version_ok(version),
            "Replicate version in {model:?} must be 64 lowercase hex characters"
        );
        return Ok(ReplicateRoute::Version(version));
    }
    if model.contains('/') {
        let (owner, name) = replicate_owner_name(model)?;
        return Ok(ReplicateRoute::Model { owner, name });
    }
    ensure!(
        version_ok(model),
        "Replicate model {model:?} must be owner/name, owner/name:version, or a 64-character version ID"
    );
    Ok(ReplicateRoute::Version(model))
}

fn replicate_owner_name(model: &str) -> Result<(&str, &str)> {
    let (owner, name) = model
        .split_once('/')
        .ok_or_else(|| anyhow!("Replicate model {model:?} must be owner/name"))?;
    ensure!(
        segment_ok(owner) && segment_ok(name),
        "Replicate model {model:?} must be owner/name"
    );
    Ok((owner, name))
}

/// One path segment: letters, digits, `.`, `-`, `_`, not starting with `.`.
fn segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 100
        && !segment.starts_with('.')
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn version_ok(version: &str) -> bool {
    version.len() == 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The body for BFL: prompt, optional dimensions and seed. Configured `input` is not sent here
/// because BFL's settings support only size and seed.
fn bfl_body(target: &Target, request: &Request<'_>) -> Result<Value> {
    let mut body = Map::new();
    body.insert("prompt".to_owned(), json!(request.prompt));
    body.insert(
        "output_format".to_owned(),
        json!(output_format(target.wire, request.output)?),
    );
    if let Some((width, height)) = bfl_dimensions(target, request)? {
        body.insert("width".to_owned(), json!(width));
        body.insert("height".to_owned(), json!(height));
    }
    if let Some(seed) = target.settings.seed {
        body.insert("seed".to_owned(), json!(seed));
    }
    Ok(Value::Object(body))
}

/// FLUX.2 endpoints take `width` and `height`. Other BFL models use different fields
/// (for example Kontext's `aspect_ratio`), so an explicit size on them fails instead.
fn bfl_dimensions(target: &Target, request: &Request<'_>) -> Result<Option<(u32, u32)>> {
    if !model_supports_size(target) {
        ensure_size_unspecified(target, request)?;
        return Ok(None);
    }
    let dimensions = match request.size {
        "auto" => return Ok(None),
        "1024x1024" => (1024, 1024),
        "1536x1024" => (1536, 1024),
        "1024x1536" => (1024, 1536),
        other => bail!("BFL FLUX.2 does not accept size {other}"),
    };
    Ok(Some(dimensions))
}

/// fal takes the model input object as the request body.
fn fal_body(target: &Target, request: &Request<'_>) -> Result<Value> {
    ensure_size_unspecified(target, request)?;
    let mut input = configured_input(target, request);
    input.insert("num_images".to_owned(), json!(1));
    input.insert(
        "output_format".to_owned(),
        json!(output_format(target.wire, request.output)?),
    );
    Ok(Value::Object(input))
}

/// Replicate takes `input` plus `version` for the version form.
fn replicate_body(
    target: &Target,
    request: &Request<'_>,
    route: &ReplicateRoute<'_>,
) -> Result<Value> {
    ensure_size_unspecified(target, request)?;
    let mut input = configured_input(target, request);
    input.insert("num_outputs".to_owned(), json!(1));
    input.insert(
        "output_format".to_owned(),
        json!(output_format(target.wire, request.output)?),
    );
    let mut body = Map::new();
    if let ReplicateRoute::Version(version) = route {
        body.insert("version".to_owned(), json!(version));
    }
    body.insert("input".to_owned(), Value::Object(input));
    Ok(Value::Object(body))
}

/// Configured model input with the prompt and seed applied. Adapter-owned keys are dropped.
fn configured_input(target: &Target, request: &Request<'_>) -> Map<String, Value> {
    let mut input: Map<String, Value> = target
        .settings
        .input
        .iter()
        .filter(|(key, _)| !ADAPTER_OWNED_INPUT.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    input.insert("prompt".to_owned(), json!(request.prompt));
    if let Some(seed) = target.settings.seed {
        input.insert("seed".to_owned(), json!(seed));
    }
    input
}

/// Generic queue models have no shared size field. The parent defaults omitted sizes to `auto`.
fn ensure_size_unspecified(target: &Target, request: &Request<'_>) -> Result<()> {
    ensure!(
        request.size == "auto",
        "{} has no size field that image_gen can set; remove the size or set the model's own size under input",
        target.model
    );
    Ok(())
}

/// Whether image_gen can set an explicit size on this target's queued route. Only BFL's FLUX.2
/// endpoints take dimensions here. Other queued models default to `auto`, and their own size
/// fields can be set through configured `input`.
pub(super) fn model_supports_size(target: &Target) -> bool {
    target.wire == Wire::BlackForestLabs && target.model.starts_with("flux-2-")
}

/// Queue output format follows the destination extension; fal's documented FLUX routes do not encode WebP.
fn output_format(wire: Wire, output: &str) -> Result<&'static str> {
    let extension = std::path::Path::new(output)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => Ok("png"),
        Some("jpg" | "jpeg") => Ok(if wire == Wire::Replicate {
            "jpg"
        } else {
            "jpeg"
        }),
        Some("webp") if wire != Wire::Fal => Ok("webp"),
        _ => bail!("this queued image route does not support the requested output extension"),
    }
}

/// The single image URL in a fal result, which must contain exactly one image.
fn fal_image_url(result: &Value) -> Result<&str> {
    let images = result["images"]
        .as_array()
        .ok_or_else(|| anyhow!("fal result contained no images array"))?;
    ensure!(
        images.len() == 1,
        "fal result contained {} images; image_gen requires exactly one",
        images.len()
    );
    field(&images[0], "url").ok_or_else(|| anyhow!("fal image entry contained no url"))
}

/// The single image URL in a Replicate output (a string, or an array of exactly one string).
fn replicate_output(output: &Value) -> Result<&str> {
    match output {
        Value::String(url) if !url.is_empty() => Ok(url.as_str()),
        Value::Array(urls) => {
            ensure!(
                urls.len() == 1,
                "Replicate prediction returned {} outputs; image_gen requires exactly one",
                urls.len()
            );
            urls[0]
                .as_str()
                .filter(|url| !url.is_empty())
                .ok_or_else(|| anyhow!("Replicate prediction returned an empty output"))
        }
        _ => bail!("Replicate prediction returned no output image"),
    }
}

/// A non-empty string field.
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().filter(|text| !text.is_empty())
}

/// A required field from a submission response. A missing field can mean a job was accepted
/// without a tracking handle, so the message says so.
fn required<'a>(value: &'a Value, key: &str, provider: &str) -> Result<&'a str> {
    field(value, key).ok_or_else(|| {
        anyhow!("{provider} submission returned no {key}; a remote job may have been accepted")
    })
}

/// Names the remote task for a failure that happened after submission.
fn interrupted(task: &str, error: anyhow::Error) -> anyhow::Error {
    anyhow!("{error:#}; remote task {task} may still be running and was not cancelled")
}

/// The provider's error text, clipped and redacted. Never the full response body.
fn remote_detail(value: &Value) -> String {
    ["error", "error_type", "message", "details"]
        .iter()
        .find_map(|key| value[*key].as_str().filter(|text| !text.is_empty()))
        .map(clip)
        .unwrap_or_else(|| "no detail was returned".to_owned())
}

fn clip(text: &str) -> String {
    let clipped: String = text.chars().take(MAX_ERROR_CHARS).collect();
    redact_text(&clipped).into_owned()
}

#[cfg(test)]
mod tests;
