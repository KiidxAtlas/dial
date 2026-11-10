//! Read-only structural maps with bounded, explicitly non-causal usage observations.
use super::{bounded, directory, lock, stored, target, target_name};
use crate::config::types::Config;
use crate::learn::worker;
use crate::platform::atomic::write_private;
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const VERSION: u64 = 1;
const MAX_OBSERVATIONS: usize = 64;
const LIMITS: &str = "Roles are structural inferences from names, not measured semantic functions. Turn outcomes describe the configured agent route, not individual weights or model quality. External serving identity, adapters and fallback routes are not verified. No activations, interventions or weight edits are performed. Fingerprints bind metadata, not all weight bytes.";

/// Content-free facts about a settled agent turn, not a model-quality judgment.
#[derive(Debug)]
pub(crate) struct Observation<'a> {
    pub outcome: &'a str,
    pub steps: u64,
    pub elapsed: Duration,
}

/// Read the last knowledge snapshot without accessing a model or running Python.
///
/// # Errors
/// Returns an error for unreadable, malformed or unsupported stored state.
pub fn stored_report(config: &Config) -> Result<Value> {
    let Some(mut value) = stored(&directory(config).join("knowledge.json"))? else {
        return Ok(Value::Null);
    };
    ensure!(
        value["version"] == VERSION,
        "unsupported model knowledge version"
    );
    value["configured_target_matches"] = json!(value["target"] == target_name(config));
    value["freshness"] =
        json!("stored snapshot; use knowledge --refresh to check current metadata");
    Ok(value)
}

/// Refresh checkpoint headers and structural explanations without loading or editing weights.
///
/// # Errors
/// Returns an error when the target, installed worker, metadata or private storage is unavailable.
pub fn refresh(config: &Config, cancel: &AtomicBool) -> Result<Value> {
    update(config, None, cancel)
}

pub(crate) fn observe(
    config: &Config,
    observation: Observation<'_>,
    cancel: &AtomicBool,
) -> Result<()> {
    if !config.weights.knowledge {
        return Ok(());
    }
    update(config, Some(observation), cancel)?;
    Ok(())
}

fn mapping_timeout_secs(configured: u64) -> u64 {
    // Zero means unlimited for the general worker, not a one-second mapping deadline.
    if configured == 0 {
        5
    } else {
        configured.min(5)
    }
}

fn update(
    config: &Config,
    observation: Option<Observation<'_>>,
    cancel: &AtomicBool,
) -> Result<Value> {
    let model = target(config)?;
    let _lock = lock(config)?;
    let previous = stored_report(config)?;
    let known = if previous["configured_target_matches"] == true {
        previous["atlas"]["model_id"].clone()
    } else {
        Value::Null
    };
    // Header mapping needs no MLX imports or model load. Never provision a Python runtime here,
    // and bound its latency independently of training/inference worker budgets.
    ensure!(
        config.python.is_file(),
        "model knowledge requires the installed Python worker runtime"
    );
    let mut worker_config = config.clone();
    worker_config.worker_timeout_secs = mapping_timeout_secs(config.worker_timeout_secs);
    let response = worker::call_controlled(
        &worker_config,
        &json!({"op": "weights_map", "model": model, "known_model_id": known}),
        Some(cancel),
    )?;
    if let Some(error) = response["error"].as_str() {
        bail!("model knowledge map: {error}");
    }
    let atlas = if response["unchanged"] == true {
        ensure!(
            known.is_string() && response["model_id"] == known,
            "invalid cached model map identity"
        );
        previous["atlas"].clone()
    } else {
        ensure!(
            response["op"] == "map"
                && response["model_id"].is_string()
                && response["knowledge"].is_object()
                && response["tensors"].is_array(),
            "worker returned an incomplete model knowledge map"
        );
        bounded(response)
    };
    let value = merge(config, previous, atlas, observation);
    write_private(
        &directory(config).join("knowledge.json"),
        &serde_json::to_vec(&value)?,
    )?;
    Ok(value)
}

fn merge(
    config: &Config,
    previous: Value,
    atlas: Value,
    observation: Option<Observation<'_>>,
) -> Value {
    let same = previous["target"] == target_name(config)
        && previous["atlas"]["model_id"] == atlas["model_id"];
    let mut observations = if same {
        previous["observations"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if let Some(observation) = observation {
        observations.push(json!({
            "evidence_kind": "agent_turn_observation",
            "recorded_at": crate::learn::improve::now(),
            "configured_model": config.model,
            "configured_provider": config.provider,
            "outcome": observation.outcome,
            "steps": observation.steps,
            "elapsed_seconds": observation.elapsed.as_secs_f64(),
            "causal_attribution": false,
        }));
    }
    let excess = observations.len().saturating_sub(MAX_OBSERVATIONS);
    observations.drain(..excess);
    json!({
        "version": VERSION,
        "target": target_name(config),
        "configured_target_matches": true,
        "updated_at": crate::learn::improve::now(),
        "freshness": "metadata checked at updated_at; not a full weight-content attestation",
        "limits": LIMITS,
        "atlas": atlas,
        "observations": observations,
        "observation_limit": MAX_OBSERVATIONS,
    })
}

/// Render structural roles, predicted local effects and non-causal usage counts.
pub fn render(value: &Value) -> String {
    if value.is_null() {
        return "No model knowledge yet. Run `dial weights knowledge --refresh`, or enable weights.knowledge for collection after turns.".into();
    }
    if let Some(error) = value["error"].as_str() {
        return format!("Model knowledge unavailable: {error}");
    }
    let mut lines = vec![format!(
        "Model knowledge · {}",
        value["target"].as_str().unwrap_or("unknown")
    )];
    if value["configured_target_matches"] == false {
        lines.push("WARNING: this snapshot belongs to a different configured target.".into());
    }
    lines.push(
        value["freshness"]
            .as_str()
            .unwrap_or("freshness unknown")
            .into(),
    );
    lines.push(LIMITS.into());
    lines.push(super::render(&value["atlas"]));
    let knowledge = &value["atlas"]["knowledge"];
    if let Some(roles) = knowledge["roles"].as_object() {
        for (role, details) in roles {
            lines.push(format!(
                "{role} [structural inference]: {} Change: {}",
                details["function"].as_str().unwrap_or("unknown"),
                details["change_effect"].as_str().unwrap_or("unknown")
            ));
        }
    }
    for field in ["linear_effect", "quantization"] {
        if let Some(text) = knowledge[field].as_str() {
            lines.push(text.into());
        }
    }
    let observations = value["observations"].as_array();
    lines.push(format!(
        "Retained agent-turn observations: {} (maximum {MAX_OBSERVATIONS}; not causal evidence).",
        observations.map_or(0, Vec::len)
    ));
    if let Some(last) = observations.and_then(|rows| rows.last()) {
        lines.push(format!(
            "Latest: {} · {} steps · {} seconds · configured model {}",
            last["outcome"].as_str().unwrap_or("unknown"),
            last["steps"],
            last["elapsed_seconds"],
            last["configured_model"].as_str().unwrap_or("unknown")
        ));
    }
    lines.push("`dial weights knowledge --json` includes tensor-to-role mappings and retained observations.".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests;
