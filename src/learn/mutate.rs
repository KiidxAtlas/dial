//! Deterministic harness mutation operators.
//!
//! These give the improvement loop population diversity without a model call, so it still makes
//! progress when the feedback model is too weak to propose valid JSON. Each operator offers a few
//! concrete edits of the active harness; `candidates` takes at most one acceptable edit per
//! operator, in an order shuffled by the caller's RNG.

use crate::config::types::{EditModeConfig, Harness};
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
    pub default_edit_mode: EditModeConfig,
    /// `harness_digest` of every harness already evaluated.
    pub tried: &'a HashSet<String>,
    pub valid: &'a dyn Fn(&Harness) -> bool,
}

/// SHA-256 hex of the canonical (key-sorted) JSON of the harness.
pub fn harness_digest(h: &Harness) -> String {
    let canonical = serde_json::to_value(h).map(|v| v.to_string()).unwrap_or_default();
    Sha256::digest(canonical.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
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
        "text_call" | "dropped_call" => Some("Always call tools through the tool-calling interface with a valid JSON arguments object; never write a tool call as plain text."),
        "malformed_arguments" => Some("Tool arguments must be a single valid JSON object; close every brace and quote, and escape newlines inside strings."),
        "schema" => Some("Re-read each tool's description before calling it and match its parameter names and types exactly; do not invent or omit parameters."),
        "unknown_tool" => Some("Only use the tools that are actually listed for you; if a tool you want is missing, do the job with the listed ones."),
        _ => None,
    }
}

fn family_advice(family: &str) -> Option<&'static str> {
    Some(match family {
        "read_line" => "When asked for a specific line of a file, read the file and count lines exactly; answer with that line's text only.",
        "create_file" => "When asked to create a file, write exactly the requested content to exactly the requested path, then read it back to confirm.",
        "replace_value" => "To change a value in a file, read the file first, change only that value, keep all surrounding text byte-for-byte, and re-read to confirm.",
        "count_matches" => "To count matches, use grep or a command that counts, do not estimate by eye, and answer with the number alone.",
        "list_by_ext" => "To list files by extension, search the whole tree with glob, filter on the exact extension, and answer with the sorted list only.",
        "run_compute" => "When a result must be computed, run a command or script to compute it instead of working it out mentally, and report its exact output.",
        "fix_bug_py" => "To fix a Python bug, read the code, run it or its tests to see the failure, make a minimal fix, then run it again to confirm it passes.",
        "json_set" => "When editing JSON, keep the file valid: change only the requested key, preserve all other keys, and re-read the file to confirm it parses.",
        "append_line" => "To append a line, add it at the end of the file without touching existing lines, including a trailing newline only if the file already uses one.",
        "rename_symbol" => "When renaming a symbol, search every file for all occurrences, update each one, and search again to confirm none remain.",
        "sum_two_files" => "When a task combines values from two files, read both files fully, do the arithmetic with a command, and answer with the number alone.",
        "move_file" => "To move a file, move it (do not copy and leave the original), then list both locations to confirm the source is gone and the target exists.",
        "multi_file_edit" => "For edits that span several files, list every file that needs changing first, edit them one by one, and verify each before finishing.",
        "typo_hunt" => "When hunting typos, read the whole file slowly, fix only genuine misspellings, and leave correct text exactly as it was.",
        "script_and_run" => "When asked to write a script and run it, create the script file, run it, and report its actual output rather than predicting it.",
        _ => return None,
    })
}

/// Append `note`, evicting the oldest notes if needed to stay within the harness memory limits.
/// Returns false when the note cannot fit at all or is already present.
pub(super) fn push_note(h: &mut Harness, note: &str) -> bool {
    if note.len() > MAX_NOTE_BYTES || h.memory.iter().any(|m| m == note) { return false; }
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
    Mutation { name, rationale, harness }
}

type Operator = fn(&Harness, &Context, &mut dyn RngCore) -> Vec<Mutation>;

fn toggle_edit_mode(active: &Harness, ctx: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let current = active.edit_mode.unwrap_or(ctx.default_edit_mode);
    let next = match current { EditModeConfig::Hashline => EditModeConfig::Replace, EditModeConfig::Replace => EditModeConfig::Hashline };
    let mut h = active.clone();
    h.edit_mode = Some(next);
    vec![mutation("toggle_edit_mode", format!("switch edit mode from {current:?} to {next:?}: the other format may suit this model's editing better"), h)]
}

fn step_temperature(active: &Harness, _: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let t = active.temperature;
    let below = TEMPERATURES.iter().rev().find(|g| **g < t - 1e-9);
    let above = TEMPERATURES.iter().find(|g| **g > t + 1e-9);
    [below, above].into_iter().flatten().map(|g| {
        let mut h = active.clone();
        h.temperature = *g;
        mutation("temperature", format!("move temperature {t} -> {g} ({})", if *g < t { "more deterministic tool use" } else { "more exploration after failures" }), h)
    }).collect()
}

fn scale_max_steps(active: &Harness, _: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let from = active.max_steps;
    [(1.5, "more room to finish multi-step tasks"), (0.67, "a tighter budget stops aimless wandering")].into_iter().map(|(factor, why)| {
        let mut h = active.clone();
        h.max_steps = ((f64::from(from) * factor).round() as u32).clamp(MIN_STEPS, MAX_STEPS);
        mutation("max_steps", format!("max_steps {from} -> {}: {why}", h.max_steps), h)
    }).collect()
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
    let mut options: Vec<Mutation> = crate::tools::default_tools().into_iter().filter(|t| !active.tools.contains(t)).map(|tool| {
        let mut h = active.clone();
        h.tools.push(tool.clone());
        mutation("restore_tool", format!("restore the default {tool} tool: an earlier ablation may have removed something the model needs"), h)
    }).collect();
    options.shuffle(rng);
    options
}

fn guidance(active: &Harness, _: &Context, rng: &mut dyn RngCore) -> Vec<Mutation> {
    let mut options: Vec<Mutation> = guidance_variants().into_iter().filter(|(_, p)| *p != active.system_prompt).map(|(label, prompt)| {
        let mut h = active.clone();
        h.system_prompt = prompt;
        mutation("guidance", format!("replace the system prompt with the '{label}' variant"), h)
    }).collect();
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
    ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    ranked.into_iter().filter_map(|(advice, (count, kinds))| {
        let mut h = active.clone();
        push_note(&mut h, advice).then(|| mutation("failure_note", format!("{count} tool-call form failures ({}): add a memory note addressing them", kinds.join(", ")), h))
    }).collect()
}

fn family_note(active: &Harness, ctx: &Context, _: &mut dyn RngCore) -> Vec<Mutation> {
    let mut weak: Vec<(&str, usize, usize)> = ctx.selfplay_rates.iter()
        .map(|(f, p, t)| (f.strip_prefix("selfplay:").unwrap_or(f), *p, *t))
        .filter(|(_, p, t)| *t > 0 && p * 2 < *t)
        .collect();
    weak.sort_by(|a, b| (a.1 * b.2).cmp(&(b.1 * a.2)).then(a.0.cmp(b.0)));
    weak.into_iter().filter_map(|(family, passed, total)| {
        let advice = family_advice(family)?;
        let mut h = active.clone();
        push_note(&mut h, advice).then(|| mutation("family_note", format!("self-play family {family} passes {passed}/{total}: add targeted advice"), h))
    }).collect()
}

const OPERATORS: [Operator; 8] = [toggle_edit_mode, step_temperature, scale_max_steps, ablate_tool, restore_tool, guidance, failure_note, family_note];

/// Up to `limit` distinct valid mutations of `active`, at most one per operator, in an order
/// determined by `rng`. Edits equal to the active harness, already tried, or rejected by
/// `ctx.valid` are skipped; an operator whose first option is rejected falls back to its next one.
pub fn candidates(active: &Harness, ctx: &Context, rng: &mut impl Rng, limit: usize) -> Vec<Mutation> {
    let mut seen: HashSet<String> = HashSet::from([harness_digest(active)]);
    let mut operators = OPERATORS;
    operators.shuffle(rng);
    let mut out = Vec::new();
    for operator in operators {
        if out.len() >= limit { break; }
        for option in operator(active, ctx, rng) {
            let digest = harness_digest(&option.harness);
            if ctx.tried.contains(&digest) || seen.contains(&digest) || !(ctx.valid)(&option.harness) { continue; }
            seen.insert(digest);
            out.push(option);
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    fn ctx<'a>(kinds: &'a BTreeMap<String, usize>, rates: &'a [(String, usize, usize)], tried: &'a HashSet<String>, valid: &'a dyn Fn(&Harness) -> bool) -> Context<'a> {
        Context { failure_kinds: kinds, selfplay_rates: rates, default_edit_mode: EditModeConfig::Hashline, tried, valid }
    }

    fn signals() -> (BTreeMap<String, usize>, Vec<(String, usize, usize)>) {
        (BTreeMap::from([("text_call".into(), 4), ("schema".into(), 2)]), vec![("selfplay:read_line".into(), 1, 6), ("selfplay:move_file".into(), 5, 5)])
    }

    fn all(active: &Harness, c: &Context, seed: u64) -> Vec<Mutation> {
        candidates(active, c, &mut StdRng::seed_from_u64(seed), 100)
    }

    fn names(ms: &[Mutation]) -> HashSet<&'static str> {
        ms.iter().map(|m| m.name).collect()
    }

    #[test]
    fn every_operator_fires_and_every_output_differs() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let mut active = Harness::default();
        active.tools.retain(|t| t != "grep");
        let out = all(&active, &ctx(&kinds, &rates, &tried, &yes), 1);
        assert_eq!(names(&out), HashSet::from(["toggle_edit_mode", "temperature", "max_steps", "ablate_tool", "restore_tool", "guidance", "failure_note", "family_note"]));
        assert_eq!(out.len(), 8);
        let digests: HashSet<_> = out.iter().map(|m| harness_digest(&m.harness)).collect();
        assert_eq!(digests.len(), out.len());
        assert!(out.iter().all(|m| m.harness != active && !m.rationale.is_empty()));
        let restored = out.iter().find(|m| m.name == "restore_tool").unwrap();
        assert!(restored.harness.tools.contains(&"grep".to_string()));
        let ablated = out.iter().find(|m| m.name == "ablate_tool").unwrap();
        assert_eq!(ablated.harness.tools.len(), active.tools.len() - 1);
    }

    #[test]
    fn no_signals_means_no_note_operators() {
        let (kinds, rates) = (BTreeMap::new(), vec![("selfplay:read_line".into(), 3, 4), ("selfplay:nonexistent_family".into(), 0, 9)]);
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let out = all(&Harness::default(), &ctx(&kinds, &rates, &tried, &yes), 1);
        assert!(!names(&out).contains("failure_note") && !names(&out).contains("family_note") && !names(&out).contains("restore_tool"));
    }

    #[test]
    fn edit_mode_toggle_resolves_against_the_default() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let toggled = |active: &Harness, default| {
            let c = Context { default_edit_mode: default, ..ctx(&kinds, &rates, &tried, &yes) };
            all(active, &c, 1).into_iter().find(|m| m.name == "toggle_edit_mode").unwrap().harness.edit_mode
        };
        let mut h = Harness::default();
        assert_eq!(toggled(&h, EditModeConfig::Hashline), Some(EditModeConfig::Replace));
        assert_eq!(toggled(&h, EditModeConfig::Replace), Some(EditModeConfig::Hashline));
        h.edit_mode = Some(EditModeConfig::Replace);
        assert_eq!(toggled(&h, EditModeConfig::Replace), Some(EditModeConfig::Hashline));
    }

    #[test]
    fn temperature_moves_to_adjacent_grid_values_only() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let temps = |t: f64| -> Vec<f64> {
            let mut h = Harness::default();
            h.temperature = t;
            let mut seen = HashSet::new();
            let mut out = vec![];
            // One mutation per call; mark each result tried to enumerate the operator's options.
            let mut tried = tried.clone();
            for _ in 0..4 {
                let c = ctx(&kinds, &rates, &tried, &yes);
                match all(&h, &c, 1).into_iter().find(|m| m.name == "temperature") {
                    Some(m) => { tried.insert(harness_digest(&m.harness)); if seen.insert(m.harness.temperature.to_bits()) { out.push(m.harness.temperature); } }
                    None => break,
                }
            }
            out.sort_by(f64::total_cmp);
            out
        };
        assert_eq!(temps(0.2), vec![0.0, 0.5]);
        assert_eq!(temps(0.0), vec![0.2]);
        assert_eq!(temps(0.5), vec![0.2]);
        assert_eq!(temps(0.3), vec![0.2, 0.5]);
    }

    #[test]
    fn max_steps_scales_within_bounds() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let steps = |from: u32| -> Vec<u32> {
            let h = Harness { max_steps: from, ..Harness::default() };
            let mut tried = tried.clone();
            let mut out = vec![];
            while let Some(m) = all(&h, &ctx(&kinds, &rates, &tried, &yes), 1).into_iter().find(|m| m.name == "max_steps") {
                tried.insert(harness_digest(&m.harness));
                out.push(m.harness.max_steps);
            }
            out.sort_unstable();
            out
        };
        assert_eq!(steps(40), vec![27, 60]);
        assert_eq!(steps(60), vec![40, 64]);
        assert_eq!(steps(64), vec![43]);
        assert_eq!(steps(8), vec![12]);
        assert_eq!(steps(10), vec![8, 15]);
    }

    #[test]
    fn validity_filter_and_tried_set_are_respected() {
        let (kinds, rates) = signals();
        let active = Harness::default();
        let tried = HashSet::new();
        let only_edit_mode = |h: &Harness| h.edit_mode.is_some();
        let out = all(&active, &ctx(&kinds, &rates, &tried, &only_edit_mode), 3);
        assert_eq!(names(&out), HashSet::from(["toggle_edit_mode"]));
        let none = |_: &Harness| false;
        assert!(all(&active, &ctx(&kinds, &rates, &tried, &none), 3).is_empty());

        // Marking everything proposed as tried yields fresh digests each round, never a repeat,
        // and eventually nothing.
        let yes = |_: &Harness| true;
        let mut tried = HashSet::new();
        let mut rounds = 0;
        loop {
            let out = all(&active, &ctx(&kinds, &rates, &tried, &yes), rounds);
            if out.is_empty() { break; }
            for m in &out {
                assert!(tried.insert(harness_digest(&m.harness)), "repeated an already-tried harness");
            }
            rounds += 1;
            assert!(rounds < 50, "operators never exhaust");
        }
        assert!(rounds > 1);
    }

    #[test]
    fn output_is_deterministic_per_seed_and_the_seed_changes_order_and_choice() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let c = ctx(&kinds, &rates, &tried, &yes);
        let active = Harness::default();
        let run = |seed| all(&active, &c, seed).iter().map(|m| (m.name, harness_digest(&m.harness))).collect::<Vec<_>>();
        assert_eq!(run(5), run(5));
        assert!((0..10).any(|s| run(s) != run(5)));
    }

    #[test]
    fn limit_is_respected() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let c = ctx(&kinds, &rates, &tried, &yes);
        let active = Harness::default();
        assert!(candidates(&active, &c, &mut StdRng::seed_from_u64(1), 0).is_empty());
        for limit in [1, 3, 5] {
            assert_eq!(candidates(&active, &c, &mut StdRng::seed_from_u64(1), limit).len(), limit);
        }
    }

    #[test]
    fn notes_respect_harness_memory_caps() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let c = ctx(&kinds, &rates, &tried, &yes);
        // Full by count: oldest note is evicted.
        let full = Harness { memory: (0..32).map(|i| format!("old note {i}")).collect(), ..Harness::default() };
        let out = all(&full, &c, 1);
        let note = out.iter().find(|m| m.name == "failure_note").unwrap();
        assert_eq!(note.harness.memory.len(), 32);
        assert_eq!(note.harness.memory[0], "old note 1");
        assert!(note.harness.memory.last().unwrap().contains("tool-calling interface"));
        // Full by bytes: enough old notes go to fit the new one.
        let heavy = Harness { memory: vec!["x".repeat(8192), "y".repeat(8192)], ..Harness::default() };
        for m in all(&heavy, &c, 1).iter().filter(|m| m.name.ends_with("_note")) {
            assert!(m.harness.memory.len() <= 32 && m.harness.memory.iter().map(String::len).sum::<usize>() <= 16384);
            assert!(!m.harness.memory.contains(&"x".repeat(8192)));
        }
        // A note already present is not added again.
        let mut again = Harness::default();
        assert!(push_note(&mut again, "same"));
        assert!(!push_note(&mut again, "same"));
        assert!(!push_note(&mut again, &"z".repeat(MAX_NOTE_BYTES + 1)));
        assert_eq!(again.memory, vec!["same"]);
    }


    #[test]
    fn guidance_can_reset_to_the_default_prompt() {
        let (kinds, rates) = signals();
        let tried = HashSet::new();
        let yes = |_: &Harness| true;
        let c = ctx(&kinds, &rates, &tried, &yes);
        let active = Harness { system_prompt: "something odd".into(), ..Harness::default() };
        let mut tried_prompts = HashSet::new();
        let mut tried = HashSet::new();
        for _ in 0..8 {
            let c2 = Context { tried: &tried, ..ctx(&kinds, &rates, &tried, &yes) };
            let Some(m) = all(&active, &c2, 2).into_iter().find(|m| m.name == "guidance") else { break };
            tried.insert(harness_digest(&m.harness));
            tried_prompts.insert(m.harness.system_prompt);
        }
        let _ = c;
        assert!(tried_prompts.contains(&Harness::default().system_prompt));
        assert_eq!(tried_prompts.len(), 6);
    }
}
