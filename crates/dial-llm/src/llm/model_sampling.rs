//! Sampling defaults a local model ships with: `generation_config.json` in an MLX/Hugging Face
//! directory, then the `general.sampling.*` keys llama.cpp converters write into a GGUF header.
//!
//! The answer comes from the files, never from the model's name. Each value is validated on its
//! own, so one bad entry does not discard the rest. A `do_sample: false` config requests greedy
//! decoding (temperature zero), and a GGUF header does not override it.

use super::catalog::{read_json, resolve_local_model};
use super::local_format::gguf_info;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Sampler values a model recommends. `None` fields are unset or were invalid and dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecommendedSampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    /// Where the values came from: `generation_config.json` or `gguf metadata`.
    pub source: &'static str,
}

const GENERATION_CONFIG: &str = "generation_config.json";
const GGUF_METADATA: &str = "gguf metadata";

/// Model-shipped sampling for a local model path or name. `model` is resolved with
/// [`resolve_local_model`]; when that fails it is read as a path. The result is cached process-wide
/// per resolved path. `None` when nothing usable is shipped.
pub fn recommended_sampling(model: &str) -> Option<RecommendedSampling> {
    type Cache = Mutex<HashMap<PathBuf, Option<RecommendedSampling>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let path = resolve_local_model(model)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(model));
    let cache = CACHE.get_or_init(Cache::default);
    let lock = || {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };
    if let Some(known) = lock().get(&path) {
        return known.clone();
    }
    let found = read_model(&path);
    lock().insert(path, found.clone());
    found
}

fn read_model(path: &Path) -> Option<RecommendedSampling> {
    if path.is_dir()
        && let Some(config) = read_json(&path.join(GENERATION_CONFIG))
    {
        if config.get("do_sample").and_then(Value::as_bool) == Some(false) {
            return Some(RecommendedSampling {
                temperature: Some(0.0),
                source: GENERATION_CONFIG,
                ..Default::default()
            });
        }
        let found = validated(
            GENERATION_CONFIG,
            config.get("temperature").and_then(Value::as_f64),
            config.get("top_p").and_then(Value::as_f64),
            config.get("top_k").and_then(Value::as_u64),
            config.get("min_p").and_then(Value::as_f64),
        );
        if found.is_some() {
            return found;
        }
    }
    let info = gguf_info(path)?;
    validated(
        GGUF_METADATA,
        info.sampling_temp,
        info.sampling_top_p,
        info.sampling_top_k,
        info.sampling_min_p,
    )
}

/// Keeps each value inside the sampler's valid range and drops the rest; `None` when nothing remains.
fn validated(
    source: &'static str,
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u64>,
    min_p: Option<f64>,
) -> Option<RecommendedSampling> {
    let found = RecommendedSampling {
        temperature: temperature.filter(|value| (0.0..=2.0).contains(value)),
        top_p: top_p.filter(|value| *value > 0.0 && *value <= 1.0),
        top_k: top_k
            .filter(|value| (1..=1000).contains(value))
            .and_then(|value| u32::try_from(value).ok()),
        min_p: min_p.filter(|value| (0.0..1.0).contains(value)),
        source,
    };
    let any = found.temperature.is_some()
        || found.top_p.is_some()
        || found.top_k.is_some()
        || found.min_p.is_some();
    any.then_some(found)
}

#[cfg(test)]
mod tests;
