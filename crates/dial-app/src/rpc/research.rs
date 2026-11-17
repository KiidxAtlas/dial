//! Learning inspection and evaluation-gated admission, shared with the CLI owners.
use super::coded;
use crate::agent::Runtime;
use crate::config::types::Config;
use crate::learn::{auto, compute, improve};
use crate::storage::store::Store;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

/// Inspect stored admission separately from the live session's actually bound version/vector.
pub(super) fn inspect(agent: &crate::agent::Agent<'_>) -> Result<Value> {
    let mut value = status(agent.effective_config())?;
    value["session_version"] = json!(agent.version());
    value["steering"]["runtime"] = agent.steering_runtime();
    Ok(value)
}
/// Read stored learning evidence without initializing state, loading a model or running a gate.
pub(super) fn status(config: &Config) -> Result<Value> {
    let learning = if config.state_dir.join("state.sqlite3").try_exists()? {
        let store = Store::open_read_only(&config.state_dir)?;
        json!({
            "initialized": true,
            "active": store.active()?,
            "versions": store.versions()?,
            "metrics": store.metrics()?,
            "lesson_proposals": improve::lesson_proposals(&store)?,
            "latest_lora_admission": store.latest_lora_admission()?,
        })
    } else {
        json!({"initialized": false})
    };
    Ok(
        json!({"model": config.model, "learning": learning, "steering": crate::steering::status(config)}),
    )
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Evaluate {
        candidate: i64,
        confirm: bool,
    },
    PromoteSteering {
        name: String,
        #[serde(default)]
        replace: bool,
        confirm: bool,
    },
}

fn request(config: &Config, params: &Value) -> Result<Change> {
    let change: Change = serde_json::from_value(params.clone())
        .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
    match &change {
        Change::Evaluate { candidate, confirm } => {
            if *candidate <= 0 || !confirm {
                return Err(coded(
                    ErrorCode::InvalidParams,
                    "evaluate requires a positive candidate id and confirm: true; it runs held-out checks and can activate the candidate",
                ));
            }
        }
        Change::PromoteSteering { name, confirm, .. } => {
            if !confirm {
                return Err(coded(
                    ErrorCode::InvalidParams,
                    "promote_steering requires confirm: true; it replaces the configured active vector only after the owner's admission checks pass",
                ));
            }
            crate::steering::pending_path(config, name)
                .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
        }
    }
    Ok(change)
}

/// Refuse malformed or unconfirmed requests before the actor unloads resident models.
pub(super) fn validate(config: &Config, params: &Value) -> Result<()> {
    request(config, params).map(|_| ())
}

/// Run on the actor with its foreground compute lease released and resident models unloaded.
/// The caller restores that lease afterward, even on failure. `cancel` is the existing transport
/// interrupt flag; cancellation never writes a persistent operator stop or a rejection verdict.
pub(super) fn action(
    config: &Config,
    params: &Value,
    cancel: Arc<AtomicBool>,
    progress: &mut dyn FnMut(&str),
) -> Result<Value> {
    let change = request(config, params)?;
    compute::with_cancellation(cancel.clone(), || match change {
        Change::PromoteSteering { name, replace, .. } => {
            let mut result = crate::steering::promote(config, &name, replace)?;
            result["apply_settings"] = json!(true);
            Ok(result)
        }
        Change::Evaluate { candidate, .. } => {
            let control = auto::lock_file(&config.state_dir.join("control.lock"))?;
            control
                .try_lock()
                .map_err(std::io::Error::from)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        coded(
                            ErrorCode::Busy,
                            "another improvement operation owns the version store",
                        )
                    } else {
                        error.into()
                    }
                })?;
            let store = Store::open(
                &config.state_dir,
                &config.model,
                &config.use_case,
                &config.harness,
            )?;
            let version = store
                .version(candidate)
                .map_err(|error| coded(ErrorCode::NotFound, error))?;
            if version.status != "candidate" || version.parent != Some(store.active()?.id) {
                return Err(coded(
                    ErrorCode::Conflict,
                    "candidate is not pending against the active version",
                ));
            }
            let _compute = compute::Background::acquire_controlled(config, Some(cancel.as_ref()))?;
            let admission =
                improve::evaluate_queued(&mut Runtime::new(), config, &store, candidate, progress)?;
            Ok(json!({"admission": admission, "active": store.active()?, "apply_settings": true}))
        }
    })
}

#[cfg(test)]
mod tests;
