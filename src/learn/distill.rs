//! Failure-driven distillation (opt-in, `improve.distill`).
//!
//! When this model's verified self-play is weak on some train families, the explicitly assigned
//! `coach` role (hosted or local) solves freshly generated tasks from those same families in the
//! same hermetic self-play setup. Every teacher attempt is judged by the same deterministic verifier
//! as local practice. Teacher trajectories live in their own database under this model's state
//! directory, so they never change local signals (`selfplay_rates`, live failures) and never enter
//! local tool-repair records. Harvest reads only verifier-passed teacher trajectories and tags them
//! with the `teacher-<model>` origin, which then shares the existing learned-record cap.
//!
//! Never used: gate-only and anchor families, held-out gate prompts, or any model when the coach role
//! is not assigned. Budget and stop gates apply exactly as they do to local self-play.
use crate::{
    agent::{self, Runtime},
    config::types::{Config, Version},
    learn::{
        auto,
        curriculum::{self, Role, Split, Task},
        improve,
    },
    storage::store::Store,
};
use anyhow::Result;
use rand::Rng;
use serde_json::{Value, json};
use std::{collections::HashSet, path::PathBuf};

/// A train family is weak when fewer than this share of its verified self-play attempts passed.
/// Matches the weakness threshold the feedback agent already sees (`improve::diagnose`).
const WEAK_RATE: f64 = 0.6;
/// Most families a cycle distills from, matching the self-play targeting cap.
const MAX_FAMILIES: usize = 4;
/// Teacher tasks are sampled with headroom: generation is not filtered per family.
const SAMPLE_FACTOR: usize = 3;
/// Upper bound for `improve.distill_tasks`; the setting declaration enforces the same range.
pub const MAX_TASKS: u32 = 64;

/// Whether distillation is switched on and a teacher is explicitly assigned.
pub fn enabled(config: &Config) -> bool {
    config.improve.distill && config.model_roles.contains_key("coach")
}

/// Harvest origin tag for a teacher model. Contains no `:` so trajectory provenance stays parseable.
pub fn origin_tag(model: &str) -> String {
    let sanitized: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("teacher-{sanitized}")
}

/// The teacher database for the currently assigned coach model, nested under this model's state.
pub fn teacher_dir(config: &Config) -> PathBuf {
    let coach = config.for_role("coach");
    config
        .state_dir
        .join("teacher")
        .join(origin_tag(&coach.model))
}

/// Families where the active version's verified self-play is weak, weakest first. Only train
/// families with tools qualify; gate and anchor families are never practiced for distillation.
pub fn weak_families(rates: &[(String, usize, usize)]) -> Vec<String> {
    let train = train_families();
    let mut weak: Vec<(f64, String)> = rates
        .iter()
        .filter_map(|(domain, passed, total)| {
            let family = domain.trim_start_matches("selfplay:");
            if *total == 0 || !train.contains(family) {
                return None;
            }
            let rate = *passed as f64 / *total as f64;
            (rate < WEAK_RATE).then(|| (rate, family.to_owned()))
        })
        .collect();
    weak.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    weak.into_iter()
        .take(MAX_FAMILIES)
        .map(|(_, family)| family)
        .collect()
}

/// Keep at most `cap` tasks that come from the weak train families and are not sealed gate prompts.
/// Families are checked against the curriculum split, not only against the weak list.
pub fn select_tasks(
    generated: Vec<Task>,
    weak: &[String],
    held_out: &HashSet<String>,
    cap: usize,
) -> Vec<Task> {
    let train = train_families();
    generated
        .into_iter()
        .filter(|task| {
            train.contains(task.family)
                && weak.iter().any(|family| family == task.family)
                && !held_out.contains(task.prompt.trim())
        })
        .take(cap)
        .collect()
}

fn train_families() -> HashSet<&'static str> {
    curriculum::families()
        .into_iter()
        .filter(|f| f.needs_tools && f.split == Split::Train)
        .map(|f| f.name)
        .collect()
}

/// One distillation pass. Returns the cycle-report step; budget gates end it early, and stop
/// requests propagate like they do for local self-play.
pub fn run(
    runtime: &mut Runtime,
    config: &Config,
    store: &Store,
    active: &Version,
    held_out: &HashSet<String>,
    progress: &mut dyn FnMut(&str),
) -> Result<Value> {
    if !config.model_roles.contains_key("coach") {
        return Ok(json!({"state":"coach_unassigned"}));
    }
    let cap = config.improve.distill_tasks.clamp(1, MAX_TASKS) as usize;
    let weak = weak_families(&store.selfplay_rates(active.id)?);
    if weak.is_empty() {
        return Ok(json!({"state":"no_weak_families"}));
    }
    let seed: u64 = rand::rng().random();
    let tasks = select_tasks(
        curriculum::generate_targeted(Role::SelfPlay, cap * SAMPLE_FACTOR, seed, &weak),
        &weak,
        held_out,
        cap,
    );
    let coach = config.for_role("coach");
    let teacher = Store::open(
        &teacher_dir(config),
        &coach.model,
        &config.use_case,
        &active.harness,
    )?;
    let teacher_version = teacher.active()?;

    // The resident backend belongs to the local model; the teacher gets its own and hands it back.
    runtime.release();
    let mut spend = runtime.usage(&coach)?.all.observed_cost_usd.unwrap_or(0.0);
    let (mut attempted, mut passed) = (0usize, 0usize);
    let mut state = "ran";
    let mut error = None;
    for (i, task) in tasks.iter().enumerate() {
        auto::check_stop(config)?;
        progress(&format!(
            "teacher practice {}/{} ({})",
            i + 1,
            tasks.len(),
            task.family
        ));
        match agent::selfplay_run(runtime, &coach, &teacher, &teacher_version, task) {
            Ok(ok) => {
                attempted += 1;
                passed += usize::from(ok);
            }
            Err(failure) => {
                error = Some(format!("{failure:#}"));
                break;
            }
        }
        match improve::enforce_daily_cost(runtime, &coach, store, &mut spend) {
            Ok(()) => {}
            Err(stop) if stop.downcast_ref::<auto::BudgetReached>().is_some() => {
                state = "budget_reached";
                break;
            }
            Err(stop) => return Err(stop),
        }
    }
    // Hand the backend back: the next local phase rebuilds the local model from its own config.
    runtime.release();
    Ok(json!({
        "state": state,
        "model": coach.model,
        "families": weak,
        "tasks": tasks.len(),
        "attempted": attempted,
        "passed": passed,
        "error": error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(family: &'static str, prompt: &str) -> Task {
        let mut generated = curriculum::generate(Role::SelfPlay, 1, 1);
        let mut task = generated.remove(0);
        task.family = family;
        task.prompt = prompt.into();
        task
    }

    #[test]
    fn distills_only_from_weak_train_families() {
        let rates = vec![
            ("selfplay:read_line".to_owned(), 1, 10),
            ("selfplay:create_file".to_owned(), 9, 10),
            ("selfplay:sum_two_files".to_owned(), 0, 5),
        ];
        let weak = weak_families(&rates);
        assert_eq!(weak, vec!["read_line".to_owned()]);
    }

    #[test]
    fn gate_and_anchor_families_never_become_teacher_tasks() {
        let gate = curriculum::families()
            .into_iter()
            .find(|f| f.split == Split::Gate && f.needs_tools)
            .expect("a gate family with tools exists")
            .name;
        let weak = vec!["read_line".to_owned(), gate.to_owned()];
        let generated = vec![
            task(gate, "gate prompt"),
            task("read_line", "train prompt"),
            task("create_file", "not weak"),
        ];
        let selected = select_tasks(generated, &weak, &HashSet::new(), 8);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].family, "read_line");
    }

    #[test]
    fn held_out_prompts_are_excluded_and_cap_is_respected() {
        let held: HashSet<String> = ["sealed prompt".to_owned()].into();
        let generated = vec![
            task("read_line", " sealed prompt "),
            task("read_line", "a"),
            task("read_line", "b"),
            task("read_line", "c"),
        ];
        let weak = vec!["read_line".to_owned()];
        let selected = select_tasks(generated, &weak, &held, 2);
        let prompts: Vec<&str> = selected.iter().map(|t| t.prompt.as_str()).collect();
        assert_eq!(prompts, vec!["a", "b"]);
    }

    #[test]
    fn origin_tag_has_no_separator_and_distillation_is_off_by_default() {
        assert_eq!(origin_tag("org/Model:7b"), "teacher-org_Model_7b");
        assert!(!origin_tag("a:b").contains(':'));
        assert!(!Config::default().improve.distill);
        assert!(!enabled(&Config::default()));
    }
}
