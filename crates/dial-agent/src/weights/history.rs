//! Durable history of weights runs.
//!
//! Every successful operation and evaluation is kept as `<state>/weights/runs/<id>.json` next to
//! the latest-per-operation result, so earlier results stay comparable after a newer run replaces
//! the latest one. A small index (`runs/index.json`, oldest first) lists them for the status and
//! the dashboard without reading every result. Only the newest [`MAX_RUNS`] are kept.
use super::{directory, render, stored};
use crate::config::types::Config;
use crate::learn::improve::now;
use crate::platform::atomic::{private_dir, write_private};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Runs kept on disk; older ones are removed when a new run is kept.
pub const MAX_RUNS: usize = 200;
/// Runs listed in the status.
const MAX_LISTED_RUNS: usize = 50;

fn runs_dir(config: &Config) -> PathBuf {
    directory(config).join("runs")
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn index(dir: &Path) -> Vec<Value> {
    stored(&index_path(dir))
        .ok()
        .flatten()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

/// A run id names a file in `runs/`: digits, lowercase letters, `_` and `-` only.
fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-' || b == b'_'),
        "{id:?} is not a weights run id (see `dial weights history`)"
    );
    Ok(())
}

/// The experiment a result belongs to: an object (`run`) or a plain name (`evaluate`).
fn experiment_of(value: &Value) -> Value {
    value["experiment"]
        .as_str()
        .or_else(|| value["experiment"]["name"].as_str())
        .map_or(Value::Null, |name| json!(name))
}

/// The index entry of a kept result: its id, operation, time and one line of what it found.
fn entry(id: &str, value: &Value) -> Value {
    json!({
        "id": id,
        "op": value["op"],
        "created_at": value["created_at"],
        "experiment": experiment_of(value),
        "edit": value["edit"],
        "model_id": value["model_id"],
        "summary": render(value).lines().nth(1).unwrap_or_default(),
    })
}

/// Give `value` a `run_id` and keep it in the history, removing the oldest runs beyond
/// [`MAX_RUNS`]. The caller holds the job lock.
pub(super) fn keep(config: &Config, value: &mut Value) -> Result<String> {
    keep_at_most(config, value, MAX_RUNS)
}

fn keep_at_most(config: &Config, value: &mut Value, limit: usize) -> Result<String> {
    let op: String = value["op"]
        .as_str()
        .unwrap_or("run")
        .chars()
        .filter(char::is_ascii_lowercase)
        .collect();
    let at = value["created_at"].as_u64().unwrap_or_else(now);
    // Zero-padded seconds keep ids in time order when sorted as text.
    let id = format!("{at:012}-{op}-{:04x}", rand::random::<u16>());
    value["run_id"] = json!(id);
    let dir = runs_dir(config);
    private_dir(&dir)?;
    write_private(
        &dir.join(format!("{id}.json")),
        value.to_string().as_bytes(),
    )?;
    let mut rows = index(&dir);
    rows.push(entry(&id, value));
    let excess = rows.len().saturating_sub(limit);
    for old in rows.drain(..excess) {
        if let Some(old) = old["id"].as_str().filter(|old| valid_id(old).is_ok()) {
            let _ = std::fs::remove_file(dir.join(format!("{old}.json")));
        }
    }
    write_private(&index_path(&dir), Value::from(rows).to_string().as_bytes())?;
    Ok(id)
}

#[cfg(test)]
pub(super) fn keep_for_tests(config: &Config, value: &mut Value, limit: usize) -> String {
    keep_at_most(config, value, limit).unwrap()
}

/// Kept runs, newest first, optionally only those of operation `op`.
pub fn history(config: &Config, op: Option<&str>) -> Vec<Value> {
    let mut rows = index(&runs_dir(config));
    rows.reverse();
    rows.retain(|row| op.is_none_or(|op| row["op"] == op));
    rows
}

/// The newest runs for the status.
pub(super) fn recent(config: &Config) -> Vec<Value> {
    let mut rows = history(config, None);
    rows.truncate(MAX_LISTED_RUNS);
    rows
}

/// The full result of kept run `id`.
pub fn show_run(config: &Config, id: &str) -> Result<Value> {
    valid_id(id)?;
    stored(&runs_dir(config).join(format!("{id}.json")))?
        .with_context(|| format!("no weights run {id} (it may have been pruned)"))
}

/// Remove all but the newest `keep` runs; returns how many were removed.
pub fn prune(config: &Config, keep: usize) -> Result<usize> {
    let _lock = super::lock(config)?;
    let dir = runs_dir(config);
    let mut rows = index(&dir);
    let excess = rows.len().saturating_sub(keep);
    for old in rows.drain(..excess) {
        if let Some(old) = old["id"].as_str().filter(|old| valid_id(old).is_ok()) {
            let _ = std::fs::remove_file(dir.join(format!("{old}.json")));
        }
    }
    if dir.is_dir() {
        write_private(&index_path(&dir), Value::from(rows).to_string().as_bytes())?;
    }
    Ok(excess)
}

/// What changed between two `analyze` results of models with the same layer count: per-layer
/// importance (loss rise when the layer is skipped), head roles and expert-routing entropy.
pub fn compare_analyses(before: &Value, after: &Value) -> Result<Value> {
    for value in [before, after] {
        ensure!(
            value["op"] == "analyze",
            "only analyze runs can be compared; {} is a {} run",
            value["run_id"].as_str().unwrap_or("this run"),
            value["op"].as_str().unwrap_or("unknown")
        );
    }
    let layers = |value: &Value| value["layers"].as_array().cloned().unwrap_or_default();
    let (old, new) = (layers(before), layers(after));
    ensure!(
        old.len() == new.len(),
        "the runs analyzed models with different layer counts ({} and {})",
        old.len(),
        new.len()
    );
    let rows: Vec<Value> = old
        .iter()
        .zip(&new)
        .map(|(a, b)| {
            let delta = |pick: fn(&Value) -> Option<f64>| match (pick(a), pick(b)) {
                (Some(x), Some(y)) => json!(y - x),
                _ => Value::Null,
            };
            json!({
                "layer": b["layer"],
                "importance": delta(|row| row["importance"].as_f64()),
                "routing_entropy": delta(|row| row["experts"]["entropy"].as_f64()),
                "head_roles": {"before": a["head_roles"], "after": b["head_roles"]},
            })
        })
        .collect();
    let loss = |value: &Value| value["baseline_loss"].as_f64();
    Ok(json!({
        "op": "compare",
        "before": before["run_id"],
        "after": after["run_id"],
        "models": {"before": before["model_id"], "after": after["model_id"]},
        "baseline_loss": {"before": loss(before), "after": loss(after)},
        "heads_by_role": {"before": before["summary"]["heads_by_role"],
                          "after": after["summary"]["heads_by_role"]},
        "layers": rows,
    }))
}

#[cfg(test)]
mod tests;
