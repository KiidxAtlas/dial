//! Sampling defaults a local model ships with: `generation_config.json` in an MLX/Hugging Face
//! directory, then the `general.sampling.*` keys llama.cpp converters write into a GGUF header.
//!
//! The answer comes from the files, never from the model's name. Each value is validated on its
//! own, so one bad entry does not discard the rest. A `do_sample: false` config means the model is
//! published for greedy decoding: there is no recommendation, and a GGUF header does not override it.

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
            return None;
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
mod tests {
    use super::*;
    use crate::llm::local_format::gguf_metadata_tests::{header, string, uint32};
    use std::fs;

    fn float32(value: f32) -> Vec<u8> {
        let mut bytes = 6u32.to_le_bytes().to_vec();
        bytes.extend(value.to_le_bytes());
        bytes
    }

    #[test]
    fn generation_config_values_are_recommended() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(GENERATION_CONFIG),
            r#"{"do_sample": true, "temperature": 0.6, "top_p": 0.95, "top_k": 20, "min_p": 0.0}"#,
        )
        .unwrap();
        let found = recommended_sampling(root.path().to_str().unwrap()).unwrap();
        assert_eq!(
            found,
            RecommendedSampling {
                temperature: Some(0.6),
                top_p: Some(0.95),
                top_k: Some(20),
                min_p: Some(0.0),
                source: GENERATION_CONFIG,
            }
        );
    }

    #[test]
    fn do_sample_false_is_greedy_with_no_recommendation() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(GENERATION_CONFIG),
            r#"{"do_sample": false, "temperature": 0.7, "top_k": 40}"#,
        )
        .unwrap();
        assert_eq!(read_model(root.path()), None);
    }

    #[test]
    fn invalid_values_are_dropped_individually() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(GENERATION_CONFIG);
        fs::write(
            &path,
            r#"{"temperature": 3.0, "top_p": 0.0, "top_k": 0, "min_p": 1.0}"#,
        )
        .unwrap();
        assert_eq!(read_model(root.path()), None);

        fs::write(
            &path,
            r#"{"temperature": 3.0, "top_p": 0.9, "top_k": 5000, "min_p": 0.05}"#,
        )
        .unwrap();
        assert_eq!(
            read_model(root.path()),
            Some(RecommendedSampling {
                temperature: None,
                top_p: Some(0.9),
                top_k: None,
                min_p: Some(0.05),
                source: GENERATION_CONFIG,
            })
        );
    }

    #[test]
    fn gguf_header_sampling_keys_are_recommended() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("model.gguf");
        fs::write(
            &file,
            header(&[
                ("general.architecture", string("llama")),
                ("general.sampling.temp", float32(0.8)),
                ("general.sampling.top_p", float32(0.9)),
                ("general.sampling.top_k", uint32(40)),
                ("general.sampling.min_p", float32(5.0)),
            ]),
        )
        .unwrap();
        let found = recommended_sampling(file.to_str().unwrap()).unwrap();
        assert_eq!(found.source, GGUF_METADATA);
        assert_eq!(found.temperature, Some(f64::from(0.8f32)));
        assert_eq!(found.top_p, Some(f64::from(0.9f32)));
        assert_eq!(found.top_k, Some(40));
        assert_eq!(
            found.min_p, None,
            "min_p 5.0 is outside [0, 1) and is dropped"
        );
    }

    #[test]
    fn generation_config_wins_over_gguf_when_both_are_usable() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(GENERATION_CONFIG), r#"{"top_k": 20}"#).unwrap();
        fs::write(
            root.path().join("model.gguf"),
            header(&[
                ("general.architecture", string("llama")),
                ("general.sampling.top_k", uint32(40)),
            ]),
        )
        .unwrap();
        let found = read_model(root.path()).unwrap();
        assert_eq!((found.source, found.top_k), (GENERATION_CONFIG, Some(20)));
    }

    #[test]
    fn missing_or_unrecommended_models_have_no_sampling() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(recommended_sampling("/nonexistent/dial-model-path"), None);
        assert_eq!(recommended_sampling(root.path().to_str().unwrap()), None);
        fs::write(
            root.path().join(GENERATION_CONFIG),
            r#"{"eos_token_id": 2}"#,
        )
        .unwrap();
        assert_eq!(recommended_sampling(root.path().to_str().unwrap()), None);
    }
}
