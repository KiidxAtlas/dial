//! Explicit `--image` attachments for one print turn. Typed references are expanded exactly as
//! interactive input is; the explicit image files are appended as `dial_images` entries.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::agent::window::context::expand_message;
use crate::llm::images::user_attachment;

/// The user text and message for a print turn that carries explicit image files.
pub(crate) struct PreparedTurn {
    pub text: String,
    pub message: Value,
    pub warnings: Vec<String>,
}

/// Fails before any session work when `--image` is used with a model that cannot read images.
pub(super) fn ensure_image_input(supported: bool, images: usize) -> Result<()> {
    ensure!(
        images == 0 || supported,
        "--image needs a vision-capable model; the configured model does not accept image input"
    );
    Ok(())
}

pub(super) fn prepare_turn(
    root: &Path,
    prompt: &str,
    images: &[PathBuf],
    stdin_image: Option<&[u8]>,
    interrupt: &AtomicBool,
) -> Result<PreparedTurn> {
    prepare_attached_turn(root, prompt, images, stdin_image.as_slice(), interrupt)
}

/// One user turn with explicit image files and inline image bytes, shared by `--image` print runs
/// and the RPC `send` method. Typed `@file` references expand exactly as in the terminal.
pub(crate) fn prepare_attached_turn(
    root: &Path,
    prompt: &str,
    images: &[PathBuf],
    inline: &[&[u8]],
    interrupt: &AtomicBool,
) -> Result<PreparedTurn> {
    let (mut message, warnings) = expand_message(root, prompt, interrupt);
    let mut attached = message
        .get("dial_images")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for path in images {
        let image = user_attachment(root, path)
            .with_context(|| format!("attaching image {}", path.display()))?;
        if !attached.contains(&image) {
            attached.push(image);
        }
    }
    for (index, bytes) in inline.iter().enumerate() {
        let image = crate::llm::images::inline_attachment(bytes)
            .with_context(|| format!("attaching inline image {}", index + 1))?;
        if !attached.contains(&image) {
            attached.push(image);
        }
    }
    if !attached.is_empty() {
        message["dial_images"] = Value::Array(attached);
    }
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(PreparedTurn {
        text,
        message,
        warnings,
    })
}

#[cfg(test)]
mod tests;
