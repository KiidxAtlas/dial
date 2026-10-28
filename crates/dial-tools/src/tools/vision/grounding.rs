//! The `grounding` role: given a screenshot and an element description, a GUI grounding model
//! (UI-Venus, UI-TARS, ShowUI or any vision chat model) answers with one click point.
//!
//! Models answer in different coordinate spaces, so the reply is mapped back to the pixels of the
//! screenshot the caller passed in:
//! - Qwen3-VL based models (UI-Venus-1.5, Qwen3-VL) answer on a 0-1000 grid;
//! - Qwen2/2.5-VL based models (UI-TARS-1.5) answer in pixels of the image after the processor's
//!   `smart_resize` (edges rounded to a multiple of 28, bounded pixel count);
//! - ShowUI answers fractions in 0-1.

use super::Route;
use crate::config::types::Config;
use crate::llm::Chat;
use crate::llm::provider::{Backend, ProviderKind};
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub const ROLE: &str = "grounding";
/// Longest screenshot edge sent to the grounding model; larger captures are scaled down first.
const MAX_EDGE: u32 = 1920;
const MAX_REPLY_TOKENS: u32 = 64;

/// The coordinate space a grounding model answers in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Coordinates {
    /// 0-1000 on both axes (Qwen3-VL family, UI-Venus-1.5).
    Thousandths,
    /// Pixels of the image after Qwen2-VL `smart_resize` (UI-TARS-1.5).
    SmartResize {
        factor: u32,
        min_pixels: u64,
        max_pixels: u64,
    },
    /// Fractions 0-1 (ShowUI).
    Unit,
    /// Unknown model: fractions when both values are at most 1, else the 0-1000 grid.
    Auto,
}

/// UI-TARS-1.5 `preprocessor_config.json` bounds, used when an endpoint model gives no config.
const QWEN2_VL_RESIZE: Coordinates = Coordinates::SmartResize {
    factor: 28,
    min_pixels: 3136,
    max_pixels: 12_845_056,
};

/// A grounded point in the pixels of the screenshot that was passed in.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub model: String,
    pub reply: String,
}

impl Located {
    pub fn to_json(&self) -> Value {
        json!({
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "model": self.model,
        })
    }
}

/// Whether the `grounding` role is assigned.
pub fn configured(config: &Config) -> bool {
    config.role_selection(ROLE).is_some()
}

/// The coordinate space from a model's identity: its local `config.json` when there is one,
/// otherwise its name.
pub fn coordinates_for(model: &str, local_dir: Option<&Path>) -> Coordinates {
    if let Some(dir) = local_dir
        && let Some(config) = read_json(&dir.join("config.json"))
    {
        match config["model_type"].as_str() {
            Some("qwen2_vl" | "qwen2_5_vl") => return local_resize(dir),
            Some("qwen3_vl" | "qwen3_vl_moe") => return Coordinates::Thousandths,
            _ => {}
        }
    }
    let name = model.to_ascii_lowercase();
    if name.contains("showui") {
        Coordinates::Unit
    } else if name.contains("ui-tars")
        || name.contains("uitars")
        || name.contains("qwen2.5-vl")
        || name.contains("qwen2_5_vl")
        || name.contains("qwen2-vl")
    {
        QWEN2_VL_RESIZE
    } else if name.contains("venus") || name.contains("qwen3-vl") || name.contains("qwen3_vl") {
        Coordinates::Thousandths
    } else {
        Coordinates::Auto
    }
}

fn local_resize(dir: &Path) -> Coordinates {
    let Coordinates::SmartResize {
        factor,
        min_pixels,
        max_pixels,
    } = QWEN2_VL_RESIZE
    else {
        unreachable!("QWEN2_VL_RESIZE is a smart-resize space");
    };
    let Some(pre) = read_json(&dir.join("preprocessor_config.json")) else {
        return QWEN2_VL_RESIZE;
    };
    let patch = pre["patch_size"].as_u64().unwrap_or(14);
    let merge = pre["merge_size"].as_u64().unwrap_or(2);
    Coordinates::SmartResize {
        factor: u32::try_from(patch * merge).unwrap_or(factor),
        min_pixels: pre["min_pixels"].as_u64().unwrap_or(min_pixels),
        max_pixels: pre["max_pixels"].as_u64().unwrap_or(max_pixels),
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// The instruction each model family was trained on.
pub fn prompt(coordinates: Coordinates, description: &str) -> String {
    match coordinates {
        Coordinates::SmartResize { .. } => format!(
            "Output only the coordinate of one point in your response. What element matches the following task: {description}"
        ),
        Coordinates::Unit => format!(
            "Based on the screenshot of the page, I give a text description and you give its corresponding location. The coordinate represents a clickable location [x, y] for an element, which is a relative coordinate on the screenshot, scaled from 0 to 1.\n{description}"
        ),
        Coordinates::Thousandths | Coordinates::Auto => format!(
            "Output the center point of the position corresponding to the instruction: {description}. The output should just be the coordinates of a point, in the format [x,y]."
        ),
    }
}

/// Qwen2-VL `smart_resize`: the (width, height) the processor feeds the model.
pub fn smart_resize(
    width: u32,
    height: u32,
    factor: u32,
    min_pixels: u64,
    max_pixels: u64,
) -> (u32, u32) {
    let factor_f = f64::from(factor.max(1));
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let round = |value: f64| ((value / factor_f).round() * factor_f).max(factor_f);
    let (mut w_bar, mut h_bar) = (round(w), round(h));
    if w_bar * h_bar > max_pixels as f64 {
        let beta = (w * h / max_pixels as f64).sqrt();
        w_bar = ((w / beta / factor_f).floor() * factor_f).max(factor_f);
        h_bar = ((h / beta / factor_f).floor() * factor_f).max(factor_f);
    } else if w_bar * h_bar < min_pixels as f64 {
        let beta = (min_pixels as f64 / (w * h)).sqrt();
        w_bar = (w * beta / factor_f).ceil() * factor_f;
        h_bar = (h * beta / factor_f).ceil() * factor_f;
    }
    (w_bar as u32, h_bar as u32)
}

/// Map a grounding reply to a point in a `width`×`height` image. A reply with exactly four
/// numbers is read as a box and yields its center.
pub fn parse_point(
    reply: &str,
    coordinates: Coordinates,
    width: u32,
    height: u32,
) -> Result<(f64, f64)> {
    let numbers: Vec<f64> = regex::Regex::new(r"-?\d+(?:\.\d+)?")
        .expect("valid number pattern")
        .find_iter(reply)
        .filter_map(|m| m.as_str().parse().ok())
        .collect();
    let (x, y) = match numbers.as_slice() {
        [x1, y1, x2, y2] => ((x1 + x2) / 2.0, (y1 + y2) / 2.0),
        [x, y, ..] => (*x, *y),
        _ => anyhow::bail!(
            "the grounding model did not answer with a point: {:?}",
            reply.chars().take(200).collect::<String>()
        ),
    };
    let (w, h) = (f64::from(width), f64::from(height));
    let (x, y) = match coordinates {
        Coordinates::Thousandths => (x * w / 1000.0, y * h / 1000.0),
        Coordinates::Unit => (x * w, y * h),
        Coordinates::SmartResize {
            factor,
            min_pixels,
            max_pixels,
        } => {
            let (rw, rh) = smart_resize(width, height, factor, min_pixels, max_pixels);
            (x * w / f64::from(rw), y * h / f64::from(rh))
        }
        Coordinates::Auto if x <= 1.0 && y <= 1.0 => (x * w, y * h),
        Coordinates::Auto => (x * w / 1000.0, y * h / 1000.0),
    };
    ensure!(
        x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 && x <= w && y <= h,
        "the grounding model answered a point outside the screenshot: {:?}",
        reply.chars().take(200).collect::<String>()
    );
    Ok((x.min(w - 1.0).max(0.0), y.min(h - 1.0).max(0.0)))
}

/// The grounding backend stays loaded between calls; a changed role selection replaces it.
struct Resident {
    key: String,
    backend: Backend,
    coordinates: Coordinates,
    label: String,
}

static RESIDENT: Mutex<Option<Resident>> = Mutex::new(None);

fn resident_key(resolved: &Config) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        resolved.provider.as_deref().unwrap_or("mlx"),
        resolved
            .endpoint
            .as_ref()
            .map_or("", |endpoint| endpoint.base_url.as_str()),
        resolved.model
    )
}

fn build(resolved: &Config) -> Result<Resident> {
    let route = super::route(ROLE, resolved)?;
    let (kind, local_dir, allow_insecure) = match &route {
        Route::Local => {
            let dir = crate::llm::download::ensure_local_model(&resolved.python, &resolved.model)?;
            (
                ProviderKind::Local {
                    model_dir: dir.clone(),
                },
                Some(dir),
                false,
            )
        }
        Route::Endpoint {
            base_url,
            api_key_env,
            allow_insecure,
        } => (
            ProviderKind::Endpoint {
                base_url: base_url.clone(),
                model: resolved.model.clone(),
                api_key: crate::llm::catalog::resolve_credential(
                    &resolved.user_dir,
                    base_url,
                    api_key_env.as_deref(),
                )?,
            },
            None,
            *allow_insecure,
        ),
    };
    let backend = Backend::new(
        kind,
        resolved.python.clone(),
        resolved.worker_timeout_secs.min(600),
    )
    .with_protocol(Some(crate::llm::model_config::ApiProtocol::ChatCompletions))
    .with_headers(resolved.request_headers().to_vec())?
    .with_allow_insecure(allow_insecure)
    .with_secrets(&resolved.secrets, &resolved.user_dir)?;
    Ok(Resident {
        key: resident_key(resolved),
        backend,
        coordinates: coordinates_for(&resolved.model, local_dir.as_deref().map(Path::new)),
        label: resolved.selection().display_name(),
    })
}

/// Find `description` in `screenshot` (PNG, JPEG, GIF or WebP bytes) with the `grounding` role.
/// The point is in the screenshot's own pixels; nothing is clicked.
pub fn locate(
    config: &Config,
    screenshot: &[u8],
    description: &str,
    cancel: Arc<AtomicBool>,
) -> Result<Located> {
    ensure!(
        configured(config),
        "no grounding model is configured; run `/setup grounding` or assign one to the `grounding` role in Alt+M"
    );
    let description = description.trim();
    ensure!(!description.is_empty(), "describe the element to locate");
    ensure!(
        description.chars().count() <= 500,
        "the element description is limited to 500 characters"
    );
    let (width, height) = crate::llm::images::dimensions(screenshot)?;
    let mime = super::image_mime(screenshot)?;
    let (sent, sent_mime) = fit(screenshot, mime, width, height)?;
    let (sent_width, sent_height) = crate::llm::images::dimensions(&sent)?;
    let resolved = config.for_role(ROLE);
    let mut slot = RESIDENT.lock();
    if slot
        .as_ref()
        .is_none_or(|resident| resident.key != resident_key(&resolved))
    {
        if let Some(mut old) = slot.take() {
            old.backend.release();
        }
        *slot = Some(build(&resolved)?);
    }
    let resident = slot.as_mut().context("grounding backend is missing")?;
    let request = Chat {
        messages: vec![json!({"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": format!("data:{sent_mime};base64,{}", STANDARD.encode(&sent))}},
            {"type": "text", "text": prompt(resident.coordinates, description)},
        ]})],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: MAX_REPLY_TOKENS,
        seed: None,
    };
    let reply = resident
        .backend
        .chat_with_control(
            None,
            &request,
            resolved.worker_timeout_secs,
            None,
            &mut |_| {},
            cancel,
        )
        .context("grounding request failed")?;
    let (x, y) = parse_point(&reply.text, resident.coordinates, sent_width, sent_height)?;
    let scale_x = f64::from(width) / f64::from(sent_width);
    let scale_y = f64::from(height) / f64::from(sent_height);
    Ok(Located {
        x: ((x * scale_x).round() as u32).min(width.saturating_sub(1)),
        y: ((y * scale_y).round() as u32).min(height.saturating_sub(1)),
        width,
        height,
        model: resident.label.clone(),
        reply: reply.text,
    })
}

/// Stop a resident local grounding server, if any.
pub fn release() {
    if let Some(mut resident) = RESIDENT.lock().take() {
        resident.backend.release();
    }
}

/// Scale a screenshot down to [`MAX_EDGE`] (keeping its aspect ratio) and the provider byte limit.
fn fit(bytes: &[u8], mime: &str, width: u32, height: u32) -> Result<(Vec<u8>, String)> {
    if width.max(height) <= MAX_EDGE {
        return crate::llm::images::resize_for_model(bytes, mime);
    }
    let decoded = image::load_from_memory(bytes)?;
    let scaled = decoded.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle);
    let mut png = Vec::new();
    scaled.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    crate::llm::images::resize_for_model(&png, "image/png")
}

#[cfg(test)]
mod tests;
