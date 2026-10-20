use super::*;
use crate::llm::local_format::gguf_fixtures::{header, string, uint32};
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
fn do_sample_false_recommends_greedy_instead_of_inactive_sampler_values() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join(GENERATION_CONFIG),
        r#"{"do_sample": false, "temperature": 0.7, "top_k": 40}"#,
    )
    .unwrap();
    assert_eq!(
        read_model(root.path()),
        Some(RecommendedSampling {
            temperature: Some(0.0),
            source: GENERATION_CONFIG,
            ..Default::default()
        })
    );
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
