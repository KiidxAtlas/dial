//! Turn externally verified runs into training records.
//!
//! Two sources, neither of which trusts the model's own opinion of its work:
//! 1. Tool-call repairs: the conversation before a malformed call plus the later call that was
//!    schema-valid and ran without error (call form, verified by dial's validator and sandbox).
//! 2. Verified successes: trajectories whose outcome was assigned externally (a procedural
//!    verifier or a user verdict, never the agent's own command exit code). Request snapshots
//!    preserve the actual inference context and label only the new assistant target. Legacy
//!    conversations use cleaned prefixes, removing form failures and their correction messages.
//!    All prefixes and repairs sharing a trajectory or first user prompt stay in one dataset split,
//!    including snapshots before/after compaction. Existing split assignments are preserved; contaminated
//!    datasets are rejected rather than reused for evaluation.
use crate::{config::types::Config, storage::store::Store};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// Only trajectories at the top of the outcome scale teach what "good" looks like.
const SUCCESS_SCORE: f64 = 100.0;
const TRAJECTORY_LIMIT: usize = 100_000;
/// Long trajectories would otherwise dominate; the final answer turn is always kept.
const PER_TRAJECTORY: usize = 8;
/// With more than `BALANCE_ABOVE` families present, none may exceed this share of the added records.
const FAMILY_PERCENT: usize = 35;
const BALANCE_ABOVE: usize = 3;
const RECOVERY_FAMILY: &str = "tool-recovery";
/// Runtime user messages that answer a malformed tool call or a tool call written as text.
const CORRECTIONS: [&str; 2] = [
    "Your tool call was unreadable",
    "That looked like a tool call written as plain text",
];

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub added_train: usize,
    pub added_valid: usize,
    pub from_repairs: usize,
    pub from_successes: usize,
    pub from_other_models: usize,
    /// Verifier-passed teacher (`improve.distill`) trajectories; also counted in `from_other_models`.
    pub from_teacher: usize,
    pub learned_from: BTreeMap<String, usize>,
    pub skipped_overlap: usize,
    pub skipped_oversize: usize,
    pub skipped_invalid: usize,
    pub skipped_siblings: usize,
    pub skipped_learn_cap: usize,
    pub skipped_form_failure_turns: usize,
    pub total_train: usize,
    pub total_valid: usize,
}

impl Report {
    pub fn added(&self) -> usize {
        self.added_train + self.added_valid
    }

    /// Counts why a record was rejected; true when it should be kept.
    fn tally(&mut self, verdict: Verdict) -> bool {
        match verdict {
            Verdict::Keep => return true,
            Verdict::Invalid => self.skipped_invalid += 1,
            Verdict::Overlap => self.skipped_overlap += 1,
            Verdict::Oversize => self.skipped_oversize += 1,
        }
        false
    }
}

struct Candidate {
    digest: String,
    line: String,
    group: String,
    family: String,
    repair: bool,
    /// Sibling model database this record was learned from; `None` for this model's own runs.
    origin: Option<String>,
    preference_pairs: Vec<Value>,
}

/// Append new records to `config.training.{train,valid}_data`. `held_out` holds prompts of the sealed
/// gate; any record that shows one to the model is dropped so training never sees the exam.
pub fn harvest(config: &Config, store: &Store, held_out: &HashSet<String>) -> Result<Report> {
    let t = &config.training;
    let max_tokens = t.max_seq_length as usize;
    ensure!(
        t.train_data != t.valid_data
            && !matches!((t.train_data.canonicalize(), t.valid_data.canonicalize()), (Ok(a), Ok(b)) if a == b),
        "training and validation paths must differ"
    );
    let (mut seen, total_train, train_groups) = digests(&t.train_data)?;
    let (valid_seen, total_valid, valid_groups) = digests(&t.valid_data)?;
    ensure!(
        train_groups.is_disjoint(&valid_groups),
        "existing training and validation prompts overlap"
    );
    ensure!(
        seen.is_disjoint(&valid_seen),
        "existing training and validation messages overlap"
    );
    seen.extend(valid_seen);
    let mut report = Report {
        total_train,
        total_valid,
        ..Report::default()
    };

    // Keyed by digest: deduplicates and gives every later step a deterministic order.
    let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
    let mut links: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for (trajectory, payload) in store.tool_recoveries()? {
        if !config.improve.learn_from_sessions
            && !store
                .trajectory_domain(trajectory)?
                .starts_with("selfplay:")
        {
            continue;
        }
        let Some(record) = build(&payload, trajectory) else {
            report.skipped_invalid += 1;
            continue;
        };
        let mut screen = Screen::new(held_out, max_tokens, &record["tools"]);
        record["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .for_each(|m| screen.push(m));
        if !report.tally(screen.verdict()) {
            continue;
        }
        let digest = digest(&record["messages"]);
        let Some(group) = prompt_group(&record["messages"]) else {
            report.skipped_invalid += 1;
            continue;
        };
        link_group(&mut links, &group, &trajectory_key(None, trajectory));
        let preference_pairs = verified_preference_pairs(&payload, &record);
        let candidate = Candidate {
            line: format!("{}\n", render_record(&record)),
            digest: digest.clone(),
            group,
            family: RECOVERY_FAMILY.into(),
            repair: true,
            preference_pairs,
            origin: None,
        };
        match candidates.entry(digest) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().repair {
                    entry
                        .get_mut()
                        .preference_pairs
                        .extend(candidate.preference_pairs);
                }
            }
        }
    }
    for (trajectory, _version, domain, _score, source, payload) in
        store.verified_successes(SUCCESS_SCORE, TRAJECTORY_LIMIT)?
    {
        if source.starts_with("check:") {
            continue;
        }
        if !domain.starts_with("selfplay:") && !config.improve.learn_from_sessions {
            continue;
        }
        for candidate in success_candidates(
            trajectory,
            &domain,
            &source,
            &payload,
            held_out,
            max_tokens,
            &mut report,
            None,
        ) {
            link_group(
                &mut links,
                &candidate.group,
                &trajectory_key(None, trajectory),
            );
            candidates
                .entry(candidate.digest.clone())
                .or_insert(candidate);
        }
    }
    // Sibling databases contribute only when named in `training.learn_from`. Their records are
    // namespaced by origin, and `cap_learned` limits them after local records are chosen.
    for (origin, sibling) in siblings(config, &mut report) {
        match sibling_candidates(
            config,
            &origin,
            &sibling,
            held_out,
            max_tokens,
            &mut report,
            &mut links,
            true,
        ) {
            Ok(found) => {
                for candidate in found {
                    candidates
                        .entry(candidate.digest.clone())
                        .or_insert(candidate);
                }
            }
            Err(_) => report.skipped_siblings += 1,
        }
    }
    // Failure-driven distillation: verifier-passed coach runs from their own database. Successes only;
    // a teacher's tool repairs are not verified outcomes. Ignored unless distillation is enabled.
    if super::distill::enabled(config) {
        let dir = super::distill::teacher_dir(config);
        let origin = super::distill::origin_tag(&config.for_role("coach").model);
        if dir.join("state.sqlite3").is_file() {
            match Store::open_read_only(&dir) {
                Ok(teacher) => match sibling_candidates(
                    config,
                    &origin,
                    &teacher,
                    held_out,
                    max_tokens,
                    &mut report,
                    &mut links,
                    false,
                ) {
                    Ok(found) => {
                        for candidate in found {
                            candidates
                                .entry(candidate.digest.clone())
                                .or_insert(candidate);
                        }
                    }
                    Err(_) => report.skipped_siblings += 1,
                },
                Err(_) => report.skipped_siblings += 1,
            }
        }
    }
    let roots = connected_groups(&links);
    let normalize = |groups: HashSet<String>| {
        groups
            .into_iter()
            .map(|group| roots.get(&group).cloned().unwrap_or(group))
            .collect::<HashSet<_>>()
    };
    let train_groups = normalize(train_groups);
    let valid_groups = normalize(valid_groups);
    ensure!(
        train_groups.is_disjoint(&valid_groups),
        "training and validation prompts or trajectories overlap"
    );

    // Balance over everything eligible, not just what is new, so the result does not depend on how
    // much was harvested before: a re-run selects the same records and finds nothing new.
    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let minimum = crate::learn::improve::min_examples(config);
    let eligible = cap_learned(
        balance(candidates.into_values().collect()),
        t.learn_from_max_percent,
        minimum,
        &mut report,
    );
    for mut candidate in eligible {
        candidate.group = roots
            .get(&candidate.group)
            .cloned()
            .unwrap_or(candidate.group);
        if !seen.contains(&candidate.digest) {
            groups
                .entry(candidate.group.clone())
                .or_default()
                .push(candidate);
        }
    }
    let mut train = BTreeMap::new();
    let mut valid = BTreeMap::new();
    for (group, records) in groups {
        let validation = valid_groups.contains(&group)
            || (!train_groups.contains(&group) && u8::from_str_radix(&group[..2], 16)? % 5 == 0);
        if validation {
            valid.insert(group, records);
        } else {
            train.insert(group, records);
        }
    }
    // Fill small splits only with whole, previously unseen groups. Never move a group with records
    // already written to either split, or split one conversation to meet the floor.
    let floor = t.batch_size as usize;
    let mut train_count = total_train + train.values().map(Vec::len).sum::<usize>();
    let mut valid_count = total_valid + valid.values().map(Vec::len).sum::<usize>();
    let promote: Vec<String> = train
        .iter()
        .filter_map(|(group, records)| {
            if valid_count < floor
                && !train_groups.contains(group)
                && train_count >= floor + records.len()
            {
                train_count -= records.len();
                valid_count += records.len();
                Some(group.clone())
            } else {
                None
            }
        })
        .collect();
    for group in promote {
        if let Some(records) = train.remove(&group) {
            valid.insert(group, records);
        }
    }
    let demote: Vec<String> = valid
        .iter()
        .filter_map(|(group, records)| {
            if train_count < floor
                && !valid_groups.contains(group)
                && valid_count >= floor + records.len()
            {
                train_count += records.len();
                valid_count -= records.len();
                Some(group.clone())
            } else {
                None
            }
        })
        .collect();
    for group in demote {
        if let Some(records) = valid.remove(&group) {
            train.insert(group, records);
        }
    }
    let train: Vec<_> = train.into_values().flatten().collect();
    let valid: Vec<_> = valid.into_values().flatten().collect();
    for candidate in train.iter().chain(&valid) {
        if candidate.repair {
            report.from_repairs += 1
        } else {
            report.from_successes += 1
        }
        if let Some(origin) = &candidate.origin {
            if origin.starts_with("teacher-") {
                report.from_teacher += 1;
            }
            report.from_other_models += 1;
            *report.learned_from.entry(origin.clone()).or_default() += 1;
        }
    }
    let train_lines: String = train.iter().map(|c| render_candidate(c, true)).collect();
    let valid_lines: String = valid.iter().map(|c| render_candidate(c, false)).collect();
    append(&t.train_data, &train_lines)?;
    append(&t.valid_data, &valid_lines)?;
    report.added_train = train.len();
    report.added_valid = valid.len();
    report.total_train += report.added_train;
    report.total_valid += report.added_valid;
    let harvested: usize = store
        .get_meta("harvested_total")?
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(0);
    store.set_meta("harvested_total", &(harvested + report.added()).to_string())?;
    Ok(report)
}

enum Verdict {
    Keep,
    Invalid,
    Overlap,
    Oversize,
}

/// Incrementally judges a message prefix whose last message carries the training loss.
struct Screen<'a> {
    held_out: &'a HashSet<String>,
    max_tokens: usize,
    tools_len: usize,
    bytes: usize,
    count: usize,
    user_seen: bool,
    malformed: bool,
    overlap: bool,
    last_assistant: bool,
    /// Serialized size of the last message's reasoning, stripped once another message follows.
    reasoning: usize,
}

impl<'a> Screen<'a> {
    fn new(held_out: &'a HashSet<String>, max_tokens: usize, tools: &Value) -> Self {
        let tools_len = if tools.as_array().is_some_and(|t| !t.is_empty()) {
            tools.to_string().len()
        } else {
            0
        };
        Self {
            held_out,
            max_tokens,
            tools_len,
            bytes: 0,
            count: 0,
            user_seen: false,
            malformed: false,
            overlap: false,
            last_assistant: false,
            reasoning: 0,
        }
    }

    fn push(&mut self, message: &Value) {
        self.bytes -= self.reasoning;
        self.bytes += message.to_string().len();
        self.count += 1;
        self.malformed |= !well_formed(message);
        self.last_assistant = message["role"] == "assistant";
        // `,"reasoning_content":` plus the serialized string.
        self.reasoning = message
            .get("reasoning_content")
            .filter(|_| self.last_assistant)
            .map_or(0, |r| r.to_string().len() + 21);
        if message["role"] == "user" {
            self.user_seen = true;
            self.overlap |= self
                .held_out
                .contains(message["content"].as_str().unwrap_or("").trim());
        }
    }

    fn verdict(&self) -> Verdict {
        if self.malformed || !self.user_seen || !self.last_assistant {
            return Verdict::Invalid;
        }
        if self.overlap {
            return Verdict::Overlap;
        }
        // Serialized array length: elements, commas and brackets. Tokens are at least ~1 per 2 bytes
        // in the worst case; the worker rejects over-length examples outright, so skip them here
        // instead of failing the whole training run.
        if (self.bytes + self.count + 1 + self.tools_len) / 2 > self.max_tokens {
            return Verdict::Oversize;
        }
        Verdict::Keep
    }
}

fn digest(messages: &Value) -> String {
    format!("{:x}", Sha256::digest(messages.to_string().as_bytes()))
}

fn prompt_group(messages: &Value) -> Option<String> {
    let prompt = messages.as_array()?.iter().find(|m| m["role"] == "user")?["content"].as_str()?;
    Some(digest(&json!(prompt.trim())))
}

fn trajectory_group(source: &str) -> Option<String> {
    if let Some(rest) = source.strip_prefix("dial:learned:") {
        let (origin, rest) = rest.split_once(':')?;
        if origin.is_empty() {
            return None;
        }
        let id = trajectory_id(rest)?;
        return Some(trajectory_key(Some(origin), id));
    }
    let rest = source
        .strip_prefix("dial:verified:")
        .or_else(|| source.strip_prefix("dial:tool-recovery:"))?;
    let id = trajectory_id(rest)?;
    Some(trajectory_key(None, id))
}

fn trajectory_id(rest: &str) -> Option<i64> {
    let id = rest.split(':').next()?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok()
}

/// Trajectory ids are unique only within one database, so a sibling's trajectories carry its origin.
fn trajectory_key(origin: Option<&str>, trajectory: i64) -> String {
    match origin {
        Some(origin) => format!("trajectory:{origin}:{trajectory}"),
        None => format!("trajectory:{trajectory}"),
    }
}

fn link_group(links: &mut BTreeMap<String, HashSet<String>>, prompt: &str, trajectory: &str) {
    links
        .entry(prompt.to_owned())
        .or_default()
        .insert(trajectory.to_owned());
    links
        .entry(trajectory.to_owned())
        .or_default()
        .insert(prompt.to_owned());
}

/// Other model databases in this project named by `training.learn_from` (or all of them with
/// `learn_from_all_verified`), keyed by directory name. A name matches a database's model name,
/// or that name without its `provider/` prefix, which the database does not record. Databases that
/// cannot be read are counted in `skipped_siblings`.
fn siblings(config: &Config, report: &mut Report) -> Vec<(String, Store)> {
    let t = &config.training;
    if t.learn_from.is_empty() && !t.learn_from_all_verified {
        return Vec::new();
    }
    let Some(models) = config.state_dir.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(models) else {
        return Vec::new();
    };
    let own = config.state_dir.canonicalize().ok();
    let mut dirs: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    dirs.sort();
    let mut found = Vec::new();
    for dir in dirs {
        let Some(origin) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !dir.join("state.sqlite3").is_file() || (own.is_some() && dir.canonicalize().ok() == own)
        {
            continue;
        }
        let Ok(store) = Store::open_read_only(&dir) else {
            report.skipped_siblings += 1;
            continue;
        };
        let Ok(Some(model)) = store.get_meta("model") else {
            report.skipped_siblings += 1;
            continue;
        };
        let named = t.learn_from.iter().any(|entry| {
            entry == &model || entry.split_once('/').is_some_and(|(_, name)| name == model)
        });
        if named || t.learn_from_all_verified {
            found.push((origin, store));
        }
    }
    found
}

/// Verified repairs and successes from one sibling database, screened like local data. Targets lose
/// their reasoning, and no preference pairs carry over: they were verified against the sibling's own
/// model, not this one.
#[allow(clippy::too_many_arguments)]
fn sibling_candidates(
    config: &Config,
    origin: &str,
    sibling: &Store,
    held_out: &HashSet<String>,
    max_tokens: usize,
    report: &mut Report,
    links: &mut BTreeMap<String, HashSet<String>>,
    repairs: bool,
) -> Result<Vec<Candidate>> {
    let mut found = Vec::new();
    let repair_rows = if repairs {
        sibling.tool_recoveries()?
    } else {
        Vec::new()
    };
    for (trajectory, payload) in repair_rows {
        if !config.improve.learn_from_sessions
            && !sibling
                .trajectory_domain(trajectory)?
                .starts_with("selfplay:")
        {
            continue;
        }
        let Some(record) = foreign_repair(&payload, origin, trajectory) else {
            report.skipped_invalid += 1;
            continue;
        };
        let mut screen = Screen::new(held_out, max_tokens, &record["tools"]);
        record["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .for_each(|m| screen.push(m));
        if !report.tally(screen.verdict()) {
            continue;
        }
        let Some(group) = prompt_group(&record["messages"]) else {
            report.skipped_invalid += 1;
            continue;
        };
        link_group(links, &group, &trajectory_key(Some(origin), trajectory));
        found.push(Candidate {
            digest: digest(&record["messages"]),
            line: format!("{}\n", render_record(&record)),
            group,
            family: RECOVERY_FAMILY.into(),
            repair: true,
            preference_pairs: Vec::new(),
            origin: Some(origin.to_owned()),
        });
    }
    for (trajectory, _version, domain, _score, source, payload) in
        sibling.verified_successes(SUCCESS_SCORE, TRAJECTORY_LIMIT)?
    {
        if source.starts_with("check:")
            || (!domain.starts_with("selfplay:") && !config.improve.learn_from_sessions)
        {
            continue;
        }
        for candidate in success_candidates(
            trajectory,
            &domain,
            &source,
            &payload,
            held_out,
            max_tokens,
            report,
            Some(origin),
        ) {
            link_group(
                links,
                &candidate.group,
                &trajectory_key(Some(origin), trajectory),
            );
            found.push(candidate);
        }
    }
    Ok(found)
}

/// A tool repair from a sibling: the record `build` makes, with reasoning removed from every turn and
/// its source naming the origin.
fn foreign_repair(payload: &Value, origin: &str, trajectory: i64) -> Option<Value> {
    let mut record = build(payload, trajectory)?;
    let mut messages = record["messages"].as_array()?.clone();
    strip_reasoning(&mut messages, false);
    record["messages"] = Value::Array(messages);
    record["source"] = json!(format!("dial:learned:{origin}:{trajectory}:tool-recovery"));
    Some(record)
}

/// Learned records are admitted only alongside this model's own records. They may fill the batch up
/// to `minimum` records in total (so a model with little local data can still train), and beyond that
/// at most `local * percent / (100 - percent)`. Keeps the lowest digests so reruns agree. `percent` is
/// validated to be at most 50 when the configuration loads.
fn cap_learned(
    candidates: Vec<Candidate>,
    percent: u32,
    minimum: usize,
    report: &mut Report,
) -> Vec<Candidate> {
    let local = candidates.iter().filter(|c| c.origin.is_none()).count();
    let percent = percent.min(50) as usize;
    let allowed = minimum
        .saturating_sub(local)
        .max(local * percent / (100 - percent));
    let mut foreign: Vec<String> = candidates
        .iter()
        .filter(|c| c.origin.is_some())
        .map(|c| c.digest.clone())
        .collect();
    foreign.sort();
    let dropped: HashSet<String> = foreign.into_iter().skip(allowed).collect();
    report.skipped_learn_cap = dropped.len();
    candidates
        .into_iter()
        .filter(|c| !dropped.contains(&c.digest))
        .collect()
}

/// Connected prompt/trajectory components, identified by their lowest prompt hash. Register links
/// before message deduplication so repeated trajectories cannot lose their isolation constraint.
fn connected_groups(links: &BTreeMap<String, HashSet<String>>) -> BTreeMap<String, String> {
    let mut roots = BTreeMap::new();
    for start in links.keys() {
        if roots.contains_key(start) {
            continue;
        }
        let mut pending = vec![start.as_str()];
        while let Some(node) = pending.pop() {
            if roots.contains_key(node) {
                continue;
            }
            roots.insert(node.to_owned(), start.clone());
            if let Some(neighbors) = links.get(node) {
                pending.extend(neighbors.iter().map(String::as_str));
            }
        }
    }
    roots
}

fn render(messages: Vec<Value>, tools: &Value, source: String) -> Value {
    let mut record = json!({"messages":messages,"source":source,"verified":true});
    // The worker rejects an empty schema list, so an agent run without tools carries none.
    if tools.as_array().is_some_and(|t| !t.is_empty()) {
        record["tools"] = tools.clone();
    }
    record
}

fn render_record(record: &Value) -> String {
    serde_json::to_string(record).expect("training record is serializable")
}

fn render_candidate(candidate: &Candidate, train: bool) -> String {
    if !train || candidate.preference_pairs.is_empty() {
        return candidate.line.clone();
    }
    let mut record: Value =
        serde_json::from_str(candidate.line.trim_end()).expect("candidate is valid JSON");
    record["preference_pairs"] = json!(candidate.preference_pairs);
    format!("{}\n", render_record(&record))
}

/// Training templates expect tool-call arguments as objects, while the OpenAI wire format carries strings.
fn objectify(message: &Value) -> Option<Value> {
    let mut message = crate::llm::messages::canonical(message);
    // The execution journal carries a typed error flag. Chat training templates consume the
    // tool's content and call id; drop only a valid flag, retaining malformed/unknown fields so
    // the training validator still rejects them rather than silently repairing the example.
    if message["role"] == "tool" && message["is_error"].is_boolean() {
        message.as_object_mut()?.remove("is_error");
    }
    if let Some(calls) = message.get_mut("tool_calls") {
        for call in calls.as_array_mut()? {
            let arguments = call.pointer_mut("/function/arguments")?;
            if let Some(text) = arguments.as_str() {
                *arguments = serde_json::from_str::<Value>(text)
                    .ok()
                    .filter(Value::is_object)?;
            }
        }
    }
    Some(message)
}

fn build(payload: &Value, trajectory: i64) -> Option<Value> {
    let mut messages = payload.get("context")?.as_array()?.clone();
    messages.push(payload.get("corrected")?.clone());
    let (clean, _) = clean_path(&messages);
    let mut messages = clean
        .into_iter()
        .map(objectify)
        .collect::<Option<Vec<_>>>()?;
    strip_reasoning(&mut messages, true);
    Some(render(
        messages,
        payload.get("tools")?,
        format!("dial:tool-recovery:{trajectory}"),
    ))
}

/// Reasoning is a training target only on a record's final assistant message; earlier assistant
/// turns stay plain context, the way chat templates replay history at inference.
fn strip_reasoning(messages: &mut [Value], keep_final: bool) {
    let last = messages
        .iter()
        .rposition(|m| m["role"] == "assistant")
        .filter(|_| keep_final);
    for (i, message) in messages.iter_mut().enumerate() {
        if Some(i) != last
            && message["role"] == "assistant"
            && let Some(object) = message.as_object_mut()
        {
            object.remove("reasoning_content");
        }
    }
}

/// Provenance allowlist: auto steering only consumes train-split pairs matching a
/// recorded successful tool recovery, never arbitrary preference JSON in a data file.
pub(crate) fn steering_pairs(store: &Store) -> Result<HashSet<String>> {
    let mut pairs = HashSet::new();
    for (trajectory, payload) in store.tool_recoveries()? {
        if let Some(record) = build(&payload, trajectory) {
            for pair in verified_preference_pairs(&payload, &record) {
                pairs.insert(serde_json::to_string(&pair)?);
            }
        }
    }
    Ok(pairs)
}

fn verified_preference_pairs(payload: &Value, record: &Value) -> Vec<Value> {
    let Some(tools) = payload
        .get("tools")
        .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
    else {
        return Vec::new();
    };
    let Some(context) = payload.get("context").and_then(Value::as_array) else {
        return Vec::new();
    };
    let (clean, _) = clean_path(context);
    let Some(mut context) = clean.into_iter().map(objectify).collect::<Option<Vec<_>>>() else {
        return Vec::new();
    };
    // Pairs contrast completions only; reasoning is stripped from both sides and the context.
    strip_reasoning(&mut context, false);
    if context.is_empty()
        || !context.iter().all(well_formed)
        || !context.iter().any(|m| m["role"] == "user")
    {
        return Vec::new();
    }
    let Some(mut chosen) = record["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
    else {
        return Vec::new();
    };
    strip_reasoning(std::slice::from_mut(&mut chosen), false);
    if chosen["role"] != "assistant" || !well_formed(&chosen) {
        return Vec::new();
    }
    let Some(rejected) = payload.get("rejected").and_then(Value::as_array) else {
        return Vec::new();
    };
    rejected
        .iter()
        .take(3)
        .filter_map(|message| {
            let mut rejected = objectify(message)?;
            strip_reasoning(std::slice::from_mut(&mut rejected), false);
            (rejected["role"] == "assistant" && well_formed(&rejected)).then(|| {
                json!({
                    "context": context, "tools": tools, "chosen": chosen, "rejected": rejected
                })
            })
        })
        .collect()
}

/// The shape the python worker accepts; anything else would abort a whole training run.
fn well_formed(message: &Value) -> bool {
    let Some(object) = message.as_object() else {
        return false;
    };
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    let only = |allowed: &[&str]| object.keys().all(|k| allowed.contains(&k.as_str()));
    let reasoning = || {
        object
            .get("reasoning_content")
            .is_none_or(|r| r.as_str().is_some_and(|r| !r.trim().is_empty()))
    };
    match text("role") {
        Some("assistant") if object.contains_key("tool_calls") => {
            only(&["role", "content", "tool_calls", "reasoning_content"])
                && reasoning()
                && object
                    .get("content")
                    .is_none_or(|c| c.is_null() || c.is_string())
                && object["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty() && calls.iter().all(well_formed_call))
        }
        Some("assistant") => {
            only(&["role", "content", "reasoning_content"])
                && reasoning()
                && text("content").is_some_and(|c| !c.trim().is_empty())
        }
        Some("tool") => {
            object.len() == 3
                && only(&["role", "content", "tool_call_id"])
                && text("content").is_some()
                && text("tool_call_id").is_some_and(|id| !id.is_empty())
        }
        Some("system" | "user") => {
            object.len() == 2 && text("content").is_some_and(|c| !c.trim().is_empty())
        }
        _ => false,
    }
}

fn well_formed_call(call: &Value) -> bool {
    let (Some(call), Some(function)) = (
        call.as_object(),
        call.get("function").and_then(Value::as_object),
    ) else {
        return false;
    };
    call.len() == 3
        && call
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && call.get("type").is_some_and(|t| t == "function")
        && function.len() == 2
        && function
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| !n.is_empty())
        && function.get("arguments").is_some_and(Value::is_object)
}

fn is_correction(message: &Value) -> bool {
    message["role"] == "user"
        && message["content"]
            .as_str()
            .is_some_and(|c| CORRECTIONS.iter().any(|p| c.trim_start().starts_with(p)))
}

/// A tool answer that shows the call itself was bad (not merely that the work failed).
fn is_rejected_call(message: &Value) -> bool {
    message["role"] == "tool"
        && message["content"].as_str().is_some_and(|c| {
            c.trim_start().starts_with("invalid tool call:") || c.trim_start().starts_with("denied")
        })
}

/// The conversation without form failures: correction messages, the assistant turns they answered,
/// and the tool messages that answered rejected calls (plus the turns issuing them). Returns the
/// clean path and the number of assistant turns removed.
fn clean_path(messages: &[Value]) -> (Vec<&Value>, usize) {
    let mut removed: Vec<bool> = messages.iter().map(is_correction).collect();
    let mut skipped = 0;
    for (i, message) in messages.iter().enumerate() {
        if message["role"] != "assistant" {
            continue;
        }
        let answers = messages[i + 1..]
            .iter()
            .take_while(|m| m["role"] == "tool")
            .count();
        let bad = messages.get(i + 1).is_some_and(is_correction)
            || messages[i + 1..i + 1 + answers]
                .iter()
                .any(is_rejected_call);
        if bad {
            removed[i..=i + answers].fill(true);
            skipped += 1;
        }
    }
    (
        messages
            .iter()
            .zip(&removed)
            .filter(|(_, r)| !**r)
            .map(|(m, _)| m)
            .collect(),
        skipped,
    )
}

/// At most `cap` indices spread evenly over `keep`, always including the last (the final answer).
fn sample(keep: &[usize], cap: usize) -> Vec<usize> {
    let Some((&last, rest)) = keep.split_last() else {
        return Vec::new();
    };
    if keep.len() <= cap {
        return keep.to_vec();
    }
    let mut picked: Vec<usize> = (0..cap - 1)
        .map(|j| rest[j * rest.len() / (cap - 1)])
        .collect();
    picked.push(last);
    picked
}

#[allow(clippy::too_many_arguments)]
fn success_candidates(
    trajectory: i64,
    domain: &str,
    source: &str,
    payload: &Value,
    held_out: &HashSet<String>,
    max_tokens: usize,
    report: &mut Report,
    origin: Option<&str>,
) -> Vec<Candidate> {
    // Sibling records keep their origin in the source so provenance and trajectory ids stay distinct.
    let provenance = |inner: &str| match origin {
        Some(origin) => format!("dial:learned:{origin}:{trajectory}:{inner}"),
        None => format!("dial:verified:{trajectory}:{inner}"),
    };
    // New capture format: each snapshot has exactly one target and its actual request.
    // Do not relabel previous conversation turns or rewrite the context seen at inference.
    if let Some(examples) = payload.get("examples") {
        let Some(examples) = examples.as_array() else {
            report.skipped_invalid += 1;
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for example in examples {
            let Some(messages) = example["messages"].as_array() else {
                report.skipped_invalid += 1;
                continue;
            };
            let Some(tools) = example.get("tools").filter(|t| t.is_array()) else {
                report.skipped_invalid += 1;
                continue;
            };
            let Some(mut messages) = messages.iter().map(objectify).collect::<Option<Vec<_>>>()
            else {
                report.skipped_invalid += 1;
                continue;
            };
            strip_reasoning(&mut messages, origin.is_none());
            let mut screen = Screen::new(held_out, max_tokens, tools);
            for message in &messages {
                screen.push(message);
            }
            if !report.tally(screen.verdict()) {
                continue;
            }
            let record = render(messages, tools, provenance(source));
            let Some(group) = prompt_group(&record["messages"]) else {
                report.skipped_invalid += 1;
                continue;
            };
            candidates.push(Candidate {
                digest: digest(&record["messages"]),
                line: format!("{record}\n"),
                group,
                family: domain.to_owned(),
                repair: false,
                preference_pairs: Vec::new(),
                origin: origin.map(str::to_owned),
            });
        }
        let keep = sample(&(0..candidates.len()).collect::<Vec<_>>(), PER_TRAJECTORY);
        return candidates
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
            .map(|(_, c)| c)
            .collect();
    }
    let tools = payload.get("tools").cloned().unwrap_or_else(|| json!([]));
    let Some(messages) = payload
        .get("messages")
        .and_then(Value::as_array)
        .filter(|_| tools.is_array())
    else {
        report.skipped_invalid += 1;
        return Vec::new();
    };
    let (clean, skipped) = clean_path(messages);
    report.skipped_form_failure_turns += skipped;
    let converted: Vec<Option<Value>> = clean.iter().map(|m| objectify(m)).collect();
    let mut screen = Screen::new(held_out, max_tokens, &tools);
    let mut keep = Vec::new();
    for (i, message) in clean.iter().enumerate() {
        match &converted[i] {
            Some(message) => screen.push(message),
            None => {
                screen.push(message);
                screen.malformed = true;
            }
        }
        if message["role"] == "assistant" && report.tally(screen.verdict()) {
            keep.push(i);
        }
    }
    sample(&keep, PER_TRAJECTORY)
        .into_iter()
        .filter_map(|target| {
            // A kept target implies every message before it converted.
            let mut messages: Vec<Value> = converted[..=target].iter().flatten().cloned().collect();
            strip_reasoning(&mut messages, origin.is_none());
            let record = render(messages, &tools, provenance(source));
            let digest = digest(&record["messages"]);
            Some(Candidate {
                line: format!("{record}\n"),
                digest,
                group: prompt_group(&record["messages"])?,
                family: domain.to_owned(),
                repair: false,
                preference_pairs: Vec::new(),
                origin: origin.map(str::to_owned),
            })
        })
        .collect()
}

/// With more than `BALANCE_ABOVE` families, trim the biggest to the largest common size at which no
/// family exceeds `FAMILY_PERCENT` of the total, keeping the lowest digests (deterministic).
fn balance(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut families: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates {
        families
            .entry(candidate.family.clone())
            .or_default()
            .push(candidate);
    }
    let largest = families.values().map(Vec::len).max().unwrap_or(0);
    let total = |cap: usize| families.values().map(|f| f.len().min(cap)).sum::<usize>();
    let cap = if families.len() > BALANCE_ABOVE {
        (1..=largest)
            .rev()
            .find(|&cap| cap * 100 <= FAMILY_PERCENT * total(cap))
            .unwrap_or(1)
    } else {
        largest
    };
    families
        .into_values()
        .flat_map(|mut family| {
            family.sort_by(|a, b| a.digest.cmp(&b.digest));
            family.truncate(cap);
            family
        })
        .collect()
}

/// Byte-level JSONL recovery shared by digest scanning and append. Returns where the
/// unterminated final fragment starts, and whether it is torn (not a complete JSON record).
/// `None` when the bytes are empty or already end with a newline.
fn unterminated_tail(bytes: &[u8]) -> Option<(usize, bool)> {
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return None;
    }
    let start = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let complete = serde_json::from_slice::<Value>(&bytes[start..]).is_ok();
    Some((start, !complete))
}

fn digests(path: &Path) -> Result<(HashSet<String>, usize, HashSet<String>)> {
    let mut seen = HashSet::new();
    let mut groups = HashSet::new();
    let mut count = 0;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((seen, 0, groups)),
        Err(error) => return Err(error).context("reading existing training data"),
    };
    // An interrupted append leaves a torn final record. `append` truncates it before writing, so
    // it is skipped here instead of blocking every future harvest. Interior records stay strict.
    let end = match unterminated_tail(&bytes) {
        Some((start, true)) => start,
        _ => bytes.len(),
    };
    for line in bytes[..end].split(|&b| b == b'\n') {
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let record: Value =
            serde_json::from_slice(line).context("existing training data is not valid JSONL")?;
        count += 1;
        if let Some(messages) = record.get("messages") {
            seen.insert(digest(messages));
            if let Some(group) = prompt_group(messages) {
                groups.insert(group);
            }
        }
        if let Some(group) = record["source"].as_str().and_then(trajectory_group) {
            groups.insert(group);
        }
    }
    Ok((seen, count, groups))
}

fn append(path: &Path, lines: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    if !lines.is_empty() && file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            // Either a complete record missing its newline, or a torn append from a crash. Keep the
            // former; truncate the latter so it never ends up mid-file where digests would reject it.
            let text = fs::read(path)?;
            match unterminated_tail(&text) {
                Some((start, true)) => file.set_len(start as u64)?,
                Some((_, false)) => file.write_all(b"\n")?,
                None => {}
            }
        }
    }
    file.write_all(lines.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests;
