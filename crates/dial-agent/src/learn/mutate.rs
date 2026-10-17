//! Deterministic harness mutation operators.
//!
//! These give the improvement loop population diversity without a model call, so it still makes
//! progress when the feedback model is too weak to propose valid JSON. Each operator offers a few
//! concrete edits of the active harness; `candidates` takes at most one acceptable edit per
//! operator, in an order shuffled by the caller's RNG.

use crate::config::types::{EditMode, Harness};
use rand::{Rng, RngCore, seq::SliceRandom};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

/// Limits mirrored from `Config::validate_harness`.
const MAX_NOTES: usize = 32;
const MAX_NOTE_BYTES: usize = 16384;
const MIN_STEPS: u32 = 8;
const MAX_STEPS: u32 = 64;
const TEMPERATURES: [f64; 3] = [0.0, 0.2, 0.5];
/// Tools whose removal can help a small model that misuses or over-calls them.
const ABLATABLE: [&str; 5] = ["task", "todo", "skill_manage", "session_search", "recall"];

pub struct Mutation {
    pub name: &'static str,
    pub rationale: String,
    pub harness: Harness,
}

pub struct Context<'a> {
    /// `tool_form_failure` kind -> count.
    pub failure_kinds: &'a BTreeMap<String, usize>,
    /// Self-play pass rates: (family, passed, total). A `selfplay:` prefix on the family is accepted.
    pub selfplay_rates: &'a [(String, usize, usize)],
    /// What `edit_mode: None` resolves to.
    pub default_edit_mode: EditMode,
    /// `harness_digest` of every harness already evaluated.
    pub tried: &'a HashSet<String>,
    pub valid: &'a dyn Fn(&Harness) -> bool,
}

/// SHA-256 hex of the canonical (key-sorted) JSON of the harness.
pub fn harness_digest(h: &Harness) -> String {
    let canonical = serde_json::to_value(h)
        .map(|v| v.to_string())
        .unwrap_or_default();
    crate::hex(&Sha256::digest(canonical.as_bytes()))
}

fn guidance_variants() -> Vec<(&'static str, String)> {
    vec![
        ("default", Harness::default().system_prompt),
        ("verify-first", "Before you change anything, find out how success will be checked. After every change, run that check and read its result before moving on; never claim the work is done without evidence.".into()),
        ("minimal-diff", "Make the smallest change that solves the task. Do not rename, reformat or tidy anything you were not asked to touch, and re-read the file after editing to confirm only the intended lines changed.".into()),
        ("plan-first", "Start by writing a short plan, then work through it one item at a time, marking items done as you verify them. Re-plan when something surprises you.".into()),
        ("ask-fewer-questions", "Act on the task as stated. Make reasonable assumptions, note them in your final answer, and ask the user a question only when the task cannot proceed without the answer.".into()),
        ("tool-discipline", "Call tools through the tool interface, never write JSON in prose. Give each tool exactly the parameters its description lists, one call at a time, and read each result before the next call.".into()),
    ]
}

fn failure_advice(kind: &str) -> Option<&'static str> {
    match kind {
        "text_call" | "dropped_call" => Some(
            "Always call tools through the tool-calling interface with a valid JSON arguments object; never write a tool call as plain text.",
        ),
        "malformed_arguments" => Some(
            "Tool arguments must be a single valid JSON object; close every brace and quote, and escape newlines inside strings.",
        ),
        "schema" => Some(
            "Re-read each tool's description before calling it and match its parameter names and types exactly; do not invent or omit parameters.",
        ),
        "unknown_tool" => Some(
            "Only use the tools that are actually listed for you; if a tool you want is missing, do the job with the listed ones.",
        ),
        _ => None,
    }
}

fn family_advice(family: &str) -> Option<&'static str> {
    Some(match family {
        "read_line" => {
            "When asked for a specific line of a file, read the file and count lines exactly; answer with that line's text only."
        }
        "create_file" => {
            "When asked to create a file, write exactly the requested content to exactly the requested path, then read it back to confirm."
        }
        "replace_value" => {
            "To change a value in a file, read the file first, change only that value, keep all surrounding text byte-for-byte, and re-read to confirm."
        }
        "count_matches" => {
            "To count matches, use grep or a command that counts, do not estimate by eye, and answer with the number alone."
        }
        "list_by_ext" => {
            "To list files by extension, search the whole tree with glob, filter on the exact extension, and answer with the sorted list only."
        }
        "run_compute" => {
            "When a result must be computed, run a command or script to compute it instead of working it out mentally, and report its exact output."
        }
        "fix_bug_py" => {
            "To fix a Python bug, read the code, run it or its tests to see the failure, make a minimal fix, then run it again to confirm it passes."
        }
        "json_set" => {
            "When editing JSON, keep the file valid: change only the requested key, preserve all other keys, and re-read the file to confirm it parses."
        }
        "append_line" => {
            "To append a line, add it at the end of the file without touching existing lines, including a trailing newline only if the file already uses one."
        }
        "rename_symbol" => {
            "When renaming a symbol, search every file for all occurrences, update each one, and search again to confirm none remain."
        }
        "sum_two_files" => {
            "When a task combines values from two files, read both files fully, do the arithmetic with a command, and answer with the number alone."
        }
        "move_file" => {
            "To move a file, move it (do not copy and leave the original), then list both locations to confirm the source is gone and the target exists."
        }
        "multi_file_edit" => {
            "For edits that span several files, list every file that needs changing first, edit them one by one, and verify each before finishing."
        }
        "typo_hunt" => {
            "When hunting typos, read the whole file slowly, fix only genuine misspellings, and leave correct text exactly as it was."
        }
        "script_and_run" => {
            "When asked to write a script and run it, create the script file, run it, and report its actual output rather than predicting it."
        }
        _ => return None,
    })
}

/// Append `note`, evicting the oldest notes if needed to stay within the harness memory limits.
/// Returns false when the note cannot fit at all or is already present.
pub(super) fn push_note(h: &mut Harness, note: &str) -> bool {
    if note.len() > MAX_NOTE_BYTES || h.memory.iter().any(|m| m == note) {
        return false;
    }
    let total = |h: &Harness| h.memory.iter().map(String::len).sum::<usize>();
    let mut evict = 0;
    let mut bytes = total(h);
    while h.memory.len() - evict + 1 > MAX_NOTES || bytes + note.len() > MAX_NOTE_BYTES {
        bytes -= h.memory[evict].len();
        evict += 1;
    }
    h.memory.drain(..evict);
    h.memory.push(note.to_owned());
    true
}

fn mutation(name: &'static str, rationale: String, harness: Harness) -> Mutation {
    Mutation {
        name,
        rationale,
        harness,
    }
}

type Operator = fn(&Harness, &Context, &mut dyn RngCore) -> Vec<Mutation>;

fn toggle_edit_mode(active: &Harness, ctx: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let current = active.edit_mode.unwrap_or(ctx.default_edit_mode);
    let next = match current {
        EditMode::Hashline => EditMode::Replace,
        _ => EditMode::Hashline,
    };
    let mut h = active.clone();
    h.edit_mode = Some(next);
    vec![mutation(
        "toggle_edit_mode",
        format!(
            "switch edit mode from {current:?} to {next:?}: the other format may suit this model's editing better"
        ),
        h,
    )]
}

fn step_temperature(active: &Harness, _: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let t = active.temperature;
    let below = TEMPERATURES.iter().rev().find(|g| **g < t - 1e-9);
    let above = TEMPERATURES.iter().find(|g| **g > t + 1e-9);
    [below, above]
        .into_iter()
        .flatten()
        .map(|g| {
            let mut h = active.clone();
            h.temperature = *g;
            mutation(
                "temperature",
                format!(
                    "move temperature {t} -> {g} ({})",
                    if *g < t {
                        "more deterministic tool use"
                    } else {
                        "more exploration after failures"
                    }
                ),
                h,
            )
        })
        .collect()
}

fn scale_max_steps(active: &Harness, _: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let from = active.max_steps;
    if from == 0 {
        return Vec::new();
    }
    [
        (1.5, "more room to finish multi-step tasks"),
        (0.67, "a tighter budget stops aimless wandering"),
    ]
    .into_iter()
    .map(|(factor, why)| {
        let mut h = active.clone();
        h.max_steps = ((f64::from(from) * factor).round() as u32).clamp(MIN_STEPS, MAX_STEPS);
        mutation(
            "max_steps",
            format!("max_steps {from} -> {}: {why}", h.max_steps),
            h,
        )
    })
    .collect()
}

fn ablate_tool(active: &Harness, _: &Context, rng: &mut dyn RngCore) -> Vec<Mutation> {
    let mut options: Vec<Mutation> = ABLATABLE.iter().filter(|t| active.tools.iter().any(|x| x == *t)).map(|tool| {
        let mut h = active.clone();
        h.tools.retain(|x| x != tool);
        mutation("ablate_tool", format!("drop the {tool} tool: fewer tools means fewer ways for a small model to misuse them"), h)
    }).collect();
    options.shuffle(rng);
    options
}

fn restore_tool(active: &Harness, _: &Context, rng: &mut dyn RngCore) -> Vec<Mutation> {
    let mut options: Vec<Mutation> = crate::config::tools::default_tools().into_iter().filter(|t| !active.tools.contains(t)).map(|tool| {
        let mut h = active.clone();
        h.tools.push(tool.clone());
        mutation("restore_tool", format!("restore the default {tool} tool: an earlier ablation may have removed something the model needs"), h)
    }).collect();
    options.shuffle(rng);
    options
}

fn guidance(active: &Harness, _: &Context, rng: &mut dyn RngCore) -> Vec<Mutation> {
    let mut options: Vec<Mutation> = guidance_variants()
        .into_iter()
        .filter(|(_, p)| *p != active.system_prompt)
        .map(|(label, prompt)| {
            let mut h = active.clone();
            h.system_prompt = prompt;
            mutation(
                "guidance",
                format!("replace the system prompt with the '{label}' variant"),
                h,
            )
        })
        .collect();
    options.shuffle(rng);
    options
}

fn failure_note(active: &Harness, ctx: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    // Merge kinds that share advice, most frequent first (ties by advice text, so deterministic).
    let mut by_advice: BTreeMap<&str, (usize, Vec<&str>)> = BTreeMap::new();
    for (kind, count) in ctx.failure_kinds.iter().filter(|(_, c)| **c > 0) {
        if let Some(advice) = failure_advice(kind) {
            let entry = by_advice.entry(advice).or_default();
            entry.0 += count;
            entry.1.push(kind);
        }
    }
    let mut ranked: Vec<_> = by_advice.into_iter().collect();
    ranked.sort_by_key(|a| std::cmp::Reverse(a.1.0));
    ranked
        .into_iter()
        .filter_map(|(advice, (count, kinds))| {
            let mut h = active.clone();
            push_note(&mut h, advice).then(|| {
                mutation(
                    "failure_note",
                    format!(
                        "{count} tool-call form failures ({}): add a memory note addressing them",
                        kinds.join(", ")
                    ),
                    h,
                )
            })
        })
        .collect()
}

fn family_note(active: &Harness, ctx: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let mut weak: Vec<(&str, usize, usize)> = ctx
        .selfplay_rates
        .iter()
        .map(|(f, p, t)| (f.strip_prefix("selfplay:").unwrap_or(f), *p, *t))
        .filter(|(_, p, t)| *t > 0 && p * 2 < *t)
        .collect();
    weak.sort_by(|a, b| (a.1 * b.2).cmp(&(b.1 * a.2)).then(a.0.cmp(b.0)));
    weak.into_iter()
        .filter_map(|(family, passed, total)| {
            let advice = family_advice(family)?;
            let mut h = active.clone();
            push_note(&mut h, advice).then(|| {
                mutation(
                    "family_note",
                    format!(
                        "self-play family {family} passes {passed}/{total}: add targeted advice"
                    ),
                    h,
                )
            })
        })
        .collect()
}

const OPERATORS: [Operator; 8] = [
    toggle_edit_mode,
    step_temperature,
    scale_max_steps,
    ablate_tool,
    restore_tool,
    guidance,
    failure_note,
    family_note,
];

/// Up to `limit` distinct valid mutations of `active`, at most one per operator, in an order
/// determined by `rng`. Edits equal to the active harness, already tried, or rejected by
/// `ctx.valid` are skipped; an operator whose first option is rejected falls back to its next one.
pub fn candidates(
    active: &Harness,
    ctx: &Context,
    rng: &mut impl Rng,
    limit: usize,
) -> Vec<Mutation> {
    let mut seen: HashSet<String> = HashSet::from([harness_digest(active)]);
    let mut operators = OPERATORS;
    operators.shuffle(rng);
    let mut out = Vec::new();
    for operator in operators {
        if out.len() >= limit {
            break;
        }
        for option in operator(active, ctx, rng) {
            let digest = harness_digest(&option.harness);
            if ctx.tried.contains(&digest)
                || seen.contains(&digest)
                || !(ctx.valid)(&option.harness)
            {
                continue;
            }
            seen.insert(digest);
            out.push(option);
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests;
