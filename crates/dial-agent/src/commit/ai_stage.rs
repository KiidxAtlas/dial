//! AI staging: decides which unstaged change units match a user instruction.
//! Each unit is scored independently; the top candidates are then
//! checked against a "none of these" option that can veto the whole selection.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::json;

use super::config::GenerationConfig;
use super::inference::{Inference, Tier};
use crate::config::types::Config;

/// Characters of one unit sent to the judge.
pub const UNIT_CHARS: usize = 2400;
/// Characters of one candidate sent to the verifier.
const VERIFY_CHARS: usize = 600;
/// Minimum part-of-it probability for a unit to be accepted.
const STAGE_THRESHOLD: f64 = 0.6;
/// Probability of "none of the candidates" that rejects every unit.
const NONE_THRESHOLD: f64 = 0.5;
/// Candidates shown to the verifier.
const MAX_CANDIDATES: usize = 8;
/// Concurrent judgments.
const WORKERS: usize = 8;

const LEVEL_UNRELATED: &str = "0";
const LEVEL_TANGENTIAL: &str = "1";
const PART_OF_IT: &str = "2";
const NONE_KEY: &str = "none";

const UNIT_SYSTEM: &str = "You judge one unit of an unstaged git change against the user's staging instruction. \
The input is JSON with instruction, all_changed_files, path, kind, and change (the bounded +/- lines of the unit). \
Reply with only a JSON object giving a probability for each level, summing to 1: \
\"0\" Unrelated: the unit has nothing to do with the instruction. \
\"1\" Tangential: same area, file, or vocabulary, but the instruction does not describe this change. \
\"2\" Part of it: the instruction describes this change itself. \
Probabilities only, no prose.";

const VERIFY_SYSTEM: &str = "You pick the change the user described from a list of candidates. \
The input is JSON with instruction, all_changed_files, and candidates (key, path, kind, change). \
Reply with only a JSON object giving a probability for each key, summing to 1: \
\"none\" means none of the candidates is the change the user described; \
each candidate key (c0, c1, ...) means that candidate is the change the user described. \
Probabilities only, no prose.";

/// What a unit is, for the judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// One hunk of a modified tracked file.
    Hunk,
    /// An untracked file, judged by its head.
    NewFile,
    /// One hunk of a deleted tracked file.
    DeletedFile,
    /// A binary file, judged by path only.
    Binary,
}

impl UnitKind {
    fn label(self) -> &'static str {
        match self {
            Self::Hunk => "hunk",
            Self::NewFile => "new file",
            Self::DeletedFile => "deleted file",
            Self::Binary => "binary file",
        }
    }
}

/// One independently stageable piece of the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageUnit {
    pub path: String,
    pub kind: UnitKind,
    /// The changed lines (`+`/`-`), or empty for binary files.
    pub change: String,
}

/// The verdict for one unit, aligned with the input units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageDecision {
    pub(crate) score: f64,
    pub accepted: bool,
}

/// Classifies `units` against `instruction`. `paths` lists every changed file for context.
/// Judgments run on eight worker threads. If every judgment fails, the first error is returned.
pub fn classify_units(
    config: &Config,
    generation: &GenerationConfig,
    instruction: &str,
    paths: &[String],
    units: &[StageUnit],
) -> Result<Vec<StageDecision>, String> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let inference = Inference::new(config, generation);
    let mut scores = Vec::with_capacity(units.len());
    let mut failures = 0usize;
    let mut first_error: Option<String> = None;
    for result in judge_all(&inference, instruction, paths, units) {
        match result {
            Ok(score) => scores.push(score),
            Err(error) => {
                failures += 1;
                first_error.get_or_insert(error);
                scores.push(0.0);
            }
        }
    }
    if failures == units.len() {
        return Err(first_error.unwrap_or_else(|| "AI staging judgments failed".to_owned()));
    }

    let mut accepted: Vec<bool> = scores
        .iter()
        .map(|score| *score >= STAGE_THRESHOLD)
        .collect();
    let candidates = verify_candidates(&scores);
    if !candidates.is_empty() {
        let none = verify(&inference, instruction, paths, units, &candidates)?;
        if none >= NONE_THRESHOLD {
            accepted.iter_mut().for_each(|flag| *flag = false);
        }
    }
    Ok(scores
        .into_iter()
        .zip(accepted)
        .map(|(score, accepted)| StageDecision { score, accepted })
        .collect())
}

fn judge_all(
    inference: &Inference<'_>,
    instruction: &str,
    paths: &[String],
    units: &[StageUnit],
) -> Vec<Result<f64, String>> {
    let never_cancelled = AtomicBool::new(false);
    map_bounded(units, &never_cancelled, |unit| {
        judge_unit(inference, instruction, paths, unit)
    })
    .into_iter()
    .map(|slot| slot.unwrap_or_else(|| Err("AI staging judgment did not run".to_owned())))
    .collect()
}

/// Runs `work` over `items` on at most [`WORKERS`] threads and returns the results in input order.
/// Once `cancel` is set no further item is admitted; its slot stays `None`. Items already running
/// finish normally.
pub(crate) fn map_bounded<T: Sync, R: Send>(
    items: &[T],
    cancel: &AtomicBool,
    work: impl Fn(&T) -> R + Sync,
) -> Vec<Option<R>> {
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(items.len()) {
            scope.spawn(|| {
                loop {
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = work(item);
                    slots.lock()[index] = Some(result);
                }
            });
        }
    });
    slots.into_inner()
}

fn judge_unit(
    inference: &Inference<'_>,
    instruction: &str,
    paths: &[String],
    unit: &StageUnit,
) -> Result<f64, String> {
    let state = json!({
        "instruction": instruction,
        "all_changed_files": paths,
        "path": unit.path,
        "kind": unit.kind.label(),
        "change": bound(&unit.change, UNIT_CHARS),
    });
    inference.complete(Tier::Fast, UNIT_SYSTEM, &state.to_string(), |reply| {
        parse_score(&reply.text)
    })
}

/// Indices of the best units (score at least the threshold), highest score first, capped.
pub(crate) fn verify_candidates(scores: &[f64]) -> Vec<usize> {
    let mut ranked: Vec<usize> = (0..scores.len())
        .filter(|&index| scores[index] >= STAGE_THRESHOLD)
        .collect();
    ranked.sort_by(|&left, &right| {
        scores[right]
            .total_cmp(&scores[left])
            .then(left.cmp(&right))
    });
    ranked.truncate(MAX_CANDIDATES);
    ranked
}

/// Asks the verifier to pick among `candidates` and returns the probability of "none".
fn verify(
    inference: &Inference<'_>,
    instruction: &str,
    paths: &[String],
    units: &[StageUnit],
    candidates: &[usize],
) -> Result<f64, String> {
    let options: Vec<serde_json::Value> = candidates
        .iter()
        .enumerate()
        .map(|(position, &unit_index)| {
            let unit = &units[unit_index];
            json!({
                "key": format!("c{position}"),
                "path": unit.path,
                "kind": unit.kind.label(),
                "change": bound(&unit.change, VERIFY_CHARS),
            })
        })
        .collect();
    let state = json!({
        "instruction": instruction,
        "all_changed_files": paths,
        "candidates": options,
    });
    let count = candidates.len();
    inference.complete(Tier::Fast, VERIFY_SYSTEM, &state.to_string(), |reply| {
        parse_pick(&reply.text, count)
    })
}

/// Bounds `text` to `limit` characters, marking the cut with an ellipsis line.
pub(crate) fn bound(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("\n…");
    out
}

/// The `+`/`-` lines of a hunk, joined by newlines.
pub fn changed_lines(lines: &[String]) -> String {
    lines
        .iter()
        .filter(|line| line.starts_with('+') || line.starts_with('-'))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts the JSON object from a model reply, tolerating surrounding prose or code fences.
fn json_object(text: &str) -> Result<serde_json::Value, String> {
    let missing = || "reply has no JSON object".to_owned();
    let start = text.find('{').ok_or_else(missing)?;
    let end = text.rfind('}').ok_or_else(missing)?;
    if end < start {
        return Err(missing());
    }
    serde_json::from_str(&text[start..=end])
        .map_err(|error| format!("reply is not valid JSON: {error}"))
}

/// Reads the named probabilities, rejecting negatives and normalising them to sum to one.
pub(crate) fn parse_probabilities(
    text: &str,
    keys: &[&str],
) -> Result<BTreeMap<String, f64>, String> {
    let value = json_object(text)?;
    let object = value
        .as_object()
        .ok_or_else(|| "reply JSON is not an object".to_owned())?;
    let mut probabilities = BTreeMap::new();
    for key in keys {
        let probability = object
            .get(*key)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| format!("reply has no probability for \"{key}\""))?;
        if !probability.is_finite() || probability < 0.0 {
            return Err(format!("invalid probability for \"{key}\""));
        }
        probabilities.insert((*key).to_owned(), probability);
    }
    let total: f64 = probabilities.values().sum();
    if total <= 0.0 {
        return Err("reply probabilities sum to zero".to_owned());
    }
    for probability in probabilities.values_mut() {
        *probability /= total;
    }
    Ok(probabilities)
}

/// The probability that a unit is part of the instruction.
pub(crate) fn parse_score(text: &str) -> Result<f64, String> {
    let probabilities =
        parse_probabilities(text, &[LEVEL_UNRELATED, LEVEL_TANGENTIAL, PART_OF_IT])?;
    Ok(probabilities[PART_OF_IT])
}

/// The probability that none of `candidates` candidates is the described change.
pub(crate) fn parse_pick(text: &str, candidates: usize) -> Result<f64, String> {
    let mut keys = vec![NONE_KEY.to_owned()];
    keys.extend((0..candidates).map(|position| format!("c{position}")));
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let probabilities = parse_probabilities(text, &refs)?;
    Ok(probabilities[NONE_KEY])
}

#[cfg(test)]
mod tests;
