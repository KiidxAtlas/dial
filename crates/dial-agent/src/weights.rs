//! Weight inspection and evaluation-gated weight edits of Dial-owned local MLX models.
//!
//! One owner for every surface (`dial weights`, `/weights`, the `model_weights` tool and the
//! dashboard): requests are validated here, run by the Python worker (`python/dial_weights.py`)
//! and the latest result per operation is stored under `<state>/weights/`. Inspections never write
//! model files. Edits ([`edits`]) write a new model copy under the state directory and never touch
//! the base model; a copy becomes the project's model only after its own held-out evaluation
//! passes. Results never contain prompt text; answer text appears only as the bounded `tail` a
//! generation asks for.
use crate::config::paths::Layout;
use crate::config::types::Config;
use crate::learn::{auto, worker};
use crate::llm::local_format::{self, LocalFormat};
use crate::platform::atomic::{private_dir, read_private, write_private};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub use edits::{
    MAX_BUDGET, MIN_EVALUATION_CASES, ModelSwitch, create_experiment, evaluate, experiment,
    promotion, reversion,
};

/// Mirrors the worker's limits so callers learn about a bad request before a model loads.
const MAX_PAIRS: usize = 64;
const MAX_CASES: usize = 16;
const MAX_PREFIX: usize = 512;
const MAX_LAYER: u64 = 1024;
const MAX_GENERATE_CASES: usize = 32;
const MAX_NEW_TOKENS: u64 = 4096;
const MAX_TAIL: u64 = 400;
const MAX_EDIT_LAYERS: usize = 16;
const MAX_EDIT_COLUMNS: u64 = 4096;
/// Stored and returned tensor rows; per-layer aggregates always cover the whole checkpoint.
const MAX_TENSOR_ROWS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Checkpoint layout and parameter counts from safetensors headers (no model load).
    Map,
    /// Per-tensor and per-layer weight statistics, read lazily (no forward pass).
    Stats,
    /// Per-layer activation contrast for matched response pairs.
    Activations,
    /// Answer-loss change with each layer, or each head of one layer, ablated.
    Trace,
    /// Greedy answers per case: length, token-limit hits, repetition and response checks.
    Generate,
    /// Per-layer unit direction between unwanted and wanted answers, saved for an edit.
    Direction,
    /// A new model copy with a saved direction projected out of chosen layers' MLP outputs.
    Edit,
}

impl Operation {
    pub const ALL: [Self; 7] = [
        Self::Map,
        Self::Stats,
        Self::Activations,
        Self::Trace,
        Self::Generate,
        Self::Direction,
        Self::Edit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::Stats => "stats",
            Self::Activations => "activations",
            Self::Trace => "trace",
            Self::Generate => "generate",
            Self::Direction => "direction",
            Self::Edit => "edit",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|op| op.name() == name)
            .with_context(|| {
                format!(
                    "unknown weights operation {name:?}; expected map, stats, activations, trace, generate, direction or edit"
                )
            })
    }

    /// Whether the operation loads the model and runs forward passes.
    pub fn loads_model(self) -> bool {
        matches!(
            self,
            Self::Activations | Self::Trace | Self::Generate | Self::Direction
        )
    }

    /// Whether the operation belongs to an experiment, whose budget it is charged against.
    pub fn budgeted(self) -> bool {
        matches!(self, Self::Generate | Self::Direction | Self::Edit)
    }
}

/// The local MLX checkpoint directory for the configured model, or why it cannot be inspected.
pub fn target(config: &Config) -> Result<PathBuf> {
    ensure!(
        config.endpoint.is_none() && config.provider.as_deref().is_none_or(|p| p == "mlx"),
        "weight inspection needs a Dial-owned local MLX model; hosted and external endpoints expose no weights"
    );
    let resolved = PathBuf::from(crate::llm::catalog::resolve_local_model(&config.model)?);
    if local_format::gguf_file(&resolved).is_some() {
        bail!(
            "GGUF models are not supported yet; weight inspection reads MLX safetensors checkpoints"
        );
    }
    ensure!(
        local_format::detect(&resolved) == Some(LocalFormat::Mlx),
        "{} is not an MLX checkpoint (config.json with safetensors)",
        resolved.display()
    );
    Ok(resolved)
}

/// A name for an experiment, direction or edit; it names a file under `<state>/weights/`.
fn valid_name(name: &str, what: &str) -> Result<String> {
    ensure!(
        name.len() <= 64
            && name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "{what} name must be 1-64 letters, digits, '-' or '_', starting with a letter or digit"
    );
    Ok(name.to_owned())
}

fn name(value: Option<&Value>, what: &str) -> Result<String> {
    let name = value
        .and_then(Value::as_str)
        .with_context(|| format!("{what} name is required"))?;
    valid_name(name, what)
}

/// 1–`max` `{messages}` conversations under `key`, each ending in a `last` turn.
fn conversations<'a>(
    value: Option<&'a Value>,
    key: &str,
    max: usize,
    last: &str,
) -> Result<&'a Vec<Value>> {
    let list = value.and_then(Value::as_array).with_context(|| {
        format!("`{key}` must be an array of {{messages}} conversations ending in the {last} turn")
    })?;
    ensure!(
        (1..=max).contains(&list.len()),
        "`{key}` needs 1–{max} conversations"
    );
    for (number, item) in list.iter().enumerate() {
        ensure!(
            item["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .is_some_and(|message| message["role"] == last),
            "{key} {number} must end with the {last} turn"
        );
    }
    Ok(list)
}

/// Validate operation-specific input and return only the fields the worker reads. Direction and
/// edit requests also keep their `name` (and the edit its `direction` name); [`run`] turns those
/// into state-directory paths.
pub fn arguments(op: Operation, input: &Value) -> Result<Value> {
    ensure!(
        input.is_null() || input.is_object(),
        "weights input must be a JSON object"
    );
    let field = |key: &str| input.get(key).filter(|value| !value.is_null());
    let mut out = json!({});
    match op {
        Operation::Map => {}
        Operation::Stats => {
            if let Some(prefix) = field("prefix") {
                let prefix = prefix.as_str().context("prefix must be a string")?;
                ensure!(
                    prefix.chars().count() <= MAX_PREFIX,
                    "prefix must be at most {MAX_PREFIX} characters"
                );
                out["prefix"] = json!(prefix);
            }
        }
        Operation::Activations => {
            let pairs = field("pairs").and_then(Value::as_array).context(
                "activations need a `pairs` array of {positive, negative} message lists",
            )?;
            ensure!(
                (1..=MAX_PAIRS).contains(&pairs.len()),
                "activations need 1–{MAX_PAIRS} contrast pairs"
            );
            for (number, pair) in pairs.iter().enumerate() {
                ensure!(
                    pair["positive"].is_array() && pair["negative"].is_array(),
                    "pair {number} needs `positive` and `negative` message lists"
                );
            }
            out["pairs"] = json!(pairs);
        }
        Operation::Trace => {
            let cases = field("cases").and_then(Value::as_array).context(
                "trace needs a `cases` array of {messages} ending in an assistant answer",
            )?;
            ensure!(
                (1..=MAX_CASES).contains(&cases.len()),
                "trace needs 1–{MAX_CASES} cases"
            );
            for (number, case) in cases.iter().enumerate() {
                ensure!(
                    case["messages"]
                        .as_array()
                        .and_then(|messages| messages.last())
                        .is_some_and(|last| last["role"] == "assistant"),
                    "case {number} must end with the assistant answer to score"
                );
            }
            out["cases"] = json!(cases);
            let mode = field("mode").map_or(Some("layers"), Value::as_str);
            match mode {
                Some("layers") => {
                    if let Some(layers) = field("layers") {
                        let layers = layers
                            .as_array()
                            .filter(|layers| !layers.is_empty())
                            .context("layers must be a non-empty array of layer indices")?;
                        ensure!(
                            layers
                                .iter()
                                .all(|layer| layer.as_u64().is_some_and(|n| n < MAX_LAYER)),
                            "layers must be zero-based indices"
                        );
                        out["layers"] = json!(layers);
                    }
                    out["mode"] = json!("layers");
                }
                Some("heads") => {
                    let layer = field("layer")
                        .and_then(Value::as_u64)
                        .filter(|&n| n < MAX_LAYER)
                        .context("heads mode needs a zero-based `layer`")?;
                    out["mode"] = json!("heads");
                    out["layer"] = json!(layer);
                }
                _ => bail!("mode must be layers or heads"),
            }
        }
        Operation::Generate => {
            out["cases"] = json!(conversations(
                field("cases"),
                "cases",
                MAX_GENERATE_CASES,
                "user"
            )?);
            if let Some(limit) = field("max_tokens") {
                let limit = limit
                    .as_u64()
                    .filter(|n| (1..=MAX_NEW_TOKENS).contains(n))
                    .with_context(|| format!("max_tokens must be 1–{MAX_NEW_TOKENS}"))?;
                out["max_tokens"] = json!(limit);
            }
            if let Some(tail) = field("tail") {
                let tail = tail
                    .as_u64()
                    .filter(|&n| n <= MAX_TAIL)
                    .with_context(|| format!("tail must be 0–{MAX_TAIL} characters"))?;
                out["tail"] = json!(tail);
            }
        }
        Operation::Direction => {
            for key in ["positive", "negative"] {
                out[key] = json!(conversations(field(key), key, MAX_PAIRS, "assistant")?);
            }
            let site = field("site").map_or(Some("mlp"), Value::as_str);
            ensure!(
                matches!(site, Some("mlp" | "residual")),
                "site must be mlp or residual"
            );
            out["site"] = json!(site);
            out["name"] = json!(name(field("name"), "direction")?);
        }
        Operation::Edit => {
            out["method"] = json!("project_out");
            out["name"] = json!(name(field("name"), "edit")?);
            out["direction"] = json!(name(field("direction"), "direction")?);
            let layers = field("layers")
                .and_then(Value::as_array)
                .map(|layers| {
                    layers
                        .iter()
                        .map(|layer| layer.as_u64().filter(|&n| n < MAX_LAYER))
                        .collect::<Option<Vec<_>>>()
                })
                .context("edit needs `layers`: zero-based layer indices")?
                .context("layers must be zero-based indices")?;
            ensure!(
                (1..=MAX_EDIT_LAYERS).contains(&layers.len())
                    && layers.iter().collect::<BTreeSet<_>>().len() == layers.len(),
                "edit needs 1–{MAX_EDIT_LAYERS} distinct layers"
            );
            out["layers"] = json!(layers);
            if let Some(columns) = field("columns") {
                let columns = columns
                    .as_u64()
                    .filter(|n| (1..=MAX_EDIT_COLUMNS).contains(n))
                    .with_context(|| format!("columns must be 1–{MAX_EDIT_COLUMNS}"))?;
                out["columns"] = json!(columns);
            }
            if let Some(alpha) = field("alpha") {
                let alpha = alpha
                    .as_f64()
                    .filter(|a| a.is_finite() && *a > 0.0 && *a <= 2.0)
                    .context("alpha must be in (0, 2]")?;
                out["alpha"] = json!(alpha);
            }
        }
    }
    Ok(out)
}

/// Where results, progress and the job lock live.
pub fn directory(config: &Config) -> PathBuf {
    config.state_dir.join("weights")
}

fn result_path(config: &Config, op: Operation) -> PathBuf {
    directory(config).join(format!("{}.json", op.name()))
}

fn progress_path(config: &Config) -> PathBuf {
    directory(config).join("progress.json")
}

/// The project's weights job lock: one job (or ledger change) at a time.
fn lock(config: &Config) -> Result<std::fs::File> {
    let dir = directory(config);
    private_dir(&dir)?;
    let lock = auto::lock_file(&dir.join("job.lock"))?;
    std::fs::File::try_lock(&lock)
        .map_err(std::io::Error::from)
        .context("another weights job is running for this project")?;
    Ok(lock)
}

/// Run `work` holding the job lock and, from the CLI (`layout`), foreground compute. An
/// interactive session already holds that compute lock and passes `None`.
fn job<T>(
    config: &Config,
    layout: Option<&Layout>,
    cancel: &AtomicBool,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lock = lock(config)?;
    let _compute = match layout {
        Some(layout) if config.compute_locks => {
            let lock = auto::compute_lock_cancellable(layout, || cancel.load(Ordering::SeqCst))?;
            Some(lock.context("weights job cancelled while waiting for compute")?)
        }
        _ => None,
    };
    let result = work();
    let _ = std::fs::remove_file(progress_path(config));
    result
}

/// One worker call for `model`; the caller holds the job lock.
fn call_worker(
    config: &Config,
    model: &Path,
    op: Operation,
    mut request: Value,
    adapter: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<Value> {
    let progress = progress_path(config);
    write_private(
        &progress,
        json!({"phase": "starting", "op": op.name()})
            .to_string()
            .as_bytes(),
    )?;
    request["op"] = json!(format!("weights_{}", op.name()));
    request["model"] = json!(model);
    request["progress"] = json!(progress);
    if let Some(adapter) = adapter {
        request["adapter"] = json!(adapter);
    }
    worker::call_controlled(config, &request, Some(cancel))
}

/// Run one operation and store it as the latest result for its operation.
///
/// `layout` is the CLI's: the job then joins foreground compute (a shared `compute.lock` that
/// background training yields to). An interactive session already holds that lock and passes
/// `None`. One weights job runs per project at a time. Generate, direction and edit runs name an
/// `experiment`, whose budget is charged before the worker starts.
pub fn run(
    config: &Config,
    layout: Option<&Layout>,
    op: Operation,
    input: &Value,
    adapter: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<Value> {
    let model = target(config)?;
    let mut request = arguments(op, input)?;
    let experiment = match input.get("experiment").filter(|value| !value.is_null()) {
        Some(value) => Some(name(Some(value), "experiment")?),
        None if op.budgeted() => bail!(
            "{} runs belong to an experiment; pass `experiment` (create one with `dial weights experiment NAME --budget N`)",
            op.name()
        ),
        None => None,
    };
    let adapter = adapter
        .map(std::fs::canonicalize)
        .transpose()
        .context("adapter must exist locally")?;
    ensure!(
        adapter.as_ref().is_none_or(|path| path.is_dir()),
        "adapter must be a directory"
    );
    job(config, layout, cancel, move || {
        let created = edits::prepare(config, op, &mut request, experiment.as_deref())?;
        let charged = experiment
            .as_deref()
            .map(|name| edits::charge(config, name, op, &request))
            .transpose()?;
        let result = call_worker(config, &model, op, request, adapter.as_deref(), cancel);
        let mut value = bounded(result?);
        value["created_at"] = json!(crate::learn::improve::now());
        if let Some(charged) = charged {
            value["experiment"] = charged;
        }
        if let (Some(created), Some(experiment)) = (created, experiment.as_deref()) {
            edits::record(config, created, experiment, &model)?;
        }
        write_private(&result_path(config, op), value.to_string().as_bytes())?;
        Ok(value)
    })
}

/// Keep stored and returned results bounded: the per-tensor list is capped, aggregates are not.
pub fn bounded(mut value: Value) -> Value {
    if let Some(rows) = value.get_mut("tensors").and_then(Value::as_array_mut)
        && rows.len() > MAX_TENSOR_ROWS
    {
        let total = rows.len();
        rows.truncate(MAX_TENSOR_ROWS);
        value["tensors_total"] = json!(total);
        value["tensors_truncated"] = json!(true);
    }
    value
}

fn stored(path: &Path) -> Result<Option<Value>> {
    if !path.parent().is_some_and(Path::is_dir) {
        return Ok(None);
    }
    read_private(path)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).context("stored weights result is invalid JSON")
        })
        .transpose()
}

/// The latest stored result of `op`, if any.
pub fn latest(config: &Config, op: Operation) -> Result<Option<Value>> {
    stored(&result_path(config, op))
}

/// The latest result of each operation and evaluation (null when absent), the edited copies and
/// any running job's progress.
pub fn status(config: &Config) -> Value {
    let mut out = json!({"model": config.model});
    out["target"] = match target(config) {
        Ok(path) => json!({"path": path}),
        Err(error) => json!({"unsupported": format!("{error:#}")}),
    };
    for op in Operation::ALL {
        out[op.name()] = latest(config, op).ok().flatten().unwrap_or(Value::Null);
    }
    out["evaluate"] = stored(&edits::evaluation_path(config))
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    out["edits"] = json!(edits::summaries(config));
    out["progress"] = stored(&progress_path(config))
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    out
}

/// Whether [`status`] holds no result, edit or running job.
pub fn is_empty(status: &Value) -> bool {
    Operation::ALL.iter().all(|op| status[op.name()].is_null())
        && status["evaluate"].is_null()
        && status["edits"].as_array().is_none_or(Vec::is_empty)
        && status["progress"].is_null()
}

/// One labelled bar per row of `key`, scaled to the largest magnitude.
pub fn bars(rows: &[Value], label: &str, key: &str, width: usize) -> Vec<String> {
    let value = |row: &Value| row[key].as_f64().filter(|v| v.is_finite());
    let max = rows
        .iter()
        .filter_map(value)
        .map(f64::abs)
        .fold(0.0, f64::max);
    rows.iter()
        .map(|row| {
            let name = row[label].to_string();
            match value(row) {
                Some(v) => {
                    let filled = if max > 0.0 {
                        ((v.abs() / max) * width as f64).round() as usize
                    } else {
                        0
                    };
                    let sign = if v < 0.0 { "-" } else { " " };
                    format!(
                        "{name:>4} {sign}{}{} {v:.4}",
                        "█".repeat(filled),
                        "·".repeat(width - filled.min(width))
                    )
                }
                None => format!("{name:>4}  n/a"),
            }
        })
        .collect()
}

/// Compact terminal summary of one result.
pub fn render(value: &Value) -> String {
    let op = value["op"].as_str().unwrap_or("unknown");
    let model = value["model_id"]
        .as_str()
        .or_else(|| value["model"].as_str())
        .unwrap_or("unknown model");
    let mut out = vec![format!("Weights {op} · {model}")];
    let rows = |key: &str| value[key].as_array().cloned().unwrap_or_default();
    match op {
        "map" => {
            let arch = &value["architecture"];
            let totals = &value["totals"];
            out.push(format!(
                "{} · {} layers · hidden {} · {} heads · {} tensors · {} params · {} bytes",
                arch["model_type"].as_str().unwrap_or("unknown"),
                arch["layers"],
                arch["hidden_size"],
                arch["heads"],
                totals["tensors"],
                totals["params"],
                totals["bytes"]
            ));
            out.push("Parameters per layer:".into());
            out.extend(bars(&rows("layers"), "layer", "params", 24));
        }
        "stats" => {
            out.push("Weight norm per layer:".into());
            out.extend(bars(&rows("layers"), "layer", "norm", 24));
        }
        "activations" => {
            out.push(format!(
                "{} pairs · hidden {} · suggested layer {}",
                value["pairs"], value["dimension"], value["suggested_layer"]
            ));
            out.push("Contrast score per layer:".into());
            out.extend(bars(&rows("layers"), "layer", "score", 24));
        }
        "trace" if value["mode"] == "heads" => {
            out.push(format!(
                "Layer {} heads · baseline loss {} over {} answer tokens",
                value["layer"], value["baseline_loss"], value["answer_tokens"]
            ));
            out.push("Loss increase when the head is removed:".into());
            out.extend(bars(&rows("heads"), "head", "delta", 24));
        }
        "trace" => {
            out.push(format!(
                "Baseline loss {} over {} answer tokens",
                value["baseline_loss"], value["answer_tokens"]
            ));
            out.push("Loss increase when the layer is skipped:".into());
            out.extend(bars(&rows("layers"), "layer", "delta", 24));
        }
        "generate" => {
            let summary = &value["summary"];
            out.push(format!(
                "{} cases · {} hit the {}-token limit · mean {} tokens · mean repeat ratio {} · {} of {} checked cases passed",
                summary["cases"],
                summary["hit_limit"],
                value["max_tokens"],
                summary["mean_tokens"],
                summary["mean_repeat_ratio"],
                summary["passed"],
                summary["checked"]
            ));
            out.push("Tokens per case:".into());
            out.extend(bars(&rows("cases"), "case", "tokens", 24));
        }
        "direction" => {
            out.push(format!(
                "{} positive / {} negative conversations · site {} · suggested layers {}",
                value["positive"], value["negative"], value["site"], value["suggested_layers"]
            ));
            out.push("Separation per layer (gap in pooled standard deviations):".into());
            out.extend(bars(&rows("layers"), "layer", "separation", 24));
        }
        "edit" => {
            out.push(format!(
                "{} · {} columns per layer · alpha {} · {} parameters edited, {} rewritten ({} of the model){}",
                value["output"].as_str().unwrap_or("?"),
                value["columns"],
                value["alpha"],
                value["edited_params"],
                value["rewritten_params"],
                value["fraction_changed"],
                if value["quantized"] == true {
                    " · quantized"
                } else {
                    ""
                }
            ));
            out.push("Edit norm per layer:".into());
            out.extend(bars(&rows("layers"), "layer", "edit_norm", 24));
            if value["quantized"] == true {
                out.push("Requantization error per layer:".into());
                out.extend(bars(&rows("layers"), "layer", "requantization_error", 24));
            }
        }
        "evaluate" => {
            out.push(format!(
                "Edit {}: {} · {} wins, {} losses, {} ties over {} held-out cases · p {}",
                value["edit"].as_str().unwrap_or("?"),
                if value["passed"] == true {
                    "passed"
                } else {
                    "not passed"
                },
                value["wins"],
                value["losses"],
                value["ties"],
                value["cases"],
                value["p_value"]
            ));
            for (label, key) in [("base", "base"), ("edit", "edited")] {
                let summary = &value[key];
                out.push(format!(
                    "{label:>4}: {} token-limit hits · mean {} tokens · mean repeat ratio {} · {} of {} checked cases passed",
                    summary["hit_limit"],
                    summary["mean_tokens"],
                    summary["mean_repeat_ratio"],
                    summary["passed"],
                    summary["checked"]
                ));
            }
            if let Some(reason) = value["reason"].as_str() {
                out.push(format!("Not admitted: {reason}"));
            }
        }
        "experiment" => out.push(format!(
            "Experiment {}: {} of {} model runs used · {} prompts seen over {} runs",
            value["experiment"].as_str().unwrap_or("?"),
            value["used"],
            value["budget"],
            value["prompts"],
            value["runs"]
        )),
        _ => out.push(value.to_string()),
    }
    if let Some(experiment) = value["experiment"].as_object()
        && op != "evaluate"
    {
        out.push(format!(
            "Experiment {}: {} of {} model runs used",
            experiment["name"].as_str().unwrap_or("?"),
            experiment["used"],
            experiment["budget"]
        ));
    }
    out.join("\n")
}

/// Terminal summary of [`status`]: the target, any running job, every stored result and the
/// edited copies.
pub fn render_status(status: &Value) -> String {
    let target = &status["target"];
    let mut out = vec![match target["path"].as_str() {
        Some(path) => format!("Model: {path}"),
        None => format!(
            "Unsupported model: {}",
            target["unsupported"].as_str().unwrap_or("unknown")
        ),
    }];
    let progress = &status["progress"];
    if !progress.is_null() {
        let counts = match (progress["completed"].as_u64(), progress["total"].as_u64()) {
            (Some(done), Some(total)) => format!(" {done}/{total}"),
            _ => String::new(),
        };
        out.push(format!(
            "Running: {}{counts}",
            progress["phase"].as_str().unwrap_or("working")
        ));
    }
    let results = Operation::ALL
        .iter()
        .map(|op| &status[op.name()])
        .chain([&status["evaluate"]]);
    for result in results.filter(|result| !result.is_null()) {
        out.push(String::new());
        out.push(render(result));
    }
    let edits = status["edits"].as_array().cloned().unwrap_or_default();
    if !edits.is_empty() {
        out.push(String::new());
        out.push("Edited copies:".into());
        for edit in edits {
            let state = match edit["passed"].as_bool() {
                Some(true) => "evaluation passed",
                Some(false) => "evaluation not passed",
                None => "not evaluated",
            };
            out.push(format!(
                "  {} · experiment {} · {state}",
                edit["name"].as_str().unwrap_or("?"),
                edit["experiment"].as_str().unwrap_or("?")
            ));
        }
    }
    if is_empty(status) {
        out.push("No results yet. Run `dial weights map` or `/weights map` first.".into());
    }
    out.join("\n")
}

pub mod dashboard;
mod edits;
pub mod tool;

#[cfg(test)]
mod tests;
