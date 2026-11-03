//! Experiments, edited model copies, held-out evaluation and promotion of weight edits.
//!
//! An experiment is a forward-pass budget. Every generate or direction run names one and is
//! charged one model run before the worker starts; an edit costs nothing but must use a direction
//! measured in the same experiment. Each run records the SHA-256 of the prompts it used (never the
//! text), so an evaluation refuses cases the experiment already saw, and later runs refuse cases an
//! evaluation held out.
//!
//! An evaluation generates on the base and on the edited copy with the same held-out cases. A
//! case is good when it stops before the token limit and passes its response checks (if it has
//! any). The edit passes when it wins more cases than it loses over at least
//! [`MIN_EVALUATION_CASES`] cases and the one-sided sign test gives p ≤ 0.1. Only a passing edit
//! whose files are unchanged since its evaluation can be promoted to the project's model, and a
//! promoted edit can be reverted to its base.
use super::{Operation, arguments, call_worker, directory, job, lock, stored, valid_name};
use crate::config::paths::Layout;
use crate::config::settings_store::Change;
use crate::config::types::Config;
use crate::learn::{gate::sign_test_p, improve::now};
use crate::llm::model_config::ModelSelection;
use crate::platform::atomic::{private_dir, write_private};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

/// Most model runs one experiment may spend; also bounds its ledger.
pub const MAX_BUDGET: u64 = 256;
/// Fewest held-out cases an evaluation may admit an edit on.
pub const MIN_EVALUATION_CASES: usize = 8;
const MAX_P_VALUE: f64 = 0.1;
/// Provenance the worker writes into every edited copy.
const EDIT_RECORD: &str = "weights-edit.json";
const MAX_EDIT_RECORD: u64 = 4 << 20;
/// Edited copies listed in the status.
const MAX_LISTED_EDITS: usize = 32;

#[derive(Debug, Serialize, Deserialize)]
struct Ledger {
    name: String,
    budget: u64,
    used: u64,
    created_at: u64,
    runs: Vec<Run>,
    /// Prompt hashes an evaluation held out; later runs of the experiment may not use them.
    #[serde(default)]
    held_out: BTreeSet<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Run {
    op: String,
    at: u64,
    cost: u64,
    prompts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DirectionRecord {
    name: String,
    experiment: String,
    created_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct EditRecord {
    name: String,
    experiment: String,
    direction: String,
    /// The configured model the edit was made from, as written in the settings.
    base_model: String,
    /// Its resolved checkpoint directory.
    base: PathBuf,
    output: PathBuf,
    created_at: u64,
    #[serde(default)]
    evaluation: Option<Value>,
}

/// What a successful direction or edit run adds to the state directory.
pub(super) enum Created {
    Direction {
        name: String,
    },
    Edit {
        name: String,
        direction: String,
        output: PathBuf,
    },
}

/// The settings writes that switch the project's main model, with a message for the user.
pub struct ModelSwitch {
    pub changes: Vec<Change>,
    pub model: PathBuf,
    pub message: String,
}

fn experiment_path(config: &Config, name: &str) -> PathBuf {
    directory(config)
        .join("experiments")
        .join(format!("{name}.json"))
}

fn direction_path(config: &Config, name: &str) -> PathBuf {
    directory(config)
        .join("directions")
        .join(format!("{name}.safetensors"))
}

fn direction_record_path(config: &Config, name: &str) -> PathBuf {
    directory(config)
        .join("directions")
        .join(format!("{name}.json"))
}

fn edits_dir(config: &Config) -> PathBuf {
    directory(config).join("edits")
}

fn record_path(config: &Config, name: &str) -> PathBuf {
    edits_dir(config).join(format!("{name}.json"))
}

/// The latest evaluation result.
pub(super) fn evaluation_path(config: &Config) -> PathBuf {
    directory(config).join("evaluate.json")
}

fn load<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    stored(path)?
        .map(|value| {
            serde_json::from_value(value)
                .with_context(|| format!("{} is not a valid weights record", path.display()))
        })
        .transpose()
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    private_dir(path.parent().context("weights record has no parent")?)?;
    write_private(path, &serde_json::to_vec(value)?)
}

fn ledger(config: &Config, name: &str) -> Result<Ledger> {
    load(&experiment_path(config, name))?.with_context(|| {
        format!("no experiment named {name}; create one with `dial weights experiment {name} --budget N`")
    })
}

/// Content identity of a prompt: role and content of each message, length-prefixed.
fn prompt_hash(messages: &[Value]) -> String {
    let mut hasher = Sha256::new();
    for message in messages {
        for part in [&message["role"], &message["content"]] {
            let text = part
                .as_str()
                .map_or_else(|| part.to_string(), str::to_owned);
            hasher.update((text.len() as u64).to_le_bytes());
            hasher.update(text.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Hashes of the prompts a validated request shows the model: generation cases whole, direction
/// conversations without the assistant answer they end in.
fn prompts(op: Operation, request: &Value) -> Vec<String> {
    let messages = |item: &Value| item["messages"].as_array().cloned().unwrap_or_default();
    match op {
        Operation::Generate => request["cases"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|case| prompt_hash(&messages(case)))
            .collect(),
        Operation::Direction => ["positive", "negative"]
            .into_iter()
            .flat_map(|key| request[key].as_array().cloned().unwrap_or_default())
            .map(|item| {
                let all = messages(&item);
                prompt_hash(all.split_last().map_or(&[][..], |(_, prompt)| prompt))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn summary(ledger: &Ledger) -> Value {
    let prompts: BTreeSet<&String> = ledger.runs.iter().flat_map(|run| &run.prompts).collect();
    json!({
        "op": "experiment",
        "experiment": ledger.name,
        "budget": ledger.budget,
        "used": ledger.used,
        "runs": ledger.runs.len(),
        "prompts": prompts.len(),
        "held_out": ledger.held_out.len(),
        "created_at": ledger.created_at,
    })
}

/// Start an experiment with a budget of `budget` model runs.
pub fn create_experiment(config: &Config, name: &str, budget: u64) -> Result<Value> {
    let name = valid_name(name, "experiment")?;
    ensure!(
        (1..=MAX_BUDGET).contains(&budget),
        "budget must be 1–{MAX_BUDGET} model runs"
    );
    let _lock = lock(config)?;
    let path = experiment_path(config, &name);
    ensure!(
        load::<Ledger>(&path)?.is_none(),
        "experiment {name} already exists"
    );
    let ledger = Ledger {
        name,
        budget,
        used: 0,
        created_at: now(),
        runs: Vec::new(),
        held_out: BTreeSet::new(),
    };
    save(&path, &ledger)?;
    Ok(summary(&ledger))
}

/// An experiment's budget, spending and prompt counts.
pub fn experiment(config: &Config, name: &str) -> Result<Value> {
    Ok(summary(&ledger(config, &valid_name(name, "experiment")?)?))
}

/// Turn validated `name`/`direction` fields into state-directory paths for the worker. The caller
/// holds the job lock.
pub(super) fn prepare(
    config: &Config,
    op: Operation,
    request: &mut Value,
    experiment: Option<&str>,
) -> Result<Option<Created>> {
    let mut take = |key: &str| {
        request
            .as_object_mut()
            .and_then(|fields| fields.remove(key))
            .and_then(|value| value.as_str().map(str::to_owned))
            .with_context(|| format!("{op:?} needs `{key}`"))
    };
    match op {
        Operation::Direction => {
            let name = take("name")?;
            let path = direction_path(config, &name);
            private_dir(path.parent().context("directions directory")?)?;
            ensure!(
                !path.exists() && !direction_record_path(config, &name).exists(),
                "a direction named {name} already exists; choose another name"
            );
            request["output"] = json!(path);
            Ok(Some(Created::Direction { name }))
        }
        Operation::Edit => {
            let name = take("name")?;
            let direction = take("direction")?;
            let measured: DirectionRecord = load(&direction_record_path(config, &direction))?
                .with_context(|| {
                    format!("no direction named {direction}; measure one with the direction operation first")
                })?;
            ensure!(
                Some(measured.experiment.as_str()) == experiment,
                "direction {direction} belongs to experiment {}; an edit must use a direction from its own experiment",
                measured.experiment
            );
            let source = direction_path(config, &direction);
            ensure!(source.is_file(), "direction {direction} has no saved file");
            let dir = edits_dir(config);
            private_dir(&dir)?;
            let output = dir.join(&name);
            ensure!(
                !output.exists() && !record_path(config, &name).exists(),
                "an edit named {name} already exists; choose another name"
            );
            request["direction"] = json!(source);
            request["output"] = json!(output);
            Ok(Some(Created::Edit {
                name,
                direction,
                output,
            }))
        }
        _ => Ok(None),
    }
}

/// Charge `op` against experiment `name` before the worker starts; the caller holds the job lock.
pub(super) fn charge(config: &Config, name: &str, op: Operation, request: &Value) -> Result<Value> {
    let path = experiment_path(config, name);
    let mut ledger = ledger(config, name)?;
    let cost = u64::from(op.loads_model());
    ensure!(
        ledger.used + cost <= ledger.budget,
        "experiment {name} has used its budget of {} model runs; evaluate what it produced or start a new experiment",
        ledger.budget
    );
    let prompts = prompts(op, request);
    if let Some(number) = prompts
        .iter()
        .position(|prompt| ledger.held_out.contains(prompt))
    {
        bail!(
            "conversation {number} is held out for evaluation in experiment {name}; experiment runs must not see evaluation cases"
        );
    }
    ledger.used += cost;
    ledger.runs.push(Run {
        op: op.name().into(),
        at: now(),
        cost,
        prompts,
    });
    save(&path, &ledger)?;
    Ok(json!({"name": name, "budget": ledger.budget, "used": ledger.used, "cost": cost}))
}

/// Record what a successful run created; the caller holds the job lock.
pub(super) fn record(
    config: &Config,
    created: Created,
    experiment: &str,
    base: &Path,
) -> Result<()> {
    match created {
        Created::Direction { name } => save(
            &direction_record_path(config, &name),
            &DirectionRecord {
                name,
                experiment: experiment.into(),
                created_at: now(),
            },
        ),
        Created::Edit {
            name,
            direction,
            output,
        } => save(
            &record_path(config, &name),
            &EditRecord {
                name: name.clone(),
                experiment: experiment.into(),
                direction,
                base_model: config.model.clone(),
                base: base.to_owned(),
                output,
                created_at: now(),
                evaluation: None,
            },
        ),
    }
}

/// The edited copies, newest first, with their evaluation state.
pub(super) fn summaries(config: &Config) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(edits_dir(config)) else {
        return Vec::new();
    };
    let mut records: Vec<EditRecord> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| load(&path).ok().flatten())
        .collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
    records
        .into_iter()
        .take(MAX_LISTED_EDITS)
        .map(|record| {
            json!({
                "name": record.name,
                "experiment": record.experiment,
                "direction": record.direction,
                "output": record.output,
                "passed": record.evaluation.as_ref().map_or(Value::Null, |e| e["passed"].clone()),
                "created_at": record.created_at,
            })
        })
        .collect()
}

/// Names, sizes and modification times of an edited copy's files: what promotion rechecks.
fn fingerprint(dir: &Path) -> Result<String> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| {
            let entry = entry?;
            let meta = entry.metadata()?;
            let modified = meta
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_or(0, |time| time.as_nanos());
            Ok((
                entry.file_name().to_string_lossy().into_owned(),
                meta.len(),
                modified,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    let mut hasher = Sha256::new();
    for (name, len, modified) in entries {
        hasher.update(format!("{name}\0{len}\0{modified}\n").as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_json_file(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .take(MAX_EDIT_RECORD + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_EDIT_RECORD,
        "{} is too large",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// A case is good when it stopped before the token limit and passed its checks, if it had any.
fn good(row: &Value) -> bool {
    row["hit_limit"] != true && row["passed"] != false
}

/// Per-case wins, losses and ties of `edited` over `base` (two generate results on the same
/// cases), and whether that admits the edit.
fn compare(base: &Value, edited: &Value) -> Result<Value> {
    let base_rows = base["cases"]
        .as_array()
        .context("base generation has no cases")?;
    let edited_rows = edited["cases"]
        .as_array()
        .context("edited generation has no cases")?;
    ensure!(
        base_rows.len() == edited_rows.len(),
        "base and edited generations covered different cases"
    );
    let side = |row: &Value| {
        json!({"tokens": row["tokens"], "hit_limit": row["hit_limit"], "passed": row["passed"],
               "repeat_ratio": row["repeat_ratio"]})
    };
    let (mut wins, mut losses) = (0, 0);
    let rows: Vec<Value> = base_rows
        .iter()
        .zip(edited_rows)
        .enumerate()
        .map(|(case, (before, after))| {
            let outcome = match (good(before), good(after)) {
                (false, true) => {
                    wins += 1;
                    "win"
                }
                (true, false) => {
                    losses += 1;
                    "loss"
                }
                _ => "tie",
            };
            json!({"case": case, "outcome": outcome, "base": side(before), "edited": side(after)})
        })
        .collect();
    let cases = rows.len();
    let p_value = sign_test_p(wins, losses);
    let reason = if cases < MIN_EVALUATION_CASES {
        Some(format!(
            "needs at least {MIN_EVALUATION_CASES} held-out cases"
        ))
    } else if wins <= losses {
        Some("the edit did not win more cases than it lost".to_owned())
    } else if p_value > MAX_P_VALUE {
        Some(format!(
            "the improvement is not significant (sign test p > {MAX_P_VALUE})"
        ))
    } else {
        None
    };
    Ok(json!({
        "cases": cases,
        "wins": wins,
        "losses": losses,
        "ties": cases - wins - losses,
        "p_value": p_value,
        "passed": reason.is_none(),
        "reason": reason,
        "base": base["summary"],
        "edited": edited["summary"],
        "max_tokens": edited["max_tokens"],
        "rows": rows,
    }))
}

/// Evaluate edit `input.edit` against its base on held-out `input.cases` (generation cases, with
/// optional `max_tokens`). Not charged to the experiment; the cases become held out for it.
pub fn evaluate(
    config: &Config,
    layout: Option<&Layout>,
    input: &Value,
    cancel: &AtomicBool,
) -> Result<Value> {
    let name = super::name(input.get("edit"), "edit")?;
    let mut request = arguments(Operation::Generate, input)?;
    if let Some(fields) = request.as_object_mut() {
        // Evaluation reports outcomes only, never answer text.
        fields.remove("tail");
    }
    let count = request["cases"].as_array().map_or(0, Vec::len);
    ensure!(
        count >= MIN_EVALUATION_CASES,
        "evaluation needs at least {MIN_EVALUATION_CASES} held-out cases; {count} given"
    );
    let hashes = prompts(Operation::Generate, &request);
    job(config, layout, cancel, || {
        let path = record_path(config, &name);
        let mut record: EditRecord =
            load(&path)?.with_context(|| format!("no edit named {name}"))?;
        let ledger_path = experiment_path(config, &record.experiment);
        let mut ledger = ledger(config, &record.experiment)?;
        let seen: BTreeSet<&String> = ledger.runs.iter().flat_map(|run| &run.prompts).collect();
        if let Some(number) = hashes.iter().position(|hash| seen.contains(hash)) {
            bail!(
                "case {number} was used during experiment {}; evaluation needs held-out cases the experiment never saw",
                record.experiment
            );
        }
        ensure!(
            record.output.is_dir(),
            "edited copy {} is missing",
            record.output.display()
        );
        ensure!(
            record.base.is_dir(),
            "base model {} is missing",
            record.base.display()
        );
        let made = read_json_file(&record.output.join(EDIT_RECORD))?;
        // Hold the cases out before any generation, so even a failed evaluation protects them.
        ledger.held_out.extend(hashes.iter().cloned());
        save(&ledger_path, &ledger)?;
        let base = call_worker(
            config,
            &record.base,
            Operation::Generate,
            request.clone(),
            None,
            cancel,
        )?;
        ensure!(
            base["model_id"] == made["base_id"],
            "the base model changed since edit {name} was made; make the edit again"
        );
        let edited = call_worker(
            config,
            &record.output,
            Operation::Generate,
            request,
            None,
            cancel,
        )?;
        let mut value = compare(&base, &edited)?;
        let fingerprint = fingerprint(&record.output)?;
        record.evaluation = Some(json!({
            "passed": value["passed"],
            "wins": value["wins"],
            "losses": value["losses"],
            "ties": value["ties"],
            "cases": value["cases"],
            "p_value": value["p_value"],
            "fingerprint": fingerprint,
            "at": now(),
        }));
        save(&path, &record)?;
        value["op"] = json!("evaluate");
        value["edit"] = json!(name);
        value["experiment"] = json!(record.experiment);
        value["model"] = json!(record.output);
        value["model_id"] = edited["model_id"].clone();
        value["base_model"] = json!(record.base);
        value["created_at"] = json!(now());
        write_private(&evaluation_path(config), value.to_string().as_bytes())?;
        Ok(value)
    })
}

fn switch(config: &Config, model: &str, message: String) -> Result<ModelSwitch> {
    let changes =
        crate::config::selection::changes(config, &ModelSelection::new("mlx", model), "main")?;
    Ok(ModelSwitch {
        changes,
        model: PathBuf::from(model),
        message,
    })
}

/// The settings writes that make edit `name` the project's model. Requires its own passing
/// evaluation and files unchanged since then.
pub fn promotion(config: &Config, name: &str) -> Result<ModelSwitch> {
    let name = valid_name(name, "edit")?;
    let record: EditRecord =
        load(&record_path(config, &name))?.with_context(|| format!("no edit named {name}"))?;
    let evaluation = record.evaluation.as_ref().with_context(|| {
        format!("edit {name} has not been evaluated; run `dial weights evaluate {name} --cases FILE` first")
    })?;
    ensure!(
        evaluation["passed"] == true,
        "edit {name} did not pass its evaluation, so it cannot become the model"
    );
    ensure!(
        evaluation["fingerprint"].as_str() == Some(fingerprint(&record.output)?.as_str()),
        "edit {name} changed after its evaluation; evaluate it again"
    );
    let model = record
        .output
        .to_str()
        .context("edited copy path is not UTF-8")?;
    switch(
        config,
        model,
        format!(
            "Promoted edit {name} to this project's model ({model}); `dial weights revert` restores {}.",
            record.base_model
        ),
    )
}

/// The settings writes that restore the base of the configured model, which must be a Dial edit.
pub fn reversion(config: &Config) -> Result<ModelSwitch> {
    let current = super::target(config)?;
    let made = read_json_file(&current.join(EDIT_RECORD))
        .context("the configured model is not a Dial weight edit; nothing to revert")?;
    let current = std::fs::canonicalize(&current)?;
    let recorded = std::fs::read_dir(edits_dir(config))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| load::<EditRecord>(&entry.path()).ok().flatten())
        .find(|record| std::fs::canonicalize(&record.output).is_ok_and(|path| path == current));
    // The settings name the base as the user chose it; the copy's own record has its directory.
    let base = match recorded {
        Some(record) => record.base_model,
        None => made["base"]
            .as_str()
            .context("the edit record names no base model")?
            .to_owned(),
    };
    switch(
        config,
        &base,
        format!("Reverted this project's model to {base}."),
    )
}

#[cfg(test)]
mod tests;
