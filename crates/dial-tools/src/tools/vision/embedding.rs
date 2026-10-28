//! The `image_embedding` role: one vector space for text and images (Qwen3-VL-Embedding or any
//! OpenAI-compatible `/embeddings` endpoint that accepts `image_url` items).

use super::Route;
use crate::config::types::Config;
use crate::llm::server::ModelServer;
use crate::storage::memory_backend::{EmbeddingWire, embedding_url, parse_embeddings};
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub const ROLE: &str = "image_embedding";
/// Inputs per HTTP request; the local server embeds them one at a time.
const BATCH: usize = 8;
/// Longest image edge sent for embedding; the model's own processor resizes further.
const MAX_EDGE: u32 = 1024;
const MAX_RESPONSE: u64 = 64 * 1024 * 1024;

/// One embedding input.
pub enum Input<'a> {
    Text(&'a str),
    /// Encoded PNG, JPEG, GIF or WebP bytes.
    Image(&'a [u8]),
}

/// Whether the `image_embedding` role is assigned.
pub fn configured(config: &Config) -> bool {
    config.role_selection(ROLE).is_some()
}

/// The model identity that keys cached vectors: provider, endpoint and model.
pub fn model_key(config: &Config) -> String {
    let resolved = config.for_role(ROLE);
    format!(
        "{}|{}|{}",
        resolved.provider.as_deref().unwrap_or("mlx"),
        resolved
            .endpoint
            .as_ref()
            .map_or("", |endpoint| endpoint.base_url.as_str()),
        resolved.model
    )
}

/// The local embedding server, kept loaded between calls and replaced when the role changes.
static SERVER: Mutex<Option<(String, ModelServer)>> = Mutex::new(None);

/// Unit-length vectors for `inputs`, in order.
pub fn embed(
    config: &Config,
    inputs: &[Input<'_>],
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Vec<f32>>> {
    ensure!(
        configured(config),
        "no image embedding model is configured; run `/setup image-embeddings` or assign one to the `image_embedding` role in Alt+M"
    );
    let resolved = config.for_role(ROLE);
    let items = inputs
        .iter()
        .map(|input| match input {
            Input::Text(text) => Ok(Value::String((*text).to_owned())),
            Input::Image(bytes) => image_item(bytes),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut vectors = Vec::with_capacity(items.len());
    match super::route(ROLE, &resolved)? {
        Route::Local => {
            let dir = crate::llm::download::ensure_local_model(&resolved.python, &resolved.model)?;
            let mut slot = SERVER.lock();
            if slot
                .as_ref()
                .is_none_or(|(loaded, server)| *loaded != dir || !server.is_running())
            {
                drop(slot.take());
                let server = ModelServer::start_embedding(
                    &resolved.python,
                    &dir,
                    resolved.worker_timeout_secs.max(120),
                    Some(cancel.clone()),
                    Some(&resolved),
                )?;
                *slot = Some((dir.clone(), server));
            }
            let (_, server) = slot.as_ref().context("embedding server is missing")?;
            let url = format!("{}/v1/embeddings", server.base_url());
            for chunk in items.chunks(BATCH) {
                vectors.extend(send(
                    &url,
                    server.request_model(),
                    chunk,
                    None,
                    resolved.worker_timeout_secs,
                    &cancel,
                )?);
            }
        }
        Route::Endpoint {
            base_url,
            api_key_env,
            allow_insecure,
        } => {
            let url = embedding_url(
                EmbeddingWire::OpenAi,
                &base_url,
                allow_insecure,
                &resolved.model,
            )?;
            let token = crate::llm::catalog::resolve_credential(
                &resolved.user_dir,
                &base_url,
                api_key_env.as_deref(),
            )?;
            for chunk in items.chunks(BATCH) {
                vectors.extend(send(
                    &url,
                    &resolved.model,
                    chunk,
                    token.as_deref(),
                    resolved.worker_timeout_secs,
                    &cancel,
                )?);
            }
        }
    }
    Ok(vectors.into_iter().map(normalized).collect())
}

/// Stop a resident local embedding server, if any.
pub fn release() {
    drop(SERVER.lock().take());
}

fn image_item(bytes: &[u8]) -> Result<Value> {
    let (width, height) = crate::llm::images::dimensions(bytes)?;
    let mime = super::image_mime(bytes)?;
    let (bytes, mime) = if width.max(height) > MAX_EDGE || mime == "image/gif" {
        let scaled = image::load_from_memory(bytes)?.resize(
            MAX_EDGE.min(width),
            MAX_EDGE.min(height),
            image::imageops::FilterType::Triangle,
        );
        let mut png = Vec::new();
        scaled.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        crate::llm::images::resize_for_model(&png, "image/png")?
    } else {
        crate::llm::images::resize_for_model(bytes, mime)?
    };
    Ok(json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{mime};base64,{}", STANDARD.encode(bytes))},
    }))
}

fn send(
    url: &str,
    model: &str,
    items: &[Value],
    token: Option<&str>,
    timeout_secs: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<Vec<f32>>> {
    let body = json!({"model": model, "input": items, "encoding_format": "float"});
    let local = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| host == "127.0.0.1" || host == "localhost");
    let mut request = crate::llm::agent_with(timeout_secs, !local, Some(cancel.clone()), None)
        .post(url)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .send(&serde_json::to_vec(&body)?)
        .map_err(|error| anyhow::anyhow!("image embedding request failed ({error})"))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RESPONSE,
        "image embedding response exceeds the size limit"
    );
    if !(200..300).contains(&status) {
        let detail: String = String::from_utf8_lossy(&bytes)
            .chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect();
        let detail = token.map_or(detail.clone(), |token| detail.replace(token, "[redacted]"));
        anyhow::bail!("image embedding endpoint returned HTTP {status}: {detail}");
    }
    parse_embeddings(EmbeddingWire::OpenAi, &bytes, items.len())
}

fn normalized(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        vector.iter_mut().for_each(|v| *v /= norm);
    }
    vector
}

/// Cosine similarity of two unit vectors.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
