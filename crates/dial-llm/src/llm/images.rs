//! Compact image references. Tool reads stay project-confined; users may explicitly supply external
//! images. Bytes are verified and encoded only for inference.
use crate::config::snapcompact::ImageDetail;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Component, Path},
};

/// UI diagnostics belong to a backend conversation, not every inference step. Keep only hashes
/// of failures in the latest image-bearing request; request content never depends on this state.
#[derive(Default)]
pub struct NoticeTracker(parking_lot::Mutex<HashSet<[u8; 32]>>);

impl NoticeTracker {
    fn report(&self, failures: Vec<([u8; 32], String)>, notice: &mut dyn FnMut(&str)) {
        let mut previous = self.0.lock();
        let mut current = HashSet::new();
        let mut fresh = Vec::new();
        for (key, reason) in failures {
            if current.insert(key) && !previous.contains(&key) {
                fresh.push(reason);
            }
        }
        *previous = current;
        drop(previous);
        for reason in fresh {
            notice(&reason);
        }
    }
}

/// Largest source image read from disk or a snapshot. Decoding is guarded separately by
/// [`MAX_IMAGE_PIXELS`]; the model-facing encoding is bounded by [`MAX_IMAGE_BYTES`].
pub const MAX_IMAGE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
pub fn is_image(path: &Path) -> bool {
    path.extension().and_then(|v| v.to_str()).is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp"
        )
    })
}

fn bytes(root: &Path, path: &Path) -> Result<(Vec<u8>, &'static str)> {
    ensure!(
        !path.is_absolute()
            && !path
                .components()
                .any(|p| matches!(p, Component::ParentDir | Component::Prefix(_))),
        "image must stay inside the project"
    );
    let root = root
        .canonicalize()
        .context("image project root is unavailable")?;
    let real = root
        .join(path)
        .canonicalize()
        .context("image file is unavailable")?;
    ensure!(
        real.starts_with(&root),
        "image resolves outside the project"
    );
    let file = fs::File::open(real).context("image cannot be opened")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_IMAGE_SOURCE_BYTES,
        "image must be a regular file no larger than 64 MiB"
    );
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_IMAGE_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_IMAGE_SOURCE_BYTES,
        "image exceeds 64 MiB"
    );
    let mime = mime(&bytes)?;
    Ok((bytes, mime))
}

fn mime(bytes: &[u8]) -> Result<&'static str> {
    Ok(if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        anyhow::bail!("file is not a supported PNG, JPEG, GIF or WebP image")
    })
}

/// Caller must apply ordinary tool permissions before attaching a read result.
pub fn attachment(root: &Path, path: &Path) -> Result<Value> {
    let root = root.canonicalize()?;
    let (bytes, _) = bytes(&root, path)?;
    Ok(json!({"root":root,"path":path,"sha256":format!("{:x}", Sha256::digest(&bytes))}))
}

/// Reference for image bytes supplied directly (stdin `--image -`). The format is detected from
/// the magic bytes, so a non-image payload is refused before any reference exists.
pub fn inline_attachment(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() as u64 <= MAX_IMAGE_SOURCE_BYTES,
        "image exceeds 64 MiB"
    );
    mime(bytes)?;
    Ok(json!({
        "inline": STANDARD.encode(bytes),
        "sha256": format!("{:x}", Sha256::digest(bytes)),
    }))
}

/// An explicit user image reference may name a file outside the tool workspace.
/// Tool reads continue to use `attachment` and remain project-confined.
pub fn user_attachment(root: &Path, path: &Path) -> Result<Value> {
    let expanded = home_relative(path)
        .map(|rest| {
            crate::config::paths::home_dir()
                .map(|home| home.join(rest))
                .context("home directory is unavailable")
        })
        .transpose()?;
    let path = expanded.as_deref().unwrap_or(path);
    if !path.is_absolute() {
        return attachment(root, path);
    }
    let parent = path.parent().context("image has no parent directory")?;
    let name = path.file_name().context("image has no file name")?;
    attachment(parent, Path::new(name))
}

/// The rest of a `~/`-prefixed path (also `~\` on Windows). Native Windows rarely sets `HOME`,
/// so expansion uses [`crate::config::paths::home_dir`], which falls back to `USERPROFILE`.
fn home_relative(path: &Path) -> Option<&str> {
    let text = path.to_str()?;
    text.strip_prefix("~/")
        .or_else(|| cfg!(windows).then(|| text.strip_prefix("~\\")).flatten())
}

/// Re-reading exactly an image explicitly attached by the human does not grant arbitrary external
/// file access. The caller still verifies the recorded hash before using the reference.
/// Whether `path` could name a user-supplied image at all (absolute after `~/` expansion, with an
/// image extension). Callers check this before collecting the history [`authorized_user_reference`]
/// searches, so ordinary reads do not copy the whole conversation.
pub fn may_reference_user_image(path: &Path) -> bool {
    requested_image(path).is_some()
}

fn requested_image(path: &Path) -> Option<std::path::PathBuf> {
    let expanded = home_relative(path)
        .and_then(|rest| crate::config::paths::home_dir().map(|home| home.join(rest)));
    let requested = expanded.unwrap_or_else(|| path.to_path_buf());
    (requested.is_absolute() && is_image(&requested)).then_some(requested)
}

pub fn authorized_user_reference(messages: &[Value], path: &Path) -> Option<Value> {
    let requested = requested_image(path)?;
    let requested = requested.canonicalize().ok()?;
    messages
        .iter()
        .filter(|m| {
            m["role"] == "user"
                || (m["role"] == "tool"
                    && (m.get("dial_question_result").is_some()
                        || m.get("dial_question_results").is_some()))
        })
        .flat_map(|m| m["dial_images"].as_array().into_iter().flatten())
        .find(|reference| {
            let (Some(root), Some(path)) = (reference["root"].as_str(), reference["path"].as_str())
            else {
                return false;
            };
            Path::new(root).join(path) == requested
        })
        .cloned()
}

/// Resolve only the already-authorized attachment, with the same hash and file limits as inference.
pub fn verified_bytes(reference: &Value) -> Result<(Vec<u8>, &'static str)> {
    if let Some(reason) = reference["unavailable"].as_str() {
        anyhow::bail!("{reason}");
    }
    let reference = if let Some(snapshot) = reference.get("snapshot") {
        ensure!(
            snapshot["sha256"] == reference["sha256"],
            "image snapshot identity mismatch"
        );
        snapshot
    } else {
        reference
    };
    let (bytes, mime) = if let Some(encoded) = reference.get("inline").and_then(Value::as_str) {
        ensure!(
            encoded.len() as u64 <= MAX_IMAGE_SOURCE_BYTES.div_ceil(3) * 4,
            "inline image exceeds 64 MiB"
        );
        let bytes = STANDARD.decode(encoded)?;
        ensure!(
            bytes.len() as u64 <= MAX_IMAGE_SOURCE_BYTES,
            "inline image exceeds 64 MiB"
        );
        let mime = mime(&bytes)?;
        (bytes, mime)
    } else {
        let root = reference["root"]
            .as_str()
            .context("image reference has no project root")?;
        let path = reference["path"]
            .as_str()
            .context("image reference has no path")?;
        bytes(Path::new(root), Path::new(path))?
    };
    ensure!(
        reference["sha256"].as_str() == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
        "image changed since attachment; attach it again"
    );
    Ok((bytes, mime))
}
/// Decoded pixels beyond this are refused before any pixel buffer is allocated.
pub const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

/// Dimensions from the container header only; nothing is decoded. Oversized canvases are refused.
pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()?;
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS,
        "image has too many pixels to decode safely"
    );
    Ok((width, height))
}

/// Decode with the dimension and allocation limits that keep a small compressed input bounded.
fn decode(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}

/// Scale-and-encode rounds before giving up; each round keeps 3/4 of the previous edges.
const SHRINK_ROUNDS: usize = 12;

/// Bring an image within the model's edge and encoded-size bounds. An image already within both is
/// returned unchanged. Otherwise it is decoded once (guarded by [`dimensions`]), scaled to the edge
/// bound, and encoded in its own format when that fits; otherwise as JPEG, shrinking until it fits.
pub fn resize_for_model(bytes: &[u8], mime: &str) -> Result<(Vec<u8>, String)> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_SOURCE_BYTES,
        "image is empty or exceeds 64 MiB"
    );
    let max_dimension = super::policy::image_edge();
    let (width, height) = dimensions(bytes)?;
    if width <= max_dimension && height <= max_dimension && bytes.len() as u64 <= MAX_IMAGE_BYTES {
        return Ok((bytes.to_vec(), mime.to_owned()));
    }
    // Every request re-sends the whole history, so the same oversized screenshot would otherwise be
    // decoded, Lanczos-resized and re-encoded again on every tool step.
    let key: ResizeKey = (Sha256::digest(bytes).into(), mime.to_owned(), max_dimension);
    if let Some(hit) = resized_cache_get(&key) {
        return Ok(hit);
    }
    let mut decoded = decode(bytes)?;
    if width > max_dimension || height > max_dimension {
        decoded = decoded.resize(
            max_dimension,
            max_dimension,
            image::imageops::FilterType::Lanczos3,
        );
    }
    let resized = encode_within_limit(decoded, mime)?;
    resized_cache_put(key, &resized);
    Ok(resized)
}

type ResizeKey = ([u8; 32], String, u32);
/// Resized images kept for reuse across requests: few entries, bounded bytes, oldest evicted first.
const RESIZED_CACHE_ENTRIES: usize = 32;
const RESIZED_CACHE_BYTES: usize = 64 * 1024 * 1024;
type Resized = (Vec<u8>, String);
static RESIZED: std::sync::Mutex<std::collections::VecDeque<(ResizeKey, Resized)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

fn resized_cache_get(key: &ResizeKey) -> Option<(Vec<u8>, String)> {
    let mut cache = RESIZED.lock().unwrap_or_else(|e| e.into_inner());
    let position = cache.iter().position(|(stored, _)| stored == key)?;
    let entry = cache.remove(position)?;
    let value = entry.1.clone();
    cache.push_back(entry);
    Some(value)
}

fn resized_cache_put(key: ResizeKey, value: &(Vec<u8>, String)) {
    if value.0.len() > RESIZED_CACHE_BYTES / 4 {
        return;
    }
    let mut cache = RESIZED.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(stored, _)| *stored != key);
    cache.push_back((key, value.clone()));
    while cache.len() > RESIZED_CACHE_ENTRIES
        || cache
            .iter()
            .map(|(_, (bytes, _))| bytes.len())
            .sum::<usize>()
            > RESIZED_CACHE_BYTES
    {
        cache.pop_front();
    }
}

fn encode_within_limit(mut current: image::DynamicImage, mime: &str) -> Result<(Vec<u8>, String)> {
    for _ in 0..SHRINK_ROUNDS {
        let preserved = encode_as(&current, mime)?;
        if preserved.len() as u64 <= MAX_IMAGE_BYTES {
            return Ok((preserved, mime.to_owned()));
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(current.to_rgb8()).write_with_encoder(
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85),
        )?;
        if jpeg.len() as u64 <= MAX_IMAGE_BYTES {
            return Ok((jpeg, "image/jpeg".to_owned()));
        }
        current = shrink(&current);
    }
    anyhow::bail!("image cannot be reduced to 5 MiB")
}

fn shrink(image: &image::DynamicImage) -> image::DynamicImage {
    image.resize_exact(
        (image.width() * 3 / 4).max(1),
        (image.height() * 3 / 4).max(1),
        image::imageops::FilterType::Lanczos3,
    )
}

fn encode_as(image: &image::DynamicImage, mime: &str) -> Result<Vec<u8>> {
    let format = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/webp" => image::ImageFormat::WebP,
        "image/gif" => image::ImageFormat::Gif,
        _ => anyhow::bail!("unsupported image media type"),
    };
    let mut output = Vec::new();
    if format == image::ImageFormat::Jpeg {
        image::DynamicImage::ImageRgb8(image.to_rgb8())
            .write_to(&mut std::io::Cursor::new(&mut output), format)?;
    } else {
        image.write_to(&mut std::io::Cursor::new(&mut output), format)?;
    }
    Ok(output)
}

/// A PNG derived on this machine (rasterized SVG, video frame, PDF page) that fits the model bound.
/// Oversized PNGs are scaled down and re-encoded as PNG so the artifact channel keeps its format.
pub fn fit_png(png: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !png.is_empty() && png.len() as u64 <= MAX_IMAGE_SOURCE_BYTES,
        "derived image is empty or exceeds 64 MiB"
    );
    if png.len() as u64 <= MAX_IMAGE_BYTES {
        return Ok(png.to_vec());
    }
    let mut current = decode(png)?;
    for _ in 0..SHRINK_ROUNDS {
        current = shrink(&current);
        let encoded = encode_as(&current, "image/png")?;
        if encoded.len() as u64 <= MAX_IMAGE_BYTES {
            return Ok(encoded);
        }
    }
    anyhow::bail!("derived image cannot be reduced to 5 MiB")
}

/// Text-only description for a model that cannot view the image: format, dimensions, size and hash prefix.
pub fn describe(bytes: &[u8], mime: &str) -> String {
    let dimensions = dimensions(bytes).map_or_else(
        |_| "unknown dimensions".to_owned(),
        |(width, height)| format!("{width}x{height}"),
    );
    let digest = format!("{:x}", Sha256::digest(bytes));
    format!(
        "{mime}, {dimensions}, {} bytes, sha256 {}",
        bytes.len(),
        &digest[..12]
    )
}

pub fn image_part(reference: &Value) -> Result<Value> {
    let (bytes, mime) = verified_bytes(reference)?;
    let (bytes, mime) = resize_for_model(&bytes, mime)?;
    Ok(
        json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{}", STANDARD.encode(bytes)),"detail":"high"}}),
    )
}

fn content_parts(parts: Vec<Value>) -> Value {
    if parts.iter().all(|part| part["type"] == "text") {
        return json!(
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }
    Value::Array(parts)
}

pub fn prepare(
    req: &super::Chat,
    vision: bool,
    notices: &NoticeTracker,
    notice: &mut dyn FnMut(&str),
) -> Option<super::Chat> {
    prepare_for_target(req, vision, ImageDetail::Original, notices, notice)
}

/// Extend the normal image projection with the current route's supported generated-frame hint.
/// Retained pixels and the canonical journal are never rewritten on a model switch.
pub(crate) fn prepare_for_target(
    req: &super::Chat,
    vision: bool,
    supported_detail: ImageDetail,
    notices: &NoticeTracker,
    notice: &mut dyn FnMut(&str),
) -> Option<super::Chat> {
    if !req.messages.iter().any(|message| {
        message.get("dial_images").is_some()
            || (supported_detail != ImageDetail::Original
                && has_original_snapcompact_image(message))
    }) {
        return None;
    }
    let mut prepared = super::Chat {
        messages: Vec::with_capacity(req.messages.len()),
        tools: req.tools.clone(),
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        seed: req.seed,
    };
    let mut pending = Vec::new();
    let mut failures = Vec::new();
    for original in &req.messages {
        let mut message = original.clone();
        let is_tool = message["role"] == "tool";
        if supported_detail != ImageDetail::Original && has_original_snapcompact_image(&message) {
            for field in ["content", "dial_provider_content"] {
                if let Some(parts) = message[field].as_array_mut() {
                    for part in parts {
                        if part["type"] == "image_url" && part["image_url"]["detail"] == "original"
                        {
                            part["image_url"]["detail"] = json!("high");
                        }
                    }
                }
            }
            let reason = "Snapcompact image detail original is not declared for this route; sending high detail on the request copy. Stored PNGs and sources are unchanged.";
            failures.push((Sha256::digest(reason.as_bytes()).into(), reason.to_owned()));
        }
        if !is_tool && !pending.is_empty() {
            prepared
                .messages
                .push(json!({"role":"user","content":content_parts(std::mem::take(&mut pending)),"dial_generated_user":true}));
        }
        let references = message
            .as_object_mut()
            .and_then(|m| m.remove("dial_images"));
        let mut parts = Vec::new();
        for reference in references
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let path = reference["path"].as_str().unwrap_or("image");
            if !vision {
                let reason = format!(
                    "this model cannot view images; {path} described from metadata only. Select a vision-capable model to inspect it"
                );
                failures.push((failure_key(reference, &reason), reason));
                let text = match verified_bytes(reference) {
                    Ok((bytes, mime)) => format!(
                        "[Image {path} not viewable by this model: {}]",
                        describe(&bytes, mime)
                    ),
                    Err(error) => format!("[Image unavailable: {path}; {error}]"),
                };
                parts.push(json!({"type":"text","text":text}));
                continue;
            }
            match image_part(reference) {
                Ok(part) => parts.push(part),
                Err(error) => {
                    let reason = format!("image {path} skipped: {error}");
                    failures.push((failure_key(reference, &reason), reason));
                    // Preserve a stable, concise explanation in history even after the UI notice
                    // has been shown. Dropping it on later requests would churn the prompt prefix.
                    let detail = error.to_string();
                    let detail = if detail == "image changed since attachment; attach it again" {
                        "changed; reattach"
                    } else {
                        &detail
                    };
                    parts.push(
                        json!({"type":"text","text":format!("[Image unavailable: {path}; {detail}]")}),
                    );
                }
            }
        }
        if is_tool {
            pending.extend(parts);
        } else if !parts.is_empty() {
            let mut content = match message.get("content") {
                Some(Value::Array(parts)) => parts.clone(),
                Some(Value::String(text)) if !text.is_empty() => {
                    vec![json!({"type":"text","text":text})]
                }
                _ => Vec::new(),
            };
            content.extend(parts);
            message["content"] = content_parts(content);
        }
        prepared.messages.push(message);
    }
    if !pending.is_empty() {
        prepared.messages.push(
            json!({"role":"user","content":content_parts(pending),"dial_generated_user":true}),
        );
    }
    notices.report(failures, notice);
    Some(prepared)
}

fn has_original_snapcompact_image(message: &Value) -> bool {
    (message["dial_snapcompact"].is_object() || message["dial_snapcompact_view"].is_object())
        && ["content", "dial_provider_content"].iter().any(|field| {
            message[*field].as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["type"] == "image_url" && part["image_url"]["detail"] == "original"
                })
            })
        })
}

fn failure_key(reference: &Value, reason: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(reference.to_string());
    hash.update([0]);
    hash.update(reason);
    hash.finalize().into()
}

pub fn anthropic_content(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        parts
            .iter()
            .map(|part| {
                if part["type"] != "image_url" {
                    return part.clone();
                }
                let Some((mime, data)) = part
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .and_then(|url| url.strip_prefix("data:"))
                    .and_then(|url| url.split_once(";base64,"))
                else {
                    return part.clone();
                };
                json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}})
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;

/// Splits a `path?q=question` read target. The question is everything after the first `?q=`, and an
/// empty path or question is not a question read.
pub fn split_question(target: &str) -> Option<(&str, &str)> {
    let (path, question) = target.split_once("?q=")?;
    (!path.is_empty() && !question.trim().is_empty()).then_some((path, question.trim()))
}
