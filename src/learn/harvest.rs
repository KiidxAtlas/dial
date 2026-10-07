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
    path::Path,
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
    pub skipped_overlap: usize,
    pub skipped_oversize: usize,
    pub skipped_invalid: usize,
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
        link_group(&mut links, &group, trajectory);
        let preference_pairs = verified_preference_pairs(&payload, &record);
        let candidate = Candidate {
            line: format!("{}\n", render_record(&record)),
            digest: digest.clone(),
            group,
            family: RECOVERY_FAMILY.into(),
            repair: true,
            preference_pairs,
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
        ) {
            link_group(&mut links, &candidate.group, trajectory);
            candidates
                .entry(candidate.digest.clone())
                .or_insert(candidate);
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
    for mut candidate in balance(candidates.into_values().collect()) {
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
        }
    }

    fn push(&mut self, message: &Value) {
        self.bytes += message.to_string().len();
        self.count += 1;
        self.malformed |= !well_formed(message);
        self.last_assistant = message["role"] == "assistant";
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
    let rest = source
        .strip_prefix("dial:verified:")
        .or_else(|| source.strip_prefix("dial:tool-recovery:"))?;
    let id = rest.split(':').next()?.parse::<i64>().ok()?;
    Some(format!("trajectory:{id}"))
}

fn link_group(links: &mut BTreeMap<String, HashSet<String>>, prompt: &str, trajectory: i64) {
    let trajectory = format!("trajectory:{trajectory}");
    links
        .entry(prompt.to_owned())
        .or_default()
        .insert(trajectory.clone());
    links
        .entry(trajectory)
        .or_default()
        .insert(prompt.to_owned());
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
    let messages = clean
        .into_iter()
        .map(objectify)
        .collect::<Option<Vec<_>>>()?;
    Some(render(
        messages,
        payload.get("tools")?,
        format!("dial:tool-recovery:{trajectory}"),
    ))
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
    let Some(context) = clean.into_iter().map(objectify).collect::<Option<Vec<_>>>() else {
        return Vec::new();
    };
    if context.is_empty()
        || !context.iter().all(well_formed)
        || !context.iter().any(|m| m["role"] == "user")
    {
        return Vec::new();
    }
    let Some(chosen) = record["messages"].as_array().and_then(|m| m.last()) else {
        return Vec::new();
    };
    if chosen["role"] != "assistant" || !well_formed(chosen) {
        return Vec::new();
    }
    let Some(rejected) = payload.get("rejected").and_then(Value::as_array) else {
        return Vec::new();
    };
    rejected
        .iter()
        .take(3)
        .filter_map(|message| {
            let rejected = objectify(message)?;
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
    match text("role") {
        Some("assistant") if object.contains_key("tool_calls") => {
            only(&["role", "content", "tool_calls"])
                && object
                    .get("content")
                    .is_none_or(|c| c.is_null() || c.is_string())
                && object["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty() && calls.iter().all(well_formed_call))
        }
        Some("tool") => {
            object.len() == 3
                && only(&["role", "content", "tool_call_id"])
                && text("content").is_some()
                && text("tool_call_id").is_some_and(|id| !id.is_empty())
        }
        Some("system" | "user" | "assistant") => {
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

fn success_candidates(
    trajectory: i64,
    domain: &str,
    source: &str,
    payload: &Value,
    held_out: &HashSet<String>,
    max_tokens: usize,
    report: &mut Report,
) -> Vec<Candidate> {
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
            let Some(messages) = messages.iter().map(objectify).collect::<Option<Vec<_>>>() else {
                report.skipped_invalid += 1;
                continue;
            };
            let mut screen = Screen::new(held_out, max_tokens, tools);
            for message in &messages {
                screen.push(message);
            }
            if !report.tally(screen.verdict()) {
                continue;
            }
            let record = render(
                messages,
                tools,
                format!("dial:verified:{trajectory}:{source}"),
            );
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
            let messages: Vec<Value> = converted[..=target].iter().flatten().cloned().collect();
            let record = render(
                messages,
                &tools,
                format!("dial:verified:{trajectory}:{source}"),
            );
            let digest = digest(&record["messages"]);
            Some(Candidate {
                line: format!("{record}\n"),
                digest,
                group: prompt_group(&record["messages"])?,
                family: domain.to_owned(),
                repair: false,
                preference_pairs: Vec::new(),
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

fn digests(path: &Path) -> Result<(HashSet<String>, usize, HashSet<String>)> {
    let mut seen = HashSet::new();
    let mut groups = HashSet::new();
    let mut count = 0;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((seen, 0, groups)),
        Err(error) => return Err(error).context("reading existing training data"),
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        count += 1;
        let record: Value =
            serde_json::from_str(line).context("existing training data is not valid JSONL")?;
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
            file.write_all(b"\n")?;
        }
    }
    file.write_all(lines.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Outcome;
    use std::path::PathBuf;

    const CALL_FAILURE: &str = "That looked like a tool call written as plain text, so nothing ran. Call the tool through the tool-calling interface, or answer in words.";
    const DROPPED: &str = "Your tool call was unreadable JSON. Issue it again with valid arguments, or answer directly.";

    fn system() -> Value {
        json!({"role":"system","content":"sys"})
    }
    fn user(text: &str) -> Value {
        json!({"role":"user","content":text})
    }
    fn say(text: &str) -> Value {
        json!({"role":"assistant","content":text})
    }
    fn call(id: &str, path: &str) -> Value {
        json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":"read","arguments":format!("{{\"path\":\"{path}\"}}")}}]})
    }
    fn answer(id: &str, text: &str) -> Value {
        json!({"role":"tool","tool_call_id":id,"content":text})
    }
    fn schemas() -> Value {
        json!([{"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}}])
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
        config: Config,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(
                &dir.path().join("state"),
                "model",
                "case",
                &Default::default(),
            )
            .unwrap();
            let mut config = Config::default();
            config.training.train_data = dir.path().join("data/train.jsonl");
            config.training.valid_data = dir.path().join("data/valid.jsonl");
            config.training.batch_size = 1;
            config.training.max_seq_length = 4096;
            Self { dir, store, config }
        }

        fn record(&self, domain: &str, score: f64, messages: Vec<Value>) -> i64 {
            let version = self.store.active().unwrap().id;
            let id = self
                .store
                .begin_trajectory(version, domain, "task", "prompt")
                .unwrap();
            self.store
                .finish_trajectory(id, Some("done"), None)
                .unwrap();
            self.store
                .outcome(&Outcome {
                    trajectory: id,
                    score,
                    source: "verifier:test".into(),
                    reference: None,
                })
                .unwrap();
            self.store
                .save_messages(id, &json!({"messages":messages,"tools":schemas()}))
                .unwrap();
            id
        }

        fn run(&self) -> Report {
            self.run_with(&HashSet::new())
        }
        fn run_with(&self, held_out: &HashSet<String>) -> Report {
            harvest(&self.config, &self.store, held_out).unwrap()
        }

        /// All harvested records, train then valid.
        fn records(&self) -> Vec<Value> {
            let t = &self.config.training;
            [&t.train_data, &t.valid_data]
                .iter()
                .flat_map(|p: &&PathBuf| {
                    fs::read_to_string(p)
                        .unwrap_or_default()
                        .lines()
                        .map(|l| serde_json::from_str(l).unwrap())
                        .collect::<Vec<Value>>()
                })
                .collect()
        }
    }

    fn roles(record: &Value) -> Vec<&str> {
        record["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn arguments_become_objects_and_bad_json_is_rejected() {
        let payload = json!({"context":[{"role":"system","content":"s"},{"role":"user","content":"u"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]},
            {"role":"tool","tool_call_id":"1","content":"x"}],
            "corrected":{"role":"assistant","content":null,"tool_calls":[{"id":"2","type":"function","function":{"name":"read","arguments":"{\"path\":\"b\"}"}}]},
            "tools":[]});
        let record = build(&payload, 7).unwrap();
        assert_eq!(
            record["messages"][4]["tool_calls"][0]["function"]["arguments"],
            json!({"path":"b"})
        );
        assert_eq!(
            record["messages"][2]["tool_calls"][0]["function"]["arguments"],
            json!({"path":"a"})
        );
        assert_eq!(record["source"], "dial:tool-recovery:7");
        let mut bad = payload.clone();
        bad["corrected"]["tool_calls"][0]["function"]["arguments"] = json!("not json");
        assert!(build(&bad, 7).is_none());
    }

    #[test]
    fn harness_metadata_is_removed_from_successes_and_repairs_without_relaxing_validation() {
        let f = Fixture::new();
        let mut observed = answer("1", "evidence");
        observed["dial_evidence"] = json!({"artifact":"artifact://proof"});
        f.record(
            "live",
            100.0,
            vec![
                system(),
                user("inspect metadata"),
                call("1", "a"),
                observed.clone(),
                say("done"),
            ],
        );
        let report = f.run();
        assert_eq!(report.from_successes, 2);
        let records = f.records();
        assert!(
            records
                .iter()
                .all(|r| !r.to_string().contains("dial_evidence"))
        );
        let payload = json!({"context":[system(),user("repair"),call("1","a"),observed],
            "corrected":call("2","b"),"tools":schemas()});
        let record = build(&payload, 9).unwrap();
        assert!(
            record["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(well_formed)
        );
        assert_eq!(record["messages"][3]["content"], "evidence");
        let mut unknown = answer("1", "evidence");
        unknown["unknown"] = json!(true);
        assert!(!well_formed(&objectify(&unknown).unwrap()));
        let mut image = answer("1", "evidence");
        image["dial_images"] = json!([{"path":"image.png"}]);
        assert!(!well_formed(&objectify(&image).unwrap()));
    }

    #[test]
    fn request_snapshots_preserve_context_and_never_relabel_previous_turns() {
        let f = Fixture::new();
        let id = f.record(
            "live",
            100.0,
            vec![system(), user("placeholder"), say("legacy")],
        );
        let messages = vec![
            system(),
            user("original constraints"),
            say("old answer"),
            user("continue"),
            call("1", "a"),
            answer("1", "actual evidence"),
            say("new answer"),
        ];
        f.store
            .save_messages(
                id,
                &json!({"examples":[{"messages":messages,"tools":schemas()}]}),
            )
            .unwrap();
        assert_eq!(f.run().from_successes, 1);
        let records = f.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["messages"][2]["content"], "old answer");
        assert_eq!(records[0]["messages"][6]["content"], "new answer");
        assert_eq!(
            records[0]["messages"][4]["tool_calls"][0]["function"]["arguments"],
            json!({"path":"a"})
        );
        assert_eq!(f.run().added(), 0);
    }

    #[test]
    fn request_snapshots_keep_heldout_and_size_checks() {
        let f = Fixture::new();
        let id = f.record(
            "live",
            100.0,
            vec![system(), user("placeholder"), say("legacy")],
        );
        f.store.save_messages(id, &json!({"examples":[
            {"messages":[system(),user("sealed task"),say("old"),user("continue"),say("new")],"tools":schemas()},
            {"messages":[system(),user("large"),say(&"x".repeat(10000))],"tools":schemas()},
            {"messages":[system(),user("bad"),{"role":"assistant","content":"new","unknown":true}],"tools":schemas()}
        ]})).unwrap();
        let report = f.run_with(&HashSet::from(["sealed task".into()]));
        assert_eq!(report.added(), 0);
        assert_eq!(report.skipped_overlap, 1);
        assert_eq!(report.skipped_oversize, 1);
        assert_eq!(report.skipped_invalid, 1);
    }

    #[test]
    fn clean_success_yields_one_record_per_assistant_turn_with_loss_last() {
        let f = Fixture::new();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user("read a"),
                call("c1", "a"),
                answer("c1", "text"),
                say("it says text"),
            ],
        );
        let report = f.run();
        assert_eq!(
            (report.from_successes, report.from_repairs, report.added()),
            (2, 0, 2)
        );
        let mut records = f.records();
        records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
        assert_eq!(roles(&records[0]), ["system", "user", "assistant"]);
        assert_eq!(
            roles(&records[1]),
            ["system", "user", "assistant", "tool", "assistant"]
        );
        assert_eq!(
            records[0]["messages"][2]["tool_calls"][0]["function"]["arguments"],
            json!({"path":"a"})
        );
        assert_eq!(records[1]["messages"][4]["content"], "it says text");
        assert_eq!(
            records[1]["source"],
            format!("dial:verified:{id}:verifier:test")
        );
        assert_eq!(records[1]["verified"], true);
        assert_eq!(records[1]["tools"], schemas());
    }

    #[test]
    fn failed_outcomes_are_not_learned_from() {
        let f = Fixture::new();
        f.record("selfplay:files", 99.0, vec![system(), user("q"), say("a")]);
        assert_eq!(f.run().added(), 0);
    }

    #[test]
    fn form_failures_and_their_corrections_are_removed_from_later_contexts() {
        let f = Fixture::new();
        let text_call = say("{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}");
        f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user("read a"),
                text_call.clone(),
                user(CALL_FAILURE),
                user(DROPPED),
                call("c1", "a"),
                answer("c1", "text"),
                say("done"),
            ],
        );
        let report = f.run();
        assert_eq!(report.skipped_form_failure_turns, 1);
        assert_eq!(report.added(), 2);
        for record in f.records() {
            let text = record.to_string();
            assert!(
                !text.contains("plain text")
                    && !text.contains("unreadable")
                    && !text.contains("\\\"arguments\\\""),
                "{text}"
            );
            assert_eq!(record["messages"][2]["role"], "assistant");
            assert_eq!(record["messages"][2]["tool_calls"][0]["id"], "c1");
        }
    }

    #[test]
    fn verified_json_answers_are_not_mistaken_for_failed_tool_calls() {
        let f = Fixture::new();
        let answer = "{\"arguments\": {\"count\": 3}}";
        f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user("Return JSON describing the arguments"),
                say(answer),
            ],
        );
        let report = f.run();
        assert_eq!((report.skipped_form_failure_turns, report.added()), (0, 1));
        assert_eq!(f.records()[0]["messages"][2]["content"], answer);
    }

    #[test]
    fn denied_and_invalid_calls_drop_their_turn_but_runtime_errors_are_kept() {
        let f = Fixture::new();
        f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user("go"),
                call("c1", "x"),
                answer("c1", "denied: read-only mode; this tool changes things."),
                call("c2", "y"),
                answer("c2", "invalid tool call: missing required argument path"),
                call("c3", "z"),
                answer("c3", "reading \"z\": no such file or directory"),
                call("c4", "a"),
                answer("c4", "fine"),
                say("done"),
            ],
        );
        let report = f.run();
        assert_eq!(report.skipped_form_failure_turns, 2);
        let mut records = f.records();
        records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
        assert_eq!(records.len(), 3);
        // c3 (recovered runtime error) -> c4 -> final answer, with the rejected calls gone from every context.
        assert_eq!(roles(&records[0]), ["system", "user", "assistant"]);
        assert_eq!(records[0]["messages"][2]["tool_calls"][0]["id"], "c3");
        assert_eq!(
            roles(&records[2]),
            [
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "tool",
                "assistant"
            ]
        );
        for record in &records {
            let text = record.to_string();
            assert!(
                !text.contains("denied") && !text.contains("invalid tool call"),
                "{text}"
            );
        }
    }

    #[test]
    fn session_domains_need_learn_from_sessions() {
        let mut f = Fixture::new();
        f.record("coding", 100.0, vec![system(), user("live"), say("answer")]);
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("synthetic"), say("answer")],
        );
        f.config.improve.learn_from_sessions = false;
        assert_eq!(f.run().added(), 1);
        assert!(
            f.records()
                .iter()
                .all(|r| r["messages"][1]["content"] == "synthetic")
        );
        f.config.improve.learn_from_sessions = true;
        assert_eq!(f.run().added(), 1);
        assert_eq!(f.records().len(), 2);
    }

    #[test]
    fn disabling_live_learning_also_excludes_tool_repairs() {
        let mut f = Fixture::new();
        let id = f.record(
            "coding",
            0.0,
            vec![system(), user("private live task"), say("failed")],
        );
        f.store
            .tool_recovery(
                id,
                f.store.active().unwrap().id,
                &json!({
                    "context":[system(), user("private live task")], "tools":schemas(),
                    "corrected":call("fixed", "a"), "failure_kinds":["schema"]
                }),
            )
            .unwrap();
        f.config.improve.learn_from_sessions = false;
        assert_eq!(f.run().added(), 0);
        f.config.improve.learn_from_sessions = true;
        assert_eq!(f.run().from_repairs, 1);
        assert_eq!(
            f.records()[0]["messages"][1]["content"],
            "private live task"
        );
    }

    #[test]
    fn held_out_prompts_are_never_trained_on() {
        let f = Fixture::new();
        f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user("  secret exam question \n"),
                call("c1", "a"),
                answer("c1", "x"),
                say("done"),
            ],
        );
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("ordinary"), say("fine")],
        );
        let report = f.run_with(&HashSet::from(["secret exam question".to_owned()]));
        assert_eq!((report.skipped_overlap, report.added()), (2, 1));
        assert!(
            f.records()
                .iter()
                .all(|r| !r.to_string().contains("secret exam"))
        );
    }

    #[test]
    fn oversize_records_are_skipped_not_truncated() {
        let mut f = Fixture::new();
        f.config.training.max_seq_length = 600;
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("short"), say("ok")],
        );
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("long"), say(&"w ".repeat(2000))],
        );
        let report = f.run();
        assert_eq!((report.skipped_oversize, report.added()), (1, 1));
    }

    #[test]
    fn trajectories_are_capped_keeping_the_final_answer() {
        let f = Fixture::new();
        let mut messages = vec![system(), user("many steps")];
        for i in 0..20 {
            messages.push(call(&format!("c{i}"), "a"));
            messages.push(answer(&format!("c{i}"), "ok"));
        }
        messages.push(say("the end"));
        f.record("selfplay:files", 100.0, messages);
        assert_eq!(f.run().added(), PER_TRAJECTORY);
        let records = f.records();
        assert!(
            records
                .iter()
                .any(|r| r["messages"].as_array().unwrap().last().unwrap()["content"] == "the end")
        );
        assert_eq!(
            records
                .iter()
                .map(|r| r["messages"].to_string())
                .collect::<HashSet<_>>()
                .len(),
            PER_TRAJECTORY
        );
    }

    #[test]
    fn one_family_cannot_dominate_and_reruns_add_nothing() {
        let f = Fixture::new();
        let mut big = HashSet::new();
        for i in 0..6 {
            big.insert(f.record(
                "selfplay:big",
                100.0,
                vec![system(), user(&format!("big {i}")), say("a")],
            ));
        }
        for family in ["b", "c", "d"] {
            for i in 0..3 {
                f.record(
                    &format!("selfplay:{family}"),
                    100.0,
                    vec![system(), user(&format!("{family} {i}")), say("a")],
                );
            }
        }
        let first = f.run();
        // Largest common size c with c <= 35% of sum(min(size, c)): 4 of 13.
        assert_eq!(first.added(), 13);
        let from_big = f
            .records()
            .iter()
            .filter(|r| {
                let source = r["source"].as_str().unwrap();
                big.iter()
                    .any(|id| source.starts_with(&format!("dial:verified:{id}:")))
            })
            .count();
        assert_eq!(from_big, 4);
        let (train, valid) = (
            fs::read_to_string(&f.config.training.train_data).unwrap(),
            fs::read_to_string(&f.config.training.valid_data).unwrap(),
        );
        let second = f.run();
        assert_eq!(second.added(), 0);
        assert_eq!(
            (second.total_train, second.total_valid),
            (first.total_train, first.total_valid)
        );
        assert_eq!(
            fs::read_to_string(&f.config.training.train_data).unwrap(),
            train
        );
        assert_eq!(
            fs::read_to_string(&f.config.training.valid_data).unwrap(),
            valid
        );
        assert_eq!(
            f.store.get_meta("harvested_total").unwrap().as_deref(),
            Some("13")
        );
    }

    fn recovery_payload(prompt: &str, rejected: Option<Vec<Value>>) -> Value {
        let mut payload = json!({
            "context":[system(), user(prompt)],
            "tools":schemas(), "corrected":call("chosen", "a"), "failure_kinds":["schema"]
        });
        if let Some(rejected) = rejected {
            payload["rejected"] = json!(rejected);
        }
        payload
    }

    #[test]
    fn verified_repair_pair_is_written_only_to_its_train_split() {
        let f = Fixture::new();
        let prompt = (0..100)
            .map(|i| format!("pair prompt {i}"))
            .find(|p| {
                let group = prompt_group(&json!([user(p)])).unwrap();
                !u8::from_str_radix(&group[..2], 16)
                    .unwrap()
                    .is_multiple_of(5)
            })
            .unwrap();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![system(), user(&prompt), say("done")],
        );
        f.store
            .tool_recovery(
                id,
                f.store.active().unwrap().id,
                &recovery_payload(&prompt, Some(vec![say("malformed attempt")])),
            )
            .unwrap();
        f.run();
        let train: Vec<Value> = fs::read_to_string(&f.config.training.train_data)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let valid = fs::read_to_string(&f.config.training.valid_data).unwrap_or_default();
        let recovery = train
            .iter()
            .find(|r| r["source"] == format!("dial:tool-recovery:{id}"))
            .unwrap();
        assert_eq!(
            recovery["preference_pairs"][0]["chosen"],
            objectify(&call("chosen", "a")).unwrap()
        );
        assert_eq!(
            recovery["preference_pairs"][0]["rejected"],
            say("malformed attempt")
        );
        assert!(!valid.contains("preference_pairs"));
    }

    #[test]
    fn missing_rejected_attempt_and_held_out_recovery_produce_no_pairs() {
        let f = Fixture::new();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("unpaired"), say("done")],
        );
        f.store
            .tool_recovery(
                id,
                f.store.active().unwrap().id,
                &recovery_payload("unpaired", None),
            )
            .unwrap();
        f.run();
        assert!(
            f.records()
                .iter()
                .filter(|r| r["source"] == format!("dial:tool-recovery:{id}"))
                .all(|r| r.get("preference_pairs").is_none())
        );

        let f = Fixture::new();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("sealed"), say("done")],
        );
        f.store
            .tool_recovery(
                id,
                f.store.active().unwrap().id,
                &recovery_payload("sealed", Some(vec![say("malformed attempt")])),
            )
            .unwrap();
        f.run_with(&HashSet::from(["sealed".to_owned()]));
        assert!(
            !f.records()
                .iter()
                .any(|r| r["source"] == format!("dial:tool-recovery:{id}"))
        );
    }

    #[test]
    fn malformed_context_or_rejected_output_produces_no_pairs() {
        let mut payload = recovery_payload("overlap", Some(vec![say("bad")]));
        payload["context"] = json!([system(), user("overlap"), {"role":"assistant","content":""}]);
        let record = build(&payload, 1).unwrap();
        assert!(record.get("preference_pairs").is_none());

        let payload = recovery_payload(
            "invalid",
            Some(vec![json!({"role":"assistant","tool_calls":[]} )]),
        );
        let record = build(&payload, 2).unwrap();
        assert!(record.get("preference_pairs").is_none());
    }

    #[test]
    fn overlapping_recovery_does_not_attach_a_preference_pair() {
        let f = Fixture::new();
        let prompt = "already trained repair";
        let recovery_record = render(
            vec![
                system(),
                user(prompt),
                objectify(&call("chosen", "a")).unwrap(),
            ],
            &schemas(),
            "existing".into(),
        );
        fs::create_dir_all(f.config.training.train_data.parent().unwrap()).unwrap();
        fs::write(
            &f.config.training.train_data,
            format!("{recovery_record}\n"),
        )
        .unwrap();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![system(), user(prompt), say("done")],
        );
        f.store
            .tool_recovery(
                id,
                f.store.active().unwrap().id,
                &recovery_payload(prompt, Some(vec![say("malformed attempt")])),
            )
            .unwrap();
        f.run();
        assert!(
            f.records()
                .iter()
                .all(|record| record.get("preference_pairs").is_none())
        );
    }
    #[test]
    fn repairs_and_successes_share_a_clean_deduplicated_pipeline() {
        let f = Fixture::new();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user("read a"),
                call("bad", "missing"),
                answer("bad", "invalid tool call: missing path"),
                call("c2", "a"),
                answer("c2", "text"),
                say("done"),
            ],
        );
        let version = f.store.active().unwrap().id;
        f.store
            .tool_recovery(
                id,
                version,
                &json!({
                    "context":[system(), user("read a"), call("bad", "missing"),
                        answer("bad", "invalid tool call: missing path")],
                    "tools":schemas(), "corrected":call("c2", "a"), "failure_kinds":["schema"]
                }),
            )
            .unwrap();
        let report = f.run();
        assert_eq!(
            (report.from_repairs, report.from_successes, report.added()),
            (1, 1, 2)
        );
        let mut records = f.records();
        records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
        assert_eq!(
            records[0]["messages"],
            json!([
                system(),
                user("read a"),
                objectify(&call("c2", "a")).unwrap()
            ])
        );
        assert_eq!(records[1]["messages"][4]["content"], "done");
        assert_eq!(f.run().added(), 0);
    }

    #[test]
    fn command_exit_codes_do_not_make_successful_training_targets() {
        let f = Fixture::new();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("repair"), say("claimed done")],
        );
        f.store
            .outcome(&Outcome {
                trajectory: id,
                score: 100.0,
                source: "check:exit0".into(),
                reference: None,
            })
            .unwrap();
        assert_eq!(f.run().added(), 0);
        f.store
            .outcome(&Outcome {
                trajectory: id,
                score: 100.0,
                source: "user".into(),
                reference: None,
            })
            .unwrap();
        assert_eq!(f.run().added(), 1);
    }

    #[test]
    fn prefixes_and_later_same_prompt_trajectories_keep_their_split() {
        let f = Fixture::new();
        // Pick an ordinary training group, then verify that the validation floor cannot split it.
        let prompt = (0..100)
            .map(|i| format!("read file {i}"))
            .find(|p| {
                let group = prompt_group(&json!([user(p)])).unwrap();
                !u8::from_str_radix(&group[..2], 16)
                    .unwrap()
                    .is_multiple_of(5)
            })
            .unwrap();
        f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user(&prompt),
                call("c1", "a"),
                answer("c1", "x"),
                say("x"),
            ],
        );
        let first = f.run();
        assert_eq!((first.added_train, first.added_valid), (2, 0));
        f.record(
            "selfplay:files",
            100.0,
            vec![
                system(),
                user(&prompt),
                call("c1", "a"),
                answer("c1", "x"),
                say("The answer is x"),
            ],
        );
        let next = f.run();
        assert_eq!((next.added_train, next.added_valid), (1, 0));
        // An independent prompt can supply validation without moving the existing group.
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("a different task"), say("other")],
        );
        let last = f.run();
        assert_eq!((last.added_train, last.added_valid), (0, 1));
        let (_, _, train_groups) = digests(&f.config.training.train_data).unwrap();
        let (_, _, valid_groups) = digests(&f.config.training.valid_data).unwrap();
        assert!(train_groups.is_disjoint(&valid_groups));
        assert_eq!(f.run().added(), 0);
    }

    #[test]
    fn compacted_recovery_and_success_are_one_split_group() {
        let f = Fixture::new();
        let id = f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("recent context"), say("done")],
        );
        f.store
            .tool_recovery(
                id,
                f.store.active().unwrap().id,
                &json!({
                    "context":[system(), user("original context")],
                    "tools":schemas(), "corrected":call("c1", "a"), "failure_kinds":["schema"]
                }),
            )
            .unwrap();
        // A different trajectory with the same original prompt joins the component too.
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user("original context"), say("also done")],
        );
        let report = f.run();
        assert!(matches!(
            (report.added_train, report.added_valid),
            (3, 0) | (0, 3)
        ));
        assert_eq!(f.run().added(), 0);
    }

    #[test]
    fn small_corpus_keeps_both_splits_without_splitting_a_group() {
        let f = Fixture::new();
        let prompts: Vec<_> = (0..100)
            .map(|i| format!("question {i}"))
            .filter(|p| {
                let group = prompt_group(&json!([user(p)])).unwrap();
                u8::from_str_radix(&group[..2], 16)
                    .unwrap()
                    .is_multiple_of(5)
            })
            .take(2)
            .collect();
        assert_eq!(prompts.len(), 2);
        for prompt in prompts {
            f.record(
                "selfplay:files",
                100.0,
                vec![system(), user(&prompt), say("answer")],
            );
        }
        let report = f.run();
        assert_eq!((report.added_train, report.added_valid), (1, 1));
        assert_eq!(f.run().added(), 0);
    }

    #[test]
    fn preexisting_prompt_overlap_is_rejected_even_with_distinct_answers() {
        let f = Fixture::new();
        fs::create_dir_all(f.dir.path().join("data")).unwrap();
        for (path, answer) in [
            (&f.config.training.train_data, "yes"),
            (&f.config.training.valid_data, "certainly"),
        ] {
            fs::write(
                path,
                format!(
                    "{}\n",
                    render(
                        vec![user("same question"), say(answer)],
                        &json!([]),
                        "operator".into()
                    )
                ),
            )
            .unwrap();
        }
        let error = harvest(&f.config, &f.store, &HashSet::new()).unwrap_err();
        assert!(error.to_string().contains("prompts overlap"));
    }

    #[test]
    fn appending_preserves_jsonl_without_an_existing_trailing_newline() {
        let f = Fixture::new();
        let path = f.dir.path().join("data.jsonl");
        fs::write(&path, "{\"a\":1}").unwrap();
        append(&path, "{\"a\":2}\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "{\"a\":1}\n{\"a\":2}\n");
    }
}
