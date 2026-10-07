//! Compact image references. Tool reads stay project-confined; users may explicitly supply external
//! images. Bytes are verified and encoded only for inference.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path},
};

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
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        anyhow::bail!("file is not a supported PNG, JPEG, GIF or WebP image")
    };
    Ok((bytes, mime))
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
        .filter(|m| m["role"] == "user")
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
    let root = reference["root"]
        .as_str()
        .context("image reference has no project root")?;
    let path = reference["path"]
        .as_str()
        .context("image reference has no path")?;
    let (bytes, mime) = bytes(Path::new(root), Path::new(path))?;
    ensure!(
        reference["sha256"].as_str() == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
        "image changed since attachment; attach it again"
    );
    Ok((bytes, mime))
}

fn image_part(reference: &Value) -> Result<Value> {
    let (bytes, mime) = verified_bytes(reference)?;
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
    for original in &req.messages {
        let mut message = original.clone();
        let is_tool = message["role"] == "tool";
        if !is_tool && !pending.is_empty() {
            prepared
                .messages
                .push(json!({"role":"user","content":content_parts(std::mem::take(&mut pending))}));
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
                notice(&reason);
                parts.push(json!({"type":"text","text":format!("[Image unavailable: {reason}]")}));
                continue;
            }
            match image_part(reference) {
                Ok(part) => parts.push(part),
                Err(error) => {
                    let reason = format!("image {path} skipped: {error}");
                    notice(&reason);
                    parts.push(
                        json!({"type":"text","text":format!("[Image unavailable: {reason}]")}),
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
        prepared
            .messages
            .push(json!({"role":"user","content":content_parts(pending)}));
    }
    Some(prepared)
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
mod tests {
    use super::*;
    #[test]
    fn browser_screenshot_pixels_survive_artifact_storage_and_provider_encoding() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::tools::artifact::ArtifactStore::new(root.path()).unwrap();
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
        let mut output = crate::tools::ToolOutput {
            data: None,
            text: json!({"artifact":{"media_type":"image/png","encoding":"base64","data":encoded}})
                .to_string(),
            is_error: false,
        };
        let image = crate::tools::artifact::capture_image(Some(&store), &mut output)
            .unwrap()
            .unwrap();
        let req = super::super::Chat {
            messages: vec![
                json!({"role":"user","content":"inspect the rendered page"}),
                json!({"role":"assistant","content":null,"tool_calls":[{"id":"capture","type":"function","function":{"name":"browser","arguments":"{\"action\":\"screenshot\"}"}}]}),
                json!({"role":"tool","tool_call_id":"capture","content":output.text,"dial_images":[image]}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        assert!(!req.messages.iter().any(|m| m.to_string().contains(encoded)));
        let prepared = prepare(&req, true, &mut |_| {
            panic!("valid screenshot should attach")
        })
        .unwrap();
        assert_eq!(prepared.messages[2]["role"], "tool");
        assert_eq!(
            prepared.messages[3]["role"], "user",
            "pixels follow the complete tool results"
        );
        let pixels = &prepared.messages[3]["content"][0];
        assert_eq!(
            pixels["image_url"]["url"],
            format!("data:image/png;base64,{encoded}")
        );
        let anthropic = anthropic_content(&prepared.messages[3]["content"]);
        assert_eq!(anthropic[0]["source"]["data"], encoded);
        let responses = super::super::responses::request("vision", &prepared, None).unwrap();
        assert_eq!(responses["input"][3]["content"][0]["type"], "input_image");
        assert_eq!(
            responses["input"][3]["content"][0]["image_url"],
            pixels["image_url"]["url"]
        );
        assert!(!responses.to_string().contains("dial_images"));
    }
    #[test]
    fn unavailable_images_are_explained_to_the_model_as_well_as_the_user() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("tiny.png"), b"\x89PNG\r\n\x1a\noriginal").unwrap();
        let reference = attachment(root.path(), Path::new("tiny.png")).unwrap();
        let req = super::super::Chat {
            messages: vec![json!({"role":"user","content":"inspect it","dial_images":[reference]})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        for vision in [false, true] {
            if vision {
                fs::remove_file(root.path().join("tiny.png")).unwrap();
            }
            let mut notices = Vec::new();
            let prepared =
                prepare(&req, vision, &mut |notice| notices.push(notice.to_string())).unwrap();
            assert_eq!(notices.len(), 1);
            assert!(
                prepared.messages[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("Image unavailable")
            );
            assert!(
                !prepared
                    .messages
                    .iter()
                    .any(|m| m.to_string().contains("image_url"))
            );
        }
    }
    #[test]
    fn compact_references_reject_changed_oversized_and_escaped_images() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("tiny.png"), b"\x89PNG\r\n\x1a\noriginal").unwrap();
        let reference = attachment(root.path(), Path::new("tiny.png")).unwrap();
        assert!(reference.get("data").is_none());
        let part = image_part(&reference).unwrap();
        assert!(
            part["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        fs::write(root.path().join("tiny.png"), b"\x89PNG\r\n\x1a\nchanged").unwrap();
        assert!(image_part(&reference).is_err());
        fs::remove_file(root.path().join("tiny.png")).unwrap();
        assert!(image_part(&reference).is_err());
        assert!(attachment(root.path(), Path::new("../escape.png")).is_err());
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("image.png"),
            b"\x89PNG\r\n\x1a\noutside",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("image.png"),
            root.path().join("link.png"),
        )
        .unwrap();
        assert!(attachment(root.path(), Path::new("link.png")).is_err());
        let large = fs::File::create(root.path().join("large.png")).unwrap();
        large.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        assert!(attachment(root.path(), Path::new("large.png")).is_err());
    }
}
