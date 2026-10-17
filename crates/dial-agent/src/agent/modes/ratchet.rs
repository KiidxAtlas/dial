//! `/ratchet`: an eval gate and hill-climb runner.
//!
//! The host owns the truth. Approvals are content-hashed, the train/test split is frozen,
//! results must be complete, held-out transcripts must not leak, and keep/revert is decided here.
//! Flow state lives under `<workspace>/.dial/ratchet/<flow>/`: `_state.json`, `status.md`, and one
//! directory per variant holding `results.jsonl`, `errors.jsonl`, `traces/`, and `before.json`
//! (the pre-candidate snapshot used to revert).
//!
//! The gate rules (paired t-interval verdicts, guardrails, rerun) are not in `learn::gate` or
//! `crate::tools::knowledge::autoresearch::experiment`, so they are implemented here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::Command as Process;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::hex;

pub const ROOT: &str = ".dial/ratchet";
const STATE_FILE: &str = "_state.json";
const BEFORE_FILE: &str = "before.json";
const COST: &str = "cost_usd";
const LATENCY: &str = "latency_s";

/// Two-sided t critical values for df 1..=30; larger df uses the normal 1.96.
const T_CRITICAL: [f64; 30] = [
    12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.16,
    2.145, 2.131, 2.12, 2.11, 2.101, 2.093, 2.086, 2.08, 2.074, 2.069, 2.064, 2.06, 2.056, 2.052,
    2.048, 2.045, 2.042,
];

pub fn t_critical(df: usize) -> f64 {
    if (1..=30).contains(&df) {
        T_CRITICAL[df - 1]
    } else {
        1.96
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Higher,
    Lower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Up,
    Down,
    Flat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Baseline,
    Keep,
    Revert,
    Rerun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Inputs,
    Grader,
    Plan,
}

impl Stage {
    pub const ALL: [Stage; 3] = [Stage::Inputs, Stage::Grader, Stage::Plan];

    pub fn name(self) -> &'static str {
        match self {
            Stage::Inputs => "inputs",
            Stage::Grader => "grader",
            Stage::Plan => "plan",
        }
    }

    pub fn parse(value: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|stage| stage.name() == value)
    }
}

fn default_direction(metric: &str) -> Direction {
    if metric == COST || metric == LATENCY {
        Direction::Lower
    } else {
        Direction::Higher
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub target: String,
    pub direction: Direction,
    #[serde(default)]
    pub hold: Vec<String>,
    #[serde(default)]
    pub directions: BTreeMap<String, Direction>,
}

impl Goal {
    fn metric_direction(&self, metric: &str) -> Direction {
        if metric == self.target {
            return self.direction;
        }
        self.directions
            .get(metric)
            .copied()
            .unwrap_or_else(|| default_direction(metric))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    pub plateau: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub n: usize,
    pub mean: Option<f64>,
    pub half: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Round {
    pub round: u32,
    pub variant: String,
    pub change: String,
    pub decision: Decision,
    pub reasons: Vec<String>,
    pub train: BTreeMap<String, Summary>,
    pub test: BTreeMap<String, Summary>,
    pub errors: usize,
    pub truncated: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Best {
    pub round: u32,
    pub variant: String,
    pub test_score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub flow: String,
    pub cases_paths: Vec<String>,
    pub harness_paths: Vec<String>,
    pub change_paths: Vec<String>,
    #[serde(default)]
    pub off_limits: Vec<String>,
    #[serde(default)]
    pub goal: Option<Goal>,
    #[serde(default)]
    pub reps: Option<u32>,
    pub stop: Stop,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub train_ids: Vec<String>,
    #[serde(default)]
    pub test_ids: Vec<String>,
    #[serde(default)]
    pub split_seed: Option<u32>,
    #[serde(default)]
    pub approvals: BTreeMap<String, String>,
    #[serde(default)]
    pub rounds: Vec<Round>,
    #[serde(default)]
    pub best: Option<Best>,
    #[serde(default)]
    pub flat_rounds: u32,
    #[serde(default)]
    pub baseline_inputs_sha: Option<String>,
}

impl State {
    fn new(flow: &str) -> State {
        State {
            flow: flow.to_string(),
            cases_paths: Vec::new(),
            harness_paths: Vec::new(),
            change_paths: Vec::new(),
            off_limits: Vec::new(),
            goal: None,
            reps: None,
            stop: Stop {
                plateau: 3,
                rounds: None,
            },
            command: None,
            train_ids: Vec::new(),
            test_ids: Vec::new(),
            split_seed: None,
            approvals: BTreeMap::new(),
            rounds: Vec::new(),
            best: None,
            flat_rounds: 0,
            baseline_inputs_sha: None,
        }
    }
}

// ---------- paths and persistence ----------

fn valid_flow(flow: &str) -> bool {
    let bytes = flow.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

pub fn flow_dir(root: &Path, flow: &str) -> Result<PathBuf> {
    ensure!(
        valid_flow(flow),
        "Flow name must match [a-z0-9][a-z0-9_-]{{0,63}}, got {flow:?}"
    );
    Ok(root.join(ROOT).join(flow))
}

fn repo_path(value: &str) -> Result<String> {
    let mut parts = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => bail!("Path must stay inside the workspace: {value}"),
        }
    }
    ensure!(
        !parts.is_empty(),
        "Path must name a file or directory inside the workspace: {value:?}"
    );
    Ok(parts.join("/"))
}

fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn rel_posix(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn list_files(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut files = Vec::new();
    for entry in entries {
        files.extend(list_files(&entry.path())?);
    }
    Ok(files)
}

fn hash_paths(root: &Path, paths: &[String]) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut sorted = paths.to_vec();
    sorted.sort();
    for rel in &sorted {
        let absolute = root.join(rel);
        if !absolute.exists() {
            hasher.update(format!("missing:{rel}\0").as_bytes());
            continue;
        }
        for file in list_files(&absolute)? {
            hasher.update(rel_posix(root, &file).as_bytes());
            hasher.update(b"\0");
            hasher.update(fs::read(&file)?);
            hasher.update(b"\0");
        }
    }
    Ok(hex(&hasher.finalize()))
}

pub fn stage_digest(root: &Path, state: &State, stage: Stage) -> Result<String> {
    match stage {
        Stage::Inputs => hash_paths(root, &state.cases_paths),
        Stage::Grader => hash_paths(root, &state.harness_paths),
        Stage::Plan => {
            #[derive(Serialize)]
            struct PlanDigest<'a> {
                goal: &'a Goal,
                reps: u32,
                stop: Stop,
                command: &'a str,
                change_paths: &'a [String],
                off_limits: &'a [String],
                train_ids: &'a [String],
                test_ids: &'a [String],
            }
            let (Some(goal), Some(reps), Some(command)) = (&state.goal, state.reps, &state.command)
            else {
                bail!("The plan is incomplete: call plan() with a goal, reps, and command first");
            };
            ensure!(
                !state.train_ids.is_empty() && !state.test_ids.is_empty(),
                "The plan has no train/test split: call split() first"
            );
            let json = serde_json::to_vec(&PlanDigest {
                goal,
                reps,
                stop: state.stop,
                command,
                change_paths: &state.change_paths,
                off_limits: &state.off_limits,
                train_ids: &state.train_ids,
                test_ids: &state.test_ids,
            })?;
            Ok(hex(&Sha256::digest(&json)))
        }
    }
}

/// Per stage: `missing` (never approved), `stale` (content changed since approval), or `current`.
pub fn approval_status(root: &Path, state: &State) -> Vec<(Stage, &'static str)> {
    Stage::ALL
        .into_iter()
        .map(|stage| {
            let status = match state.approvals.get(stage.name()) {
                None => "missing",
                Some(recorded)
                    if stage_digest(root, state, stage).ok().as_deref()
                        == Some(recorded.as_str()) =>
                {
                    "current"
                }
                Some(_) => "stale",
            };
            (stage, status)
        })
        .collect()
}

/// Records the current digest of `stage` as the user's explicit approval.
pub fn approve(root: &Path, state: &mut State, stage: Stage) -> Result<String> {
    let digest = stage_digest(root, state, stage)?;
    state
        .approvals
        .insert(stage.name().to_string(), digest.clone());
    Ok(digest)
}

fn require_approvals(root: &Path, state: &State) -> Result<()> {
    let blocked: Vec<String> = approval_status(root, state)
        .into_iter()
        .filter(|(_, status)| *status != "current")
        .map(|(stage, status)| format!("{} ({status})", stage.name()))
        .collect();
    ensure!(
        blocked.is_empty(),
        "Approval required before gating: {}. Review the stage and run `/ratchet <flow> approve <stage>`",
        blocked.join(", ")
    );
    Ok(())
}

pub fn load(root: &Path, flow: &str) -> Result<Option<State>> {
    let path = flow_dir(root, flow)?.join(STATE_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn require(root: &Path, flow: &str) -> Result<State> {
    load(root, flow)?
        .with_context(|| format!("Flow {flow} does not exist; run `/ratchet {flow} init` first"))
}

pub fn save(root: &Path, state: &State) -> Result<()> {
    let dir = flow_dir(root, &state.flow)?;
    fs::create_dir_all(&dir)?;
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    write_atomic(&dir.join(STATE_FILE), &bytes)
}

/// Writes via a synced, uniquely named temp file and rename, so a crash never leaves a truncated
/// or torn state file and concurrent writers never share a temp path.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::config::settings_store::write_private(path, bytes)
}

// ---------- setup: init, plan, split ----------

#[derive(Debug, Default)]
pub struct InitOptions {
    pub cases: Vec<String>,
    pub harness: Vec<String>,
    pub change: Vec<String>,
    pub off_limits: Vec<String>,
}

pub fn init(root: &Path, flow: &str, options: InitOptions) -> Result<State> {
    flow_dir(root, flow)?;
    let cases = options
        .cases
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>>>()?;
    let harness = options
        .harness
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>>>()?;
    let change = options
        .change
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>>>()?;
    let off_limits = options
        .off_limits
        .iter()
        .map(|p| repo_path(p))
        .collect::<Result<Vec<_>>>()?;
    ensure!(!cases.is_empty(), "init needs at least one cases path");
    ensure!(
        !harness.is_empty(),
        "init needs at least one harness path (runner and grader)"
    );
    ensure!(
        !change.is_empty(),
        "init needs at least one change path (the surface being tuned)"
    );
    for rel in cases.iter().chain(&harness) {
        ensure!(root.join(rel).exists(), "Path does not exist: {rel}");
    }
    let flow_rel = format!("{ROOT}/{flow}");
    for target in &change {
        let frozen = cases
            .iter()
            .chain(&harness)
            .chain(&off_limits)
            .chain(std::iter::once(&flow_rel))
            .find(|other| overlaps(target.as_str(), other.as_str()));
        if let Some(frozen) = frozen {
            bail!(
                "Change path {target} overlaps frozen path {frozen}; the tuned surface must not include the cases, the harness, off-limits paths, or the flow directory"
            );
        }
    }
    let mut state = load(root, flow)?.unwrap_or_else(|| State::new(flow));
    ensure!(
        state.rounds.is_empty(),
        "init cannot change paths after rounds exist; start a new flow name instead"
    );
    state.cases_paths = cases;
    state.harness_paths = harness;
    state.change_paths = change;
    state.off_limits = off_limits;
    save(root, &state)?;
    Ok(state)
}

#[derive(Debug, Default)]
pub struct GoalOptions {
    pub target: String,
    pub direction: Option<Direction>,
    pub hold: Vec<String>,
    pub directions: BTreeMap<String, Direction>,
}

#[derive(Debug, Default)]
pub struct PlanOptions {
    pub goal: Option<GoalOptions>,
    pub reps: Option<u32>,
    pub plateau: Option<u32>,
    pub rounds: Option<u32>,
    pub command: Option<String>,
}

pub fn plan(state: &mut State, options: PlanOptions) -> Result<()> {
    let baseline_taken = !state.rounds.is_empty();
    if let Some(goal) = options.goal {
        ensure!(!goal.target.is_empty(), "goal.target is required");
        ensure!(
            !goal.hold.contains(&goal.target),
            "goal.hold must not include the target {}",
            goal.target
        );
        let direction = goal
            .direction
            .unwrap_or_else(|| default_direction(&goal.target));
        state.goal = Some(Goal {
            target: goal.target,
            direction,
            hold: goal.hold,
            directions: goal.directions,
        });
    }
    if let Some(reps) = options.reps {
        ensure!(reps >= 1, "reps must be an integer >= 1");
        if baseline_taken && let Some(previous) = state.reps {
            ensure!(
                reps >= previous,
                "reps cannot shrink after the baseline (was {previous})"
            );
        }
        state.reps = Some(reps);
    }
    if options.plateau.is_some() || options.rounds.is_some() {
        let plateau = options.plateau.unwrap_or(state.stop.plateau);
        ensure!(plateau >= 2, "stop.plateau must be an integer >= 2");
        let rounds = options.rounds.or(state.stop.rounds);
        ensure!(
            rounds.is_none_or(|limit| limit >= 1),
            "stop.rounds must be an integer >= 1"
        );
        state.stop = Stop { plateau, rounds };
    }
    if let Some(command) = options.command {
        ensure!(!command.trim().is_empty(), "command must not be empty");
        state.command = Some(command);
    }
    Ok(())
}

/// Seeded PRNG (mulberry32); a given seed always yields the same split.
fn mulberry32(seed: u32) -> impl FnMut() -> f64 {
    let mut value = seed;
    move || {
        value = value.wrapping_add(0x6d2b_79f5);
        let mut t = value;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// Stratified train/test split: cases are grouped by tag, shuffled per group, and the first
/// `round(len * test_fraction)` of each group go to test.
pub fn split(
    state: &mut State,
    tags: &BTreeMap<String, String>,
    test_fraction: f64,
    seed: Option<u32>,
) -> Result<()> {
    ensure!(
        test_fraction > 0.0 && test_fraction < 1.0,
        "test_fraction must be between 0 and 1 exclusive"
    );
    ensure!(
        tags.len() >= 4,
        "split needs at least 4 cases, got {}",
        tags.len()
    );
    ensure!(
        state.rounds.is_empty(),
        "The split is frozen once rounds exist; re-splitting would invalidate every gated round"
    );
    let seed = seed.unwrap_or_else(rand::random::<u32>);
    let mut random = mulberry32(seed);

    let mut buckets: Vec<(&str, Vec<String>)> = Vec::new();
    for (id, tag) in tags {
        let index = match buckets.iter().position(|(known, _)| *known == tag.as_str()) {
            Some(index) => index,
            None => {
                buckets.push((tag.as_str(), Vec::new()));
                buckets.len() - 1
            }
        };
        buckets[index].1.push(id.clone());
    }

    let mut train = Vec::new();
    let mut test = Vec::new();
    for (_, mut ids) in buckets {
        for index in (1..ids.len()).rev() {
            let swap = (random() * (index as f64 + 1.0)).floor() as usize;
            ids.swap(index, swap);
        }
        let test_count = (ids.len() as f64 * test_fraction + 0.5).floor() as usize;
        test.extend(ids[..test_count].iter().cloned());
        train.extend(ids[test_count..].iter().cloned());
    }
    if test.is_empty()
        && let Some(id) = train.pop()
    {
        test.push(id);
    }
    if train.is_empty()
        && let Some(id) = test.pop()
    {
        train.push(id);
    }
    train.sort();
    test.sort();
    state.train_ids = train;
    state.test_ids = test;
    state.split_seed = Some(seed);
    Ok(())
}

// ---------- results and metrics ----------

#[derive(Debug)]
struct Variant {
    rows: Vec<Value>,
    errors: Vec<Value>,
    traces: Vec<String>,
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut rows = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let location = format!("{}:{}", path.display(), number + 1);
        let mut row: Value =
            serde_json::from_str(line).with_context(|| format!("{location} is not JSON"))?;
        let object = row
            .as_object_mut()
            .with_context(|| format!("{location} is not a JSON object"))?;
        ensure!(
            object.get("prompt_id").and_then(Value::as_str).is_some(),
            "{location} has no prompt_id"
        );
        let rep = match object.get("rep") {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| anyhow!("{location} rep must be a non-negative integer"))?,
        };
        object.insert("rep".into(), Value::from(rep));
        rows.push(row);
    }
    Ok(rows)
}

fn read_variant(dir: &Path, variant: &str) -> Result<Variant> {
    let variant_dir = dir.join(variant);
    let results = variant_dir.join("results.jsonl");
    ensure!(
        results.exists(),
        "No results for {variant}: {} is missing",
        results.display()
    );
    let rows = read_jsonl(&results)?;
    let errors = read_jsonl(&variant_dir.join("errors.jsonl"))?;
    let traces = match fs::read_dir(variant_dir.join("traces")) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<Vec<_>>>()?,
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    Ok(Variant {
        rows,
        errors,
        traces,
    })
}

fn prompt_id(row: &Value) -> &str {
    row["prompt_id"].as_str().unwrap_or("")
}

fn slot(row: &Value) -> String {
    format!("{}#{}", prompt_id(row), row["rep"].as_u64().unwrap_or(0))
}

fn row_cost(row: &Value) -> Option<f64> {
    row[COST].as_f64()
}

fn is_truncated(row: &Value) -> bool {
    row["status"].as_str() == Some("truncated")
}

/// Metrics of one row: numeric fields, the grade (bool, number, or dict), and cost. Truncated
/// rows keep their numeric fields but lose their grade.
fn row_metrics(row: &Value) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    if let Some(object) = row.as_object() {
        for (key, value) in object {
            if key == "rep" || key == COST {
                continue;
            }
            if let Some(number) = value.as_f64() {
                metrics.insert(key.clone(), number);
            }
        }
    }
    if !is_truncated(row) {
        match &row["grade"] {
            Value::Bool(flag) => {
                metrics.insert("score".into(), f64::from(u8::from(*flag)));
            }
            Value::Number(_) => {
                if let Some(number) = row["grade"].as_f64() {
                    metrics.insert("score".into(), number);
                }
            }
            Value::Object(map) => {
                for (key, value) in map {
                    match value {
                        Value::Bool(flag) => {
                            metrics.insert(key.clone(), f64::from(u8::from(*flag)));
                        }
                        _ => {
                            if let Some(number) = value.as_f64() {
                                metrics.insert(key.clone(), number);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(cost) = row_cost(row) {
        metrics.insert(COST.into(), cost);
    }
    metrics
}

type CaseMeans = BTreeMap<String, BTreeMap<String, f64>>;

/// Per case, the mean of each metric over its reps.
fn case_means<'a>(rows: impl IntoIterator<Item = &'a Value>) -> CaseMeans {
    let mut sums: BTreeMap<String, BTreeMap<String, (f64, usize)>> = BTreeMap::new();
    for row in rows {
        let per_case = sums.entry(prompt_id(row).to_string()).or_default();
        for (metric, value) in row_metrics(row) {
            let entry = per_case.entry(metric).or_insert((0.0, 0));
            entry.0 += value;
            entry.1 += 1;
        }
    }
    sums.into_iter()
        .map(|(id, metrics)| {
            let means = metrics
                .into_iter()
                .map(|(metric, (sum, count))| (metric, sum / count as f64))
                .collect();
            (id, means)
        })
        .collect()
}

fn mean_and_half(values: &[f64]) -> Summary {
    let n = values.len();
    if n == 0 {
        return Summary::default();
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    if n < 2 {
        return Summary {
            n,
            mean: Some(mean),
            half: None,
        };
    }
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    Summary {
        n,
        mean: Some(mean),
        half: Some(t_critical(n - 1) * (variance / n as f64).sqrt()),
    }
}

fn summarize(means: &CaseMeans, ids: &[String], metric: &str) -> Summary {
    let values: Vec<f64> = ids
        .iter()
        .filter_map(|id| {
            means
                .get(id)
                .and_then(|metrics| metrics.get(metric))
                .copied()
        })
        .collect();
    mean_and_half(&values)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Paired {
    n: usize,
    verdict: Verdict,
}

/// Paired per-case differences (candidate minus reference) and their verdict against the
/// t-interval half-width. A verdict needs the whole interval to clear zero.
fn paired(
    candidate: &CaseMeans,
    reference: &CaseMeans,
    ids: &[String],
    metric: &str,
    direction: Direction,
) -> Paired {
    let diffs: Vec<f64> = ids
        .iter()
        .filter_map(|id| {
            let a = candidate.get(id)?.get(metric)?;
            let b = reference.get(id)?.get(metric)?;
            Some(a - b)
        })
        .collect();
    let summary = mean_and_half(&diffs);
    let verdict = match (summary.mean, summary.half) {
        (Some(mean), Some(half)) => {
            let gain = if direction == Direction::Higher {
                mean
            } else {
                -mean
            };
            if gain - half > 0.0 {
                Verdict::Up
            } else if gain + half < 0.0 {
                Verdict::Down
            } else {
                Verdict::Flat
            }
        }
        _ => Verdict::Flat,
    };
    Paired {
        n: summary.n,
        verdict,
    }
}

// ---------- variants, checks, and the gate ----------

fn variant_name_ok(variant: &str) -> bool {
    if variant == "baseline" {
        return true;
    }
    variant.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && !digits.starts_with('0')
    })
}

/// `baseline` before any round, else `v<last round + 1>`.
pub fn expected_variant(state: &State) -> String {
    match state.rounds.last() {
        None => "baseline".into(),
        Some(last) => format!("v{}", last.round + 1),
    }
}

/// True once the plateau counter or the round budget (`stop.rounds`) is exhausted.
pub fn finished(state: &State) -> bool {
    let climbed = state
        .rounds
        .iter()
        .filter(|round| round.variant != "baseline")
        .count() as u32;
    state.flat_rounds >= state.stop.plateau
        || state.stop.rounds.is_some_and(|limit| climbed >= limit)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub variant: String,
    pub command: String,
    pub cases: usize,
    pub reps: u32,
    pub expected_rows: usize,
    pub out_dir: String,
}

/// Validates that `variant` may run now and returns the resolved command to execute.
pub fn check_variant(root: &Path, state: &State, variant: &str) -> Result<Check> {
    ensure!(
        variant_name_ok(variant),
        "Variant must be \"baseline\" or v<N>, got {variant:?}"
    );
    require_approvals(root, state)?;
    if let Some(recorded) = &state.baseline_inputs_sha {
        ensure!(
            stage_digest(root, state, Stage::Inputs)? == *recorded,
            "The cases changed after the baseline was gated; the frozen baseline no longer covers them. Start a new flow with init()"
        );
    }
    let rerun = state
        .rounds
        .last()
        .is_some_and(|round| round.variant == variant && round.decision == Decision::Rerun);
    let expected = expected_variant(state);
    ensure!(
        rerun || variant == expected,
        "Next variant is {expected}, got {variant}"
    );
    let (Some(command), Some(reps)) = (&state.command, state.reps) else {
        bail!("The plan is incomplete: call plan() with a command and reps first");
    };
    ensure!(
        state.goal.is_some(),
        "No goal set: call plan() with a target first"
    );
    let cases = state.train_ids.len() + state.test_ids.len();
    let flow_rel = format!("{ROOT}/{}", state.flow);
    Ok(Check {
        variant: variant.to_string(),
        command: command
            .replace("{variant}", variant)
            .replace("{flow_dir}", &flow_rel),
        cases,
        reps,
        expected_rows: cases * (reps as usize),
        out_dir: format!("{flow_rel}/{variant}"),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub variant: String,
    pub decision: Decision,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub done: bool,
    pub table: String,
}

fn preview(items: &[String]) -> String {
    let shown: Vec<&str> = items.iter().take(10).map(String::as_str).collect();
    if items.len() > 10 {
        format!("{}, … (+{} more)", shown.join(", "), items.len() - 10)
    } else {
        shown.join(", ")
    }
}

/// `<id>` from a trace file named `<id>_rep<k>.json`.
fn trace_owner(file: &str) -> Option<&str> {
    let stem = file.strip_suffix(".json")?;
    let index = stem.rfind("_rep")?;
    let digits = &stem[index + 4..];
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| &stem[..index])
}

fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Baseline => "baseline",
        Decision::Keep => "keep",
        Decision::Revert => "revert",
        Decision::Rerun => "rerun",
    }
}

/// Scores one variant from its results, applies the decision rules, and records the round.
/// The round is recorded in state and `status.md`; keep/revert of the surface is the caller's job.
pub fn gate_variant(root: &Path, state: &mut State, variant: &str, change: &str) -> Result<Gate> {
    check_variant(root, state, variant)?;
    let goal = state
        .goal
        .clone()
        .context("No goal set: call plan() first")?;
    let reps = state.reps.context("reps is not set: call plan() first")? as usize;
    let dir = flow_dir(root, &state.flow)?;
    let data = read_variant(&dir, variant)?;
    let mut warnings = Vec::new();

    let split_ids: Vec<String> = state
        .train_ids
        .iter()
        .chain(&state.test_ids)
        .cloned()
        .collect();
    let known: BTreeSet<&str> = split_ids.iter().map(String::as_str).collect();

    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for row in &data.rows {
        let key = slot(row);
        if !seen.insert(key.clone()) {
            duplicates.insert(key);
        }
    }
    let duplicates: Vec<String> = duplicates.into_iter().collect();
    ensure!(
        duplicates.is_empty(),
        "{variant}/results.jsonl has duplicate (case, rep) rows: {}. Keep one row per slot and gate again",
        preview(&duplicates)
    );

    let error_slots: BTreeSet<String> = data.errors.iter().map(slot).collect();
    let mut missing = Vec::new();
    let mut errored = Vec::new();
    for id in &split_ids {
        for rep in 0..reps {
            let key = format!("{id}#{rep}");
            if seen.contains(&key) {
                continue;
            }
            if error_slots.contains(&key) {
                errored.push(key);
            } else {
                missing.push(key);
            }
        }
    }
    ensure!(
        missing.is_empty(),
        "{variant} is incomplete: {} (case, rep) slots have no result or error row ({}). Resume the runner until every slot is scored",
        missing.len(),
        preview(&missing)
    );
    ensure!(
        !(variant == "baseline" && !errored.is_empty()),
        "baseline has {} slots that only errored ({}). Fix the harness and rerun the baseline until every slot is scored",
        errored.len(),
        preview(&errored)
    );

    let test_set: BTreeSet<&str> = state.test_ids.iter().map(String::as_str).collect();
    let leaked: Vec<String> = data
        .traces
        .iter()
        .filter(|file| trace_owner(file).is_some_and(|id| test_set.contains(id)))
        .cloned()
        .collect();
    ensure!(
        leaked.is_empty(),
        "{variant}/traces holds transcripts for held-out test cases ({}). The runner must write traces for train cases only; remove them and gate again",
        preview(&leaked)
    );

    let strays = data
        .rows
        .iter()
        .filter(|row| !known.contains(prompt_id(row)))
        .count();
    if strays > 0 {
        warnings.push(format!(
            "{strays} rows fall outside the split and were ignored"
        ));
    }
    if !data.errors.is_empty() {
        warnings.push(format!(
            "{} error rows count as missing data, not scores",
            data.errors.len()
        ));
    }
    let scored: Vec<&Value> = data
        .rows
        .iter()
        .filter(|row| known.contains(prompt_id(row)))
        .collect();
    let truncated_count = scored.iter().filter(|row| is_truncated(row)).count();
    if truncated_count > 0 {
        warnings.push(format!(
            "{truncated_count} truncated rows excluded from means"
        ));
    }
    let measured: Vec<&Value> = scored
        .iter()
        .copied()
        .filter(|row| !is_truncated(row))
        .collect();
    if !scored.iter().any(|row| row_cost(row).is_some()) {
        warnings.push(
            "no row carries cost_usd; cost is unmeasured, so a cost guardrail cannot hold until rows report it".into(),
        );
    }

    let target = goal.target.as_str();
    let mut metrics: BTreeSet<String> = BTreeSet::new();
    metrics.insert(goal.target.clone());
    metrics.extend(goal.hold.iter().cloned());
    metrics.insert(COST.into());
    let means = case_means(measured.iter().copied());
    let mut train = BTreeMap::new();
    let mut test = BTreeMap::new();
    for metric in &metrics {
        train.insert(metric.clone(), summarize(&means, &state.train_ids, metric));
        test.insert(metric.clone(), summarize(&means, &state.test_ids, metric));
    }
    ensure!(
        train[target].n > 0 || test[target].n > 0,
        "No row carries the target metric {target:?}. Targets are grade-dict keys, \"score\" for a bool or number grade, \"cost_usd\", or a numeric row field"
    );
    let unmeasured: Vec<String> = goal
        .hold
        .iter()
        .filter(|metric| train[metric.as_str()].n < 2 || test[metric.as_str()].n < 2)
        .cloned()
        .collect();
    ensure!(
        unmeasured.is_empty(),
        "Guardrail {} has fewer than 2 measured cases on train or test in {variant}; the gate cannot hold a guardrail it cannot measure",
        preview(&unmeasured)
    );

    let last = state.rounds.last().cloned();
    let rerun = last
        .as_ref()
        .is_some_and(|round| round.variant == variant && round.decision == Decision::Rerun);
    let round_number = match (&last, rerun) {
        (Some(round), true) => round.round,
        (Some(round), false) => round.round + 1,
        (None, _) => 0,
    };

    let mut reasons = Vec::new();
    let decision = if variant == "baseline" {
        if let Some(half) = test[target].half {
            reasons.push(format!("test noise floor on {target}: ±{half:.3}"));
        }
        let overall = summarize(&means, &split_ids, target);
        let unit = measured.iter().all(|row| {
            row_metrics(row)
                .get(target)
                .is_none_or(|v| (0.0..=1.0).contains(v))
        });
        if let Some(mean) = overall.mean
            && goal.direction == Direction::Higher
            && unit
            && mean >= 0.95
        {
            warnings.push(format!(
                "baseline {target} is {mean:.3}: near the ceiling, so the climb has little room; add harder cases or aim at cost or latency"
            ));
        }
        if let (Some(tm), Some(em), Some(th), Some(eh)) = (
            train[target].mean,
            test[target].mean,
            train[target].half,
            test[target].half,
        ) && (tm - em).abs() > th.hypot(eh)
        {
            warnings.push(format!(
                "train and test {target} means differ beyond noise; the split may be unrepresentative"
            ));
        }
        Decision::Baseline
    } else if !errored.is_empty() {
        reasons.push(format!(
            "{} slots only errored ({}); resume {variant} until every slot is scored, then gate it again",
            errored.len(),
            preview(&errored)
        ));
        Decision::Rerun
    } else {
        let reference = state
            .best
            .as_ref()
            .map(|best| best.variant.clone())
            .context("No best round to compare against; gate the baseline first")?;
        let reference_data = read_variant(&dir, &reference)?;
        let reference_rows: Vec<&Value> = reference_data
            .rows
            .iter()
            .filter(|row| known.contains(prompt_id(row)) && !is_truncated(row))
            .collect();
        let reference_means = case_means(reference_rows.iter().copied());
        let pair = |ids: &[String], metric: &str| {
            paired(
                &means,
                &reference_means,
                ids,
                metric,
                goal.metric_direction(metric),
            )
        };

        let unpaired: Vec<String> = goal
            .hold
            .iter()
            .filter(|metric| {
                pair(&state.train_ids, metric).n < 2 || pair(&state.test_ids, metric).n < 2
            })
            .cloned()
            .collect();
        ensure!(
            unpaired.is_empty(),
            "Guardrail {} has fewer than 2 paired cases against {reference} on train or test; the gate cannot judge it",
            preview(&unpaired)
        );
        let regressed: Vec<String> = goal
            .hold
            .iter()
            .filter(|metric| {
                pair(&state.train_ids, metric).verdict == Verdict::Down
                    || pair(&state.test_ids, metric).verdict == Verdict::Down
            })
            .cloned()
            .collect();
        let target_train = pair(&state.train_ids, target).verdict;
        let target_test = pair(&state.test_ids, target).verdict;

        if !regressed.is_empty() {
            reasons.push(format!(
                "guardrail regressed vs {reference}: {}",
                regressed.join(", ")
            ));
            Decision::Revert
        } else if target_train == Verdict::Down || target_test == Verdict::Down {
            reasons.push(format!("{target} regressed vs {reference}"));
            Decision::Revert
        } else if target_train == Verdict::Up && target_test == Verdict::Up {
            reasons.push(format!(
                "{target} improved on train and test vs {reference}"
            ));
            Decision::Keep
        } else if target_train == Verdict::Up {
            reasons.push(format!(
                "overfit suspect: {target} improved on train but not test vs {reference}"
            ));
            Decision::Revert
        } else if target_test == Verdict::Up {
            reasons.push(format!(
                "{target} improved on test but not train vs {reference}; resume {variant} and gate again"
            ));
            Decision::Rerun
        } else {
            reasons.push(format!("no change outside noise vs {reference}"));
            Decision::Revert
        }
    };

    let test_score = test[target].mean;
    let round = Round {
        round: round_number,
        variant: variant.to_string(),
        change: change.to_string(),
        decision,
        reasons: reasons.clone(),
        train,
        test,
        errors: data.errors.len(),
        truncated: truncated_count,
    };
    if rerun && let Some(slot) = state.rounds.last_mut() {
        *slot = round;
    } else {
        state.rounds.push(round);
    }
    if decision == Decision::Baseline {
        state.baseline_inputs_sha = Some(stage_digest(root, state, Stage::Inputs)?);
    }
    match decision {
        Decision::Baseline | Decision::Keep => {
            state.best = Some(Best {
                round: round_number,
                variant: variant.to_string(),
                test_score,
            });
            state.flat_rounds = 0;
        }
        Decision::Revert => state.flat_rounds += 1,
        Decision::Rerun => {}
    }
    let done = finished(state);
    save(root, state)?;
    let table = render_status_table(state);
    fs::write(
        dir.join("status.md"),
        format!("# {}\n\n{table}\n", state.flow),
    )?;
    Ok(Gate {
        variant: variant.to_string(),
        decision,
        reasons,
        warnings,
        done,
        table,
    })
}

const NO_ROUNDS: &str = "(no rounds gated yet)";

fn cell(summary: Option<&Summary>, metric: &str) -> String {
    let digits: usize = if metric == COST { 4 } else { 3 };
    match summary.and_then(|s| s.mean.map(|mean| (mean, s.half))) {
        None => "–".into(),
        Some((mean, Some(half))) => format!("{mean:.digits$} ±{half:.digits$}"),
        Some((mean, None)) => format!("{mean:.digits$}"),
    }
}

pub fn render_status_table(state: &State) -> String {
    let Some(goal) = &state.goal else {
        return NO_ROUNDS.into();
    };
    if state.rounds.is_empty() {
        return NO_ROUNDS.into();
    }
    let mut metrics = vec![goal.target.clone()];
    metrics.extend(
        goal.hold
            .iter()
            .filter(|metric| metric.as_str() != COST)
            .cloned(),
    );
    metrics.push(COST.into());

    let mut header = vec![
        "round".to_string(),
        "variant".into(),
        "change".into(),
        "decision".into(),
    ];
    for metric in &metrics {
        header.push(format!("train {metric}"));
        header.push(format!("test {metric}"));
    }
    let mut lines = vec![
        format!("| {} |", header.join(" | ")),
        format!("|{}|", vec!["---"; header.len()].join("|")),
    ];
    for round in &state.rounds {
        let best = state
            .best
            .as_ref()
            .is_some_and(|best| best.variant == round.variant);
        let mut cells = vec![
            round.round.to_string(),
            format!("{}{}", round.variant, if best { " ★" } else { "" }),
            round.change.replace('|', "\\|"),
            decision_name(round.decision).into(),
        ];
        for metric in &metrics {
            cells.push(cell(round.train.get(metric), metric));
            cells.push(cell(round.test.get(metric), metric));
        }
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

// ---------- snapshots: the revert mechanism ----------

type Snapshot = BTreeMap<String, Vec<u8>>;

fn snapshot_paths(root: &Path, paths: &[String]) -> Result<Snapshot> {
    let mut snapshot = BTreeMap::new();
    for rel in paths {
        let absolute = root.join(rel);
        if !absolute.exists() {
            continue;
        }
        for file in list_files(&absolute)? {
            snapshot.insert(rel_posix(root, &file), fs::read(&file)?);
        }
    }
    Ok(snapshot)
}

fn restore_snapshot(root: &Path, paths: &[String], snapshot: &Snapshot) -> Result<()> {
    for path in snapshot_paths(root, paths)?.keys() {
        if !snapshot.contains_key(path) {
            fs::remove_file(root.join(path))?;
        }
    }
    for (path, bytes) in snapshot {
        let absolute = root.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&absolute, bytes)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn save_snapshot(root: &Path, state: &State, variant: &str) -> Result<()> {
    let dir = flow_dir(root, &state.flow)?.join(variant);
    fs::create_dir_all(&dir)?;
    let snapshot = snapshot_paths(root, &state.change_paths)?;
    write_atomic(&dir.join(BEFORE_FILE), &serde_json::to_vec(&snapshot)?)
}

/// Removes a variant's previous outputs so a new candidate is never scored on stale rows.
fn clear_outputs(variant_dir: &Path) -> Result<()> {
    remove_if_exists(&variant_dir.join("results.jsonl"))?;
    remove_if_exists(&variant_dir.join("errors.jsonl"))?;
    match fs::remove_dir_all(variant_dir.join("traces")) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Applies the decision to the surface: `Revert` restores the pre-candidate snapshot; both
/// `Keep` and `Revert` discard it. `Rerun` leaves the candidate in place with its snapshot.
fn settle(root: &Path, state: &State, variant: &str, decision: Decision) -> Result<()> {
    let before = flow_dir(root, &state.flow)?.join(variant).join(BEFORE_FILE);
    match decision {
        Decision::Revert => {
            let bytes =
                fs::read(&before).with_context(|| format!("reading {}", before.display()))?;
            let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
            restore_snapshot(root, &state.change_paths, &snapshot)?;
            remove_if_exists(&before)
        }
        Decision::Keep => remove_if_exists(&before),
        Decision::Baseline | Decision::Rerun => Ok(()),
    }
}

// ---------- runner ----------

/// Produces one candidate by editing the change paths.
pub trait Proposer {
    /// Returns a one-line description of the change, or `None` when there is nothing to try.
    fn propose(&mut self, root: &Path, state: &State, variant: &str) -> Result<Option<String>>;
}

/// Runs the flow command for one variant. It must write `<out_dir>/results.jsonl`, plus
/// `errors.jsonl` and `traces/` when applicable.
pub trait Evaluator {
    fn run(&mut self, root: &Path, check: &Check) -> Result<()>;
}

/// Runs the resolved command through `sh -c` in the workspace root. A non-zero exit is an error
/// with stderr attached; the gate separately checks that every slot has a row.
pub struct ShellEvaluator;

impl Evaluator for ShellEvaluator {
    fn run(&mut self, root: &Path, check: &Check) -> Result<()> {
        fs::create_dir_all(root.join(&check.out_dir))?;
        let output = Process::new("sh")
            .arg("-c")
            .arg(&check.command)
            .current_dir(root)
            .output()
            .with_context(|| format!("starting the ratchet command for {}", check.variant))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = stderr
                .chars()
                .rev()
                .take(800)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            bail!(
                "ratchet command for {} exited with {}: {tail}",
                check.variant,
                output.status
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// A stop condition was reached (plateau or round budget).
    Done,
    /// The proposer returned no candidate.
    NoCandidate,
    /// The variant needs another evaluator pass before it can be judged; the candidate stays in place.
    Rerun { variant: String },
}

fn run_variant(
    root: &Path,
    state: &mut State,
    variant: &str,
    change: &str,
    evaluator: &mut dyn Evaluator,
) -> Result<Gate> {
    let check = check_variant(root, state, variant)?;
    evaluator.run(root, &check)?;
    let gate = gate_variant(root, state, variant, change)?;
    settle(root, state, variant, gate.decision)?;
    Ok(gate)
}

/// Hill-climbs: baseline, then propose, evaluate, gate, and keep or revert until a stop
/// condition. A pending rerun is resumed once before new candidates are proposed.
pub fn climb(
    root: &Path,
    flow: &str,
    proposer: &mut dyn Proposer,
    evaluator: &mut dyn Evaluator,
) -> Result<Halt> {
    let mut state = require(root, flow)?;
    require_approvals(root, &state)?;
    if state.rounds.is_empty() {
        if run_variant(root, &mut state, "baseline", "baseline", evaluator)?.done {
            return Ok(Halt::Done);
        }
    } else if let Some(pending) = state
        .rounds
        .last()
        .filter(|round| round.decision == Decision::Rerun)
        .cloned()
    {
        let gate = run_variant(
            root,
            &mut state,
            &pending.variant,
            &pending.change,
            evaluator,
        )?;
        if gate.decision == Decision::Rerun {
            return Ok(Halt::Rerun {
                variant: pending.variant,
            });
        }
        if gate.done {
            return Ok(Halt::Done);
        }
    }
    loop {
        if finished(&state) {
            return Ok(Halt::Done);
        }
        let variant = expected_variant(&state);
        // A candidate snapshotted but never gated (evaluator or gate error) is still in place.
        // Discard it first so the new snapshot holds the pre-candidate surface.
        let variant_dir = flow_dir(root, &state.flow)?.join(&variant);
        if variant_dir.join(BEFORE_FILE).exists() {
            settle(root, &state, &variant, Decision::Revert)?;
        }
        clear_outputs(&variant_dir)?;
        save_snapshot(root, &state, &variant)?;
        let change = match proposer.propose(root, &state, &variant) {
            Ok(Some(change)) => change,
            Ok(None) => {
                settle(root, &state, &variant, Decision::Revert)?;
                return Ok(Halt::NoCandidate);
            }
            Err(err) => {
                settle(root, &state, &variant, Decision::Revert)?;
                return Err(err);
            }
        };
        let gate = run_variant(root, &mut state, &variant, &change, evaluator)?;
        if gate.decision == Decision::Rerun {
            return Ok(Halt::Rerun { variant });
        }
        if gate.done {
            return Ok(Halt::Done);
        }
    }
}

#[cfg(test)]
mod tests;
