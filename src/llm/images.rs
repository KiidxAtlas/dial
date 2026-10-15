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
mod tests {
    use super::*;
    #[test]
    fn retained_pixels_survive_source_overwrite_deletion_and_storage_reopen() {
        let root = tempfile::tempdir().unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let path = root.path().join("shot.png");
        fs::write(&path, tiny_png()).unwrap();
        let mut message = json!({"role":"user", "content":"inspect", "dial_images":[attachment(root.path(), Path::new("shot.png")).unwrap()]});
        crate::storage::media::retain_message(
            &mut message,
            &crate::tools::artifact::ArtifactStore::new(blobs.path()).unwrap(),
        );
        let durable = serde_json::to_vec(&message).unwrap();
        assert!(
            durable.len() < 1024,
            "only compact references enter the journal"
        );
        fs::write(&path, [tiny_png(), b"new screenshot".to_vec()].concat()).unwrap();
        assert_eq!(
            verified_bytes(&message["dial_images"][0]).unwrap().0,
            tiny_png()
        );
        fs::remove_file(&path).unwrap();
        let reopened: Value = serde_json::from_slice(&durable).unwrap();
        assert_eq!(
            verified_bytes(&reopened["dial_images"][0]).unwrap().0,
            tiny_png()
        );
        let snapshot = &reopened["dial_images"][0]["snapshot"];
        fs::write(
            Path::new(snapshot["root"].as_str().unwrap()).join(snapshot["path"].as_str().unwrap()),
            b"tampered",
        )
        .unwrap();
        assert!(verified_bytes(&reopened["dial_images"][0]).is_err());
    }

    #[test]
    fn capture_failure_never_substitutes_replacement_pixels() {
        let root = tempfile::tempdir().unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let path = root.path().join("shot.png");
        fs::write(&path, tiny_png()).unwrap();
        let mut message =
            json!({"dial_images":[attachment(root.path(), Path::new("shot.png")).unwrap()]});
        fs::remove_file(&path).unwrap();
        crate::storage::media::retain_message(
            &mut message,
            &crate::tools::artifact::ArtifactStore::new(blobs.path()).unwrap(),
        );
        fs::write(&path, tiny_png()).unwrap();
        assert!(
            verified_bytes(&message["dial_images"][0])
                .unwrap_err()
                .to_string()
                .contains("capture failed")
        );
    }
    #[test]
    fn stale_image_notices_do_not_repeat_or_mutate_request_history() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("shot.png");
        fs::write(&path, tiny_png()).unwrap();
        let reference = attachment(root.path(), Path::new("shot.png")).unwrap();
        let req = super::super::Chat {
            messages: vec![json!({"role":"user","content":"inspect","dial_images":[reference]})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        let original = req.messages.clone();
        let tracker = NoticeTracker::default();
        let mut notices = Vec::new();
        fs::write(&path, [tiny_png(), b"changed".to_vec()].concat()).unwrap();
        let mut previous = None;
        for _ in 0..5 {
            let prepared =
                prepare(&req, true, &tracker, &mut |n| notices.push(n.to_owned())).unwrap();
            let content = prepared.messages[0]["content"].as_str().unwrap();
            assert_eq!(
                content,
                "inspect\n\n[Image unavailable: shot.png; changed; reattach]"
            );
            if let Some(previous) = &previous {
                assert_eq!(&prepared.messages, previous);
            }
            previous = Some(prepared.messages);
        }
        assert_eq!(notices.len(), 1);
        assert_eq!(req.messages, original);

        // A fresh reference to the replacement gets its pixels; the old reference remains invalid.
        let mut refreshed = req.clone();
        refreshed.messages[0]["dial_images"] =
            json!([attachment(root.path(), Path::new("shot.png")).unwrap()]);
        let prepared = prepare(&refreshed, true, &tracker, &mut |_| panic!("fresh image")).unwrap();
        assert_eq!(prepared.messages[0]["content"][1]["type"], "image_url");
        // Successful recovery re-arms the warning if the newly attached file changes again.
        fs::write(&path, tiny_png()).unwrap();
        prepare(&refreshed, true, &tracker, &mut |n| {
            notices.push(n.to_owned())
        });
        assert_eq!(notices.len(), 2);
        let recovered = prepare(&req, true, &tracker, &mut |_| panic!("restored image")).unwrap();
        assert_eq!(recovered.messages[0]["content"][1]["type"], "image_url");
    }

    #[test]
    fn image_notices_distinguish_failure_changes_and_model_capabilities() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("shot.png");
        fs::write(&path, tiny_png()).unwrap();
        let reference = attachment(root.path(), Path::new("shot.png")).unwrap();
        let req = super::super::Chat {
            messages: vec![
                json!({"role":"user","content":"inspect","dial_images":[reference.clone(), reference]}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        let tracker = NoticeTracker::default();
        let mut notices = Vec::new();
        for _ in 0..2 {
            prepare(&req, false, &tracker, &mut |n| notices.push(n.to_owned()));
        }
        assert_eq!(
            notices.len(),
            1,
            "duplicate references and requests share a notice"
        );
        prepare(&req, true, &tracker, &mut |_| panic!("vision works"));
        fs::remove_file(path).unwrap();
        for _ in 0..2 {
            prepare(&req, true, &tracker, &mut |n| notices.push(n.to_owned()));
        }
        assert_eq!(notices.len(), 2);
        assert!(notices[1].contains("file is unavailable"));
        assert_eq!(
            tracker.0.lock().len(),
            1,
            "only current failures are retained"
        );
    }

    #[test]
    fn browser_screenshot_pixels_survive_artifact_storage_and_provider_encoding() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::tools::artifact::ArtifactStore::new(root.path()).unwrap();
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
        let envelope =
            json!({"artifact":{"media_type":"image/png","encoding":"base64","data":encoded}});
        let mut output = crate::tools::ToolOutput {
            execution: Default::default(),
            data: Some(envelope.clone()),
            text: envelope.to_string(),
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
        let prepared = prepare(&req, true, &NoticeTracker::default(), &mut |_| {
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
            let prepared = prepare(&req, vision, &NoticeTracker::default(), &mut |notice| {
                notices.push(notice.to_string())
            })
            .unwrap();
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
        fs::write(root.path().join("tiny.png"), tiny_png()).unwrap();
        let reference = attachment(root.path(), Path::new("tiny.png")).unwrap();
        assert!(reference.get("data").is_none());
        let part = image_part(&reference).unwrap();
        assert!(
            part["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        fs::write(
            root.path().join("tiny.png"),
            [tiny_png(), b"edit".to_vec()].concat(),
        )
        .unwrap();
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
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("image.png"),
                root.path().join("link.png"),
            )
            .unwrap();
            assert!(attachment(root.path(), Path::new("link.png")).is_err());
        }
        let large = fs::File::create(root.path().join("large.png")).unwrap();
        large.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        assert!(attachment(root.path(), Path::new("large.png")).is_err());
    }

    /// A real 1x1 PNG; decoding validates the header chain before any pixels reach a provider.
    fn tiny_png() -> Vec<u8> {
        STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=")
            .unwrap()
    }
}
