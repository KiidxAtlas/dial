//! Compact, project-confined image references. Bytes are verified and encoded only for inference.
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

fn image_part(reference: &Value) -> Result<Value> {
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
    Ok(
        json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{}", STANDARD.encode(bytes))}}),
    )
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
                .push(json!({"role":"user","content":std::mem::take(&mut pending)}));
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
                notice(&format!(
                    "no vision capability is confirmed for this model; {path} skipped"
                ));
                continue;
            }
            match image_part(reference) {
                Ok(part) => parts.push(part),
                Err(error) => notice(&format!("image {path} skipped: {error}")),
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
            message["content"] = Value::Array(content);
        }
        prepared.messages.push(message);
    }
    if !pending.is_empty() {
        prepared
            .messages
            .push(json!({"role":"user","content":pending}));
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
