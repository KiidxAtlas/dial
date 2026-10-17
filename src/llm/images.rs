//! Compact image references. Tool reads stay project-confined; users may explicitly supply external
//! images. Bytes are verified and encoded only for inference.
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
pub(super) struct NoticeTracker(parking_lot::Mutex<HashSet<[u8; 32]>>);

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
        meta.is_file() && meta.len() <= MAX_IMAGE_BYTES,
        "image must be a regular file no larger than 5 MiB"
    );
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_IMAGE_BYTES, "image exceeds 5 MiB");
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

/// An explicit user image reference may name a file outside the tool workspace.
/// Tool reads continue to use `attachment` and remain project-confined.
pub fn user_attachment(root: &Path, path: &Path) -> Result<Value> {
    let expanded = path
        .to_str()
        .and_then(|p| p.strip_prefix("~/"))
        .map(|p| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(p))
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

/// Re-reading exactly an image explicitly attached by the human does not grant arbitrary external
/// file access. The caller still verifies the recorded hash before using the reference.
pub(crate) fn authorized_user_reference(messages: &[Value], path: &Path) -> Option<Value> {
    let expanded = path
        .to_str()
        .and_then(|p| p.strip_prefix("~/"))
        .and_then(|p| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(p)));
    let requested = expanded.as_deref().unwrap_or(path);
    if !requested.is_absolute() || !is_image(requested) {
        return None;
    }
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
pub(crate) fn verified_bytes(reference: &Value) -> Result<(Vec<u8>, &'static str)> {
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
            encoded.len() as u64 <= MAX_IMAGE_BYTES.div_ceil(3) * 4,
            "inline image exceeds 5 MiB"
        );
        let bytes = STANDARD.decode(encoded)?;
        ensure!(
            bytes.len() as u64 <= MAX_IMAGE_BYTES,
            "inline image exceeds 5 MiB"
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
/// Resize an image to the shared remote-input bound, preserving its source format when re-encoding.
/// Small inputs are returned unchanged without decoding or allocating a second image buffer.
pub fn resize_for_model(bytes: &[u8], mime: &str) -> Result<(Vec<u8>, String)> {
    let max_dimension = super::policy::image_edge();
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES,
        "image is empty or exceeds 5 MiB"
    );
    let format = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/webp" => image::ImageFormat::WebP,
        "image/gif" => image::ImageFormat::Gif,
        _ => anyhow::bail!("unsupported image media type"),
    };
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    if width <= max_dimension && height <= max_dimension {
        return Ok((bytes.to_vec(), mime.to_owned()));
    }
    let decoded = image::load_from_memory_with_format(bytes, format)?;
    let resized = decoded.resize(
        max_dimension,
        max_dimension,
        image::imageops::FilterType::Lanczos3,
    );
    let mut output = Vec::new();
    resized.write_to(&mut std::io::Cursor::new(&mut output), format)?;
    ensure!(
        output.len() as u64 <= MAX_IMAGE_BYTES,
        "resized image exceeds 5 MiB"
    );
    Ok((output, mime.to_owned()))
}

fn image_part(reference: &Value) -> Result<Value> {
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

pub(super) fn prepare(
    req: &super::Chat,
    vision: bool,
    notices: &NoticeTracker,
    notice: &mut dyn FnMut(&str),
) -> Option<super::Chat> {
    if !req
        .messages
        .iter()
        .any(|message| message.get("dial_images").is_some())
    {
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
                    "this model cannot view images; {path} skipped. Select a vision-capable model to inspect it"
                );
                failures.push((failure_key(reference, &reason), reason));
                parts.push(json!({"type":"text","text":format!("[Image unavailable: {path}; model lacks vision]")}));
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

fn failure_key(reference: &Value, reason: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(reference.to_string());
    hash.update([0]);
    hash.update(reason);
    hash.finalize().into()
}

pub(super) fn anthropic_content(content: &Value) -> Value {
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
