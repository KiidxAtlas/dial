//! Per-model evidence for local models, kept in the user directory file `local-profile.json`, and the
//! setting suggestions it supports. Evidence is counted from observed sessions (failing rounds,
//! edit outcomes per grammar, intercepted generations, measured speeds, model loads), never from
//! model names. Suggestions are offered in `/model` settings and applied only by the user.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::settings_store;
use crate::platform::atomic::write_private;

/// File in the user directory, keyed by `"{provider}/{model}"`.
pub const FILE: &str = "local-profile.json";

/// Successes and failures of one kind of action.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tally {
    pub ok: u64,
    pub failed: u64,
}

/// A running mean of a measured rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mean {
    pub samples: u64,
    pub mean: f64,
}

/// A speculative-decoding draft model under trial: generation speed with and without it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DraftTrial {
    pub draft_model: String,
    /// Decode tokens/s measured without the draft before it was applied.
    pub baseline: Mean,
    /// Decode tokens/s measured with the draft.
    pub with_draft: Mean,
}

/// Everything recorded for one local model. Unknown fields are kept out of the struct (serde
/// default), so an older or newer file still loads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    /// Highest escalation step reached (`assistance.local.escalate_after`).
    pub stage: u8,
    /// Tool rounds, and those that failed (a malformed call or a failed edit/write).
    pub rounds: Tally,
    /// Edit/write outcomes per edit grammar (`EditMode` serialized name, e.g. `"replace"`).
    pub edits: BTreeMap<String, Tally>,
    /// Model requests, and requests whose reasoning the local reasoning cap intercepted.
    pub requests: u64,
    pub reasoning_capped: u64,
    /// Generations the repetition detector stopped, and penalty retries that then succeeded/failed.
    pub repetition_loops: u64,
    pub penalty_retries: Tally,
    /// Measured decode (generation) and prefill (prompt processing) tokens/s.
    pub decode_tps: Mean,
    pub prefill_tps: Mean,
    /// Prompt tokens served from the server's prompt cache, of all prompt tokens reported with it.
    pub cached_prompt_tokens: u64,
    pub reported_prompt_tokens: u64,
    /// Model loads observed (a request that started in the loading phase) and their total seconds.
    pub loads: u64,
    pub load_secs: f64,
    /// Side requests that went to another model on the main model's local server.
    pub side_model_swaps: u64,
    /// Context overflows, and the window the server served at the last probe.
    pub overflows: u64,
    pub served_context: Option<u32>,
    /// Draft model trial, when one was applied after a suggestion.
    pub draft: Option<DraftTrial>,
}

/// What the suggestion engine needs to know about the model's current settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Current {
    /// `"mlx"`, `"lm-studio"`, `"ollama"`, `"llama-cpp"` or another provider id.
    pub provider: String,
    pub model: String,
    /// Edit grammar in effect (`EditMode` serialized name).
    pub edit_mode: String,
    /// The edit grammar is pinned by the user (profile, harness, variants or override).
    pub edit_mode_pinned: bool,
    /// Configured window and the profile's own `context_window`, if any.
    pub context_window: u32,
    pub profile_context_window: Option<u32>,
    /// Profile `local_sampling.presence_penalty` / `repetition_penalty` already set.
    pub presence_penalty: Option<f64>,
    pub repetition_penalty: Option<f64>,
    /// Profile `keep_alive` (Ollama) already set.
    pub keep_alive: Option<String>,
    /// The draft model in effect (`local_runtime.draft_model`), if any.
    pub draft_model: Option<String>,
    /// A smaller local model with an identical tokenizer (`local_server::draft_candidates`).
    pub draft_candidate: Option<String>,
    /// `assistance.local.side_models` serialized name.
    pub side_models: String,
}

/// One evidence-backed setting change. `path` is a `/model` settings path (the profile field paths
/// `ui::overlay::model_settings` edits, e.g. `"edit_mode"`, `"context_window"`,
/// `"local_sampling.presence_penalty"`, `"keep_alive"`, `"local_runtime.draft_model"`) or, when
/// `global` is true, a config key path (e.g. `"assistance.local.side_models"`).
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub path: String,
    pub global: bool,
    /// The value to apply; `Value::Null` clears the setting.
    pub value: serde_json::Value,
    /// Short imperative title, e.g. "Use the apply_patch edit grammar".
    pub title: String,
    /// The evidence behind it, with numbers, e.g. "edits failed 9 of 20 times under replace, 1 of
    /// 15 under apply_patch".
    pub evidence: String,
}

/// Edit outcomes a grammar needs before its failure rate is compared with another grammar's.
const MIN_EDITS: u64 = 10;
/// A failure share (failed, total) at or above this switches the edit grammar: 3 in 10.
const EDIT_FAILURE_SHARE: (u64, u64) = (3, 10);
/// Escalation stage at which a model that still struggles is pinned to the `replace` grammar.
const PINNING_STAGE: u8 = 2;
/// Repetition stops, and successful penalty retries, before presence penalty is suggested.
const LOOPS_MIN: u64 = 3;
const PENALTY_RETRIES_MIN: u64 = 2;
const PRESENCE_PENALTY: f64 = 0.5;
/// Providers whose `local_sampling` accepts `presence_penalty` (see `llm::profiles`).
const PRESENCE_PROVIDERS: [&str; 3] = ["mlx", "lm-studio", "ollama"];
/// Context overflows before the served window is suggested as `context_window`.
const OVERFLOWS_MIN: u64 = 2;
/// Ollama model loads before a keep-alive is suggested.
const LOADS_MIN: u64 = 3;
const KEEP_ALIVE: &str = "30m";
/// Side requests that swapped the main model out before `reuse_main` is suggested.
const SWAPS_MIN: u64 = 3;
/// Samples each draft speed needs before a draft trial is judged.
const DRAFT_SAMPLES_MIN: u64 = 20;
/// A draft must beat its baseline by this factor to be kept.
const DRAFT_GAIN: f64 = 1.05;

/// The evidence key for one model: `"{provider}/{model}"`.
pub fn model_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

impl Mean {
    /// Adds one measurement. Non-finite and non-positive rates are not measurements and are ignored.
    pub fn add(&mut self, value: f64) {
        if !value.is_finite() || value <= 0.0 {
            return;
        }
        self.samples = self.samples.saturating_add(1);
        self.mean += (value - self.mean) / self.samples as f64;
    }

    /// Folds in another running mean, weighted by its sample count.
    pub fn combine(&mut self, other: &Mean) {
        if other.samples == 0 {
            return;
        }
        if self.samples == 0 {
            *self = *other;
            return;
        }
        let total = self.samples.saturating_add(other.samples);
        self.mean =
            (self.mean * self.samples as f64 + other.mean * other.samples as f64) / total as f64;
        self.samples = total;
    }
}

impl Tally {
    pub fn add(&mut self, ok: bool) {
        if ok {
            self.ok = self.ok.saturating_add(1);
        } else {
            self.failed = self.failed.saturating_add(1);
        }
    }

    pub fn total(&self) -> u64 {
        self.ok.saturating_add(self.failed)
    }

    /// The failed share of all outcomes; `None` before the first outcome.
    pub fn rate(&self) -> Option<f64> {
        let total = self.total();
        (total > 0).then(|| self.failed as f64 / total as f64)
    }

    fn absorb(&mut self, other: &Tally) {
        self.ok = self.ok.saturating_add(other.ok);
        self.failed = self.failed.saturating_add(other.failed);
    }
}

impl Evidence {
    /// Adds a session's counters to the stored record. Counters add, means combine by weight, the
    /// stage only rises, the served window is the newest probe, and a draft trial for a different
    /// draft model replaces the stored one.
    pub fn merge(&mut self, delta: &Evidence) {
        self.stage = self.stage.max(delta.stage);
        self.rounds.absorb(&delta.rounds);
        for (grammar, tally) in &delta.edits {
            self.edits.entry(grammar.clone()).or_default().absorb(tally);
        }
        self.requests = self.requests.saturating_add(delta.requests);
        self.reasoning_capped = self.reasoning_capped.saturating_add(delta.reasoning_capped);
        self.repetition_loops = self.repetition_loops.saturating_add(delta.repetition_loops);
        self.penalty_retries.absorb(&delta.penalty_retries);
        self.decode_tps.combine(&delta.decode_tps);
        self.prefill_tps.combine(&delta.prefill_tps);
        self.cached_prompt_tokens = self
            .cached_prompt_tokens
            .saturating_add(delta.cached_prompt_tokens);
        self.reported_prompt_tokens = self
            .reported_prompt_tokens
            .saturating_add(delta.reported_prompt_tokens);
        self.loads = self.loads.saturating_add(delta.loads);
        self.load_secs += delta.load_secs;
        self.side_model_swaps = self.side_model_swaps.saturating_add(delta.side_model_swaps);
        self.overflows = self.overflows.saturating_add(delta.overflows);
        if delta.served_context.is_some() {
            self.served_context = delta.served_context;
        }
        if let Some(trial) = &delta.draft {
            match &mut self.draft {
                Some(known) if known.draft_model == trial.draft_model => {
                    known.baseline.combine(&trial.baseline);
                    known.with_draft.combine(&trial.with_draft);
                }
                _ => self.draft = Some(trial.clone()),
            }
        }
    }
}

/// Every recorded model. The file holds one object per `"{provider}/{model}"` key. Older files hold
/// a bare integer per key (the escalation stage), which maps to `Evidence { stage, .. }`. An entry
/// that does not parse is skipped, so one damaged record never hides the others.
pub fn read(user_dir: &Path) -> Result<BTreeMap<String, Evidence>> {
    let Some(bytes) = settings_store::read_private(&user_dir.join(FILE))? else {
        return Ok(BTreeMap::new());
    };
    let root: Value = serde_json::from_slice(&bytes).context("local model evidence is not JSON")?;
    let Some(entries) = root.as_object() else {
        bail!("local model evidence is not a JSON object");
    };
    Ok(entries
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), entry(value)?)))
        .collect())
}

fn entry(value: &Value) -> Option<Evidence> {
    if let Some(stage) = value.as_u64() {
        return Some(Evidence {
            stage: u8::try_from(stage.min(u64::from(u8::MAX))).ok()?,
            ..Evidence::default()
        });
    }
    serde_json::from_value(value.clone()).ok()
}

/// Merges `delta` into the stored record for `key` under the file lock, and writes it back private.
pub fn update(user_dir: &Path, key: &str, delta: &Evidence) -> Result<()> {
    settings_store::private_dir(user_dir)?;
    let path = user_dir.join(FILE);
    let _lock = settings_store::locked(&path)?;
    let mut all = read(user_dir)?;
    all.entry(key.to_owned()).or_default().merge(delta);
    write_private(&path, &serde_json::to_vec_pretty(&all)?)
}

/// Whether `tally` has at least `num/den` of its outcomes failed.
fn failure_share_at_least(tally: &Tally, (num, den): (u64, u64)) -> bool {
    let total = tally.total();
    total > 0 && tally.failed.saturating_mul(den) >= total.saturating_mul(num)
}

/// Setting changes the evidence supports, in a stable order. Each rule fires only at its threshold
/// and only when the setting it would change is not already decided by the user or the profile.
pub fn suggestions(e: &Evidence, current: &Current) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let grammar = edit_grammar(e, current);
    let suggested = grammar
        .as_ref()
        .and_then(|s| s.value.as_str().map(str::to_owned));
    out.extend(grammar);
    if e.stage >= PINNING_STAGE
        && !current.edit_mode_pinned
        && current.edit_mode != "replace"
        && suggested.as_deref() != Some("replace")
    {
        out.push(Suggestion {
            path: "edit_mode".into(),
            global: false,
            value: json!("replace"),
            title: "Pin the replace edit grammar".into(),
            evidence: format!(
                "reached escalation stage {} while the {} grammar was in use",
                e.stage, current.edit_mode
            ),
        });
    }
    if PRESENCE_PROVIDERS.contains(&current.provider.as_str())
        && e.repetition_loops >= LOOPS_MIN
        && e.penalty_retries.ok >= PENALTY_RETRIES_MIN
        && current.presence_penalty.is_none()
    {
        out.push(Suggestion {
            path: "local_sampling.presence_penalty".into(),
            global: false,
            value: json!(PRESENCE_PENALTY),
            title: format!("Set presence_penalty to {PRESENCE_PENALTY}"),
            evidence: format!(
                "the repetition detector stopped {} generations; penalty retries succeeded {} and failed {}",
                e.repetition_loops, e.penalty_retries.ok, e.penalty_retries.failed
            ),
        });
    }
    if let Some(served) = e.served_context
        && e.overflows >= OVERFLOWS_MIN
        && served < current.context_window
        && current.profile_context_window.is_none()
    {
        out.push(Suggestion {
            path: "context_window".into(),
            global: false,
            value: json!(served),
            title: format!("Set context_window to {served}"),
            evidence: format!(
                "{} context overflows; the server serves {served} tokens, below the configured {}",
                e.overflows, current.context_window
            ),
        });
    }
    if current.provider == "ollama" && e.loads >= LOADS_MIN && current.keep_alive.is_none() {
        out.push(Suggestion {
            path: "keep_alive".into(),
            global: false,
            value: json!(KEEP_ALIVE),
            title: "Keep the model loaded for 30 minutes".into(),
            evidence: format!(
                "{} model loads averaged {:.1} s",
                e.loads,
                e.load_secs / e.loads as f64
            ),
        });
    }
    if e.side_model_swaps >= SWAPS_MIN && current.side_models == "warn" {
        out.push(Suggestion {
            path: "assistance.local.side_models".into(),
            global: true,
            value: json!("reuse_main"),
            title: "Send side requests to the loaded main model".into(),
            evidence: format!(
                "{} side requests swapped the main model out; {}",
                e.side_model_swaps,
                load_text(e)
            ),
        });
    }
    if current.provider == "mlx"
        && current.draft_model.is_none()
        && let Some(candidate) = &current.draft_candidate
        && e.draft
            .as_ref()
            .is_none_or(|trial| &trial.draft_model != candidate)
    {
        out.push(Suggestion {
            path: "local_runtime.draft_model".into(),
            global: false,
            value: json!(candidate),
            title: format!("Try {candidate} as the draft model"),
            evidence: format!(
                "{candidate} is smaller and shares this model's tokenizer; it is kept only if decode speed improves"
            ),
        });
    }
    if let Some(trial) = &e.draft
        && current.draft_model.as_deref() == Some(trial.draft_model.as_str())
        && trial.baseline.samples >= DRAFT_SAMPLES_MIN
        && trial.with_draft.samples >= DRAFT_SAMPLES_MIN
        && trial.with_draft.mean < trial.baseline.mean * DRAFT_GAIN
    {
        out.push(Suggestion {
            path: "local_runtime.draft_model".into(),
            global: false,
            value: Value::Null,
            title: "Stop using the draft model".into(),
            evidence: format!(
                "{:.1} tok/s with {} over {} samples against {:.1} tok/s without over {}",
                trial.with_draft.mean,
                trial.draft_model,
                trial.with_draft.samples,
                trial.baseline.mean,
                trial.baseline.samples
            ),
        });
    }
    out
}

/// Switch to the grammar that fails clearly less, when the current one fails often.
fn edit_grammar(e: &Evidence, current: &Current) -> Option<Suggestion> {
    let worse = e.edits.get(&current.edit_mode).copied().unwrap_or_default();
    if worse.total() < MIN_EDITS || !failure_share_at_least(&worse, EDIT_FAILURE_SHARE) {
        return None;
    }
    // A better grammar has enough edits and at most half the current failure rate.
    let best = e
        .edits
        .iter()
        .filter(|(name, tally)| {
            name.as_str() != current.edit_mode
                && tally.total() >= MIN_EDITS
                && tally.failed.saturating_mul(2).saturating_mul(worse.total())
                    <= worse.failed.saturating_mul(tally.total())
        })
        .min_by(|a, b| {
            let rate = |t: &Tally| t.failed as f64 / t.total() as f64;
            rate(a.1).total_cmp(&rate(b.1))
        })?;
    let (grammar, tally) = best;
    Some(Suggestion {
        path: "edit_mode".into(),
        global: false,
        value: json!(grammar),
        title: format!("Use the {grammar} edit grammar"),
        evidence: format!(
            "edits failed {} of {} times under {}, {} of {} under {grammar}",
            worse.failed,
            worse.total(),
            current.edit_mode,
            tally.failed,
            tally.total()
        ),
    })
}

fn load_text(e: &Evidence) -> String {
    if e.loads == 0 {
        "model loads were not measured".into()
    } else {
        format!("model loads averaged {:.1} s", e.load_secs / e.loads as f64)
    }
}

#[cfg(test)]
mod tests;
