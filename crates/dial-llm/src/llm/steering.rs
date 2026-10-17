//! Provider-facing activation steering: the admitted vector specification a local model server
//! applies, its admission checks, and the runtime status it reports. Creation, promotion and
//! automatic improvement stay in `crate::steering`.
use crate::config::types::Config as AppConfig;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Swept strengths are absolute (multiplier × contrast norm at the layer), not the 0..2 setting.
pub const MAX_STRENGTH: f64 = 1e4;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Specification {
    pub path: PathBuf,
    pub id: String,
    pub strength: f64,
    pub fingerprint: String,
}
pub fn vector_path(config: &AppConfig) -> PathBuf {
    config
        .state_dir
        .join("steering")
        .join(format!("{}.json", config.steering.vector))
}

pub fn read(path: &Path) -> Result<Value> {
    let mut data = Vec::new();
    fs::File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_BYTES, "steering JSON exceeds 4 MiB");
    Ok(serde_json::from_slice(&data)?)
}
pub fn check(bundle: &Value, strength: f64) -> Result<()> {
    ensure!(
        strength.is_finite() && strength > 0.0 && strength <= MAX_STRENGTH,
        "steering strength must be finite in (0, {MAX_STRENGTH}]"
    );
    ensure!(bundle["version"] == 1, "unsupported steering format");
    let size = bundle["dimension"]
        .as_u64()
        .context("missing hidden dimension")?;
    ensure!(
        (1..=32768).contains(&size),
        "hidden dimension exceeds budget"
    );
    let layers = bundle["layer_count"]
        .as_u64()
        .context("missing layer count")?;
    ensure!(
        (1..=256).contains(&layers) && bundle["layer"].as_u64().is_some_and(|n| n < layers),
        "invalid layer selection"
    );
    let vector = bundle["vector"].as_array().context("missing vector")?;
    ensure!(vector.len() == size as usize, "vector dimension mismatch");
    let mut norm = 0.;
    for x in vector {
        let x = x.as_f64().context("non-numeric vector")?;
        ensure!(x.is_finite(), "nonfinite vector");
        norm += x * x;
    }
    ensure!(
        (0.99..=1.01).contains(&norm),
        "vector must have unit L2 norm"
    );
    ensure!(
        bundle["pairs"]
            .as_u64()
            .is_some_and(|n| (8..=64).contains(&n)),
        "insufficient contrast pairs"
    );
    ensure!(
        bundle["id"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid artifact ID"
    );
    let e = &bundle["evaluation"];
    let cases = e["cases"].as_u64().unwrap_or(0);
    let base = e["baseline_passes"].as_u64().unwrap_or(33);
    let steered = e["steered_passes"].as_u64().unwrap_or(0);
    ensure!(
        e["passed"] == true
            && e["scope"] == "held-out response contracts"
            && e["artifact"] == bundle["id"]
            && e["strength"].as_f64() == Some(strength),
        "awaiting passing held-out evaluation at this exact strength"
    );
    ensure!(
        (8..=32).contains(&cases)
            && steered <= cases
            && base <= cases
            && steered > base
            && steered as f64 / cases as f64 >= 0.8,
        "evaluation did not improve response contracts"
    );
    ensure!(
        e["tool_nonregression"] == true
            && e["repetition_nonregression"] == true
            && e["latency_nonregression"] == true,
        "evaluation regression gate failed"
    );
    Ok(())
}
/// Automatic vectors run at the strength their sweep selected and evaluation admitted; manual
/// vectors use the configured strength.
pub fn strength(config: &AppConfig, bundle: &Value) -> f64 {
    if bundle["origin"] == "automatic verified repairs" {
        bundle["evaluation"]["strength"].as_f64().unwrap_or(0.0)
    } else {
        config.steering.strength
    }
}

pub fn specification(config: &AppConfig, model: &str) -> Result<Option<Specification>> {
    if (!config.steering.enabled && !config.steering.automatic) || config.steering.strength == 0.0 {
        return Ok(None);
    }
    if let Some(reason) = local_unsupported_reason(model) {
        anyhow::bail!(reason);
    }
    config.steering.validate()?;
    let path = vector_path(config);
    if config.steering.automatic && !path.exists() {
        return Ok(None);
    }
    let bundle = read(&path).map_err(|error| {
        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::NotFound
        }) {
            anyhow::anyhow!(
                "selected vector '{}' is missing at {}. Extract and evaluate a candidate before enabling steering, or disable it in Settings > Model > Activation steering. Coding continues without steering",
                config.steering.vector,
                path.display()
            )
        } else {
            error.context(format!("cannot read steering vector at {}", path.display()))
        }
    })?;
    if !config.steering.enabled && bundle["origin"] != "automatic verified repairs" {
        return Ok(None);
    }
    ensure!(
        bundle["model_name"] == model,
        "vector belongs to a different model"
    );
    let strength = strength(config, &bundle);
    check(&bundle, strength)?;
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&bundle)?));
    Ok(Some(Specification {
        path,
        id: bundle["id"].as_str().context("missing steering id")?.into(),
        strength,
        fingerprint,
    }))
}

/// Match the supervisor's runner choice without starting Python or loading weights.
pub fn local_unsupported_reason(_model: &str) -> Option<&'static str> {
    // Both managed engines have activation hooks. Python probes decoder layers and dimensions
    // before applying an evaluated vector; hosted/external endpoints remain unsupported.
    None
}

pub fn runtime(base_url: &str) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(1)))
        .build()
        .into();
    let mut response = agent.get(&format!("{base_url}/dial/steering")).call()?;
    let raw = response
        .body_mut()
        .with_config()
        .limit(65536)
        .read_to_string()?;
    Ok(serde_json::from_str(&raw)?)
}
