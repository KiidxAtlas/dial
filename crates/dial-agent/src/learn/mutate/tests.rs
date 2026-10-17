use super::*;
use rand::{SeedableRng, rngs::StdRng};

fn ctx<'a>(
    kinds: &'a BTreeMap<String, usize>,
    rates: &'a [(String, usize, usize)],
    tried: &'a HashSet<String>,
    valid: &'a dyn Fn(&Harness) -> bool,
) -> Context<'a> {
    Context {
        failure_kinds: kinds,
        selfplay_rates: rates,
        default_edit_mode: EditMode::Hashline,
        tried,
        valid,
    }
}

type Signals = (BTreeMap<String, usize>, Vec<(String, usize, usize)>);

fn signals() -> Signals {
    (
        BTreeMap::from([("text_call".into(), 4), ("schema".into(), 2)]),
        vec![
            ("selfplay:read_line".into(), 1, 6),
            ("selfplay:move_file".into(), 5, 5),
        ],
    )
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
    let mut active = Harness {
        max_steps: 40,
        ..Default::default()
    };
    active.tools.retain(|t| t != "grep");
    let out = all(&active, &ctx(&kinds, &rates, &tried, &yes), 1);
    assert_eq!(
        names(&out),
        HashSet::from([
            "toggle_edit_mode",
            "temperature",
            "max_steps",
            "ablate_tool",
            "restore_tool",
            "guidance",
            "failure_note",
            "family_note"
        ])
    );
    assert_eq!(out.len(), 8);
    let digests: HashSet<_> = out.iter().map(|m| harness_digest(&m.harness)).collect();
    assert_eq!(digests.len(), out.len());
    assert!(
        out.iter()
            .all(|m| m.harness != active && !m.rationale.is_empty())
    );
    let restored = out.iter().find(|m| m.name == "restore_tool").unwrap();
    assert!(restored.harness.tools.contains(&"grep".to_string()));
    let ablated = out.iter().find(|m| m.name == "ablate_tool").unwrap();
    assert_eq!(ablated.harness.tools.len(), active.tools.len() - 1);
}

#[test]
fn no_signals_means_no_note_operators() {
    let (kinds, rates) = (
        BTreeMap::new(),
        vec![
            ("selfplay:read_line".into(), 3, 4),
            ("selfplay:nonexistent_family".into(), 0, 9),
        ],
    );
    let tried = HashSet::new();
    let yes = |_: &Harness| true;
    let out = all(&Harness::default(), &ctx(&kinds, &rates, &tried, &yes), 1);
    assert!(
        !names(&out).contains("failure_note")
            && !names(&out).contains("family_note")
            && !names(&out).contains("restore_tool")
    );
}

#[test]
fn edit_mode_toggle_resolves_against_the_default() {
    let (kinds, rates) = signals();
    let tried = HashSet::new();
    let yes = |_: &Harness| true;
    let toggled = |active: &Harness, default| {
        let c = Context {
            default_edit_mode: default,
            ..ctx(&kinds, &rates, &tried, &yes)
        };
        all(active, &c, 1)
            .into_iter()
            .find(|m| m.name == "toggle_edit_mode")
            .unwrap()
            .harness
            .edit_mode
    };
    let mut h = Harness::default();
    assert_eq!(toggled(&h, EditMode::Hashline), Some(EditMode::Replace));
    assert_eq!(toggled(&h, EditMode::Replace), Some(EditMode::Hashline));
    h.edit_mode = Some(EditMode::Replace);
    assert_eq!(toggled(&h, EditMode::Replace), Some(EditMode::Hashline));
}

#[test]
fn temperature_moves_to_adjacent_grid_values_only() {
    let (kinds, rates) = signals();
    let tried = HashSet::new();
    let yes = |_: &Harness| true;
    let temps = |t: f64| -> Vec<f64> {
        let h = Harness {
            temperature: t,
            ..Harness::default()
        };
        let mut seen = HashSet::new();
        let mut out = vec![];
        // One mutation per call; mark each result tried to enumerate the operator's options.
        let mut tried = tried.clone();
        for _ in 0..4 {
            let c = ctx(&kinds, &rates, &tried, &yes);
            match all(&h, &c, 1).into_iter().find(|m| m.name == "temperature") {
                Some(m) => {
                    tried.insert(harness_digest(&m.harness));
                    if seen.insert(m.harness.temperature.to_bits()) {
                        out.push(m.harness.temperature);
                    }
                }
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
        let h = Harness {
            max_steps: from,
            ..Harness::default()
        };
        let mut tried = tried.clone();
        let mut out = vec![];
        while let Some(m) = all(&h, &ctx(&kinds, &rates, &tried, &yes), 1)
            .into_iter()
            .find(|m| m.name == "max_steps")
        {
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
    assert!(
        steps(0).is_empty(),
        "unlimited is an operator choice, not a tiny budget"
    );
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
        if out.is_empty() {
            break;
        }
        for m in &out {
            assert!(
                tried.insert(harness_digest(&m.harness)),
                "repeated an already-tried harness"
            );
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
    let run = |seed| {
        all(&active, &c, seed)
            .iter()
            .map(|m| (m.name, harness_digest(&m.harness)))
            .collect::<Vec<_>>()
    };
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
        assert_eq!(
            candidates(&active, &c, &mut StdRng::seed_from_u64(1), limit).len(),
            limit
        );
    }
}

#[test]
fn notes_respect_harness_memory_caps() {
    let (kinds, rates) = signals();
    let tried = HashSet::new();
    let yes = |_: &Harness| true;
    let c = ctx(&kinds, &rates, &tried, &yes);
    // Full by count: oldest note is evicted.
    let full = Harness {
        memory: (0..32).map(|i| format!("old note {i}")).collect(),
        ..Harness::default()
    };
    let out = all(&full, &c, 1);
    let note = out.iter().find(|m| m.name == "failure_note").unwrap();
    assert_eq!(note.harness.memory.len(), 32);
    assert_eq!(note.harness.memory[0], "old note 1");
    assert!(
        note.harness
            .memory
            .last()
            .unwrap()
            .contains("tool-calling interface")
    );
    // Full by bytes: enough old notes go to fit the new one.
    let heavy = Harness {
        memory: vec!["x".repeat(8192), "y".repeat(8192)],
        ..Harness::default()
    };
    for m in all(&heavy, &c, 1)
        .iter()
        .filter(|m| m.name.ends_with("_note"))
    {
        assert!(
            m.harness.memory.len() <= 32
                && m.harness.memory.iter().map(String::len).sum::<usize>() <= 16384
        );
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
    let active = Harness {
        system_prompt: "something odd".into(),
        ..Harness::default()
    };
    let mut tried_prompts = HashSet::new();
    let mut tried = HashSet::new();
    for _ in 0..8 {
        let c2 = Context {
            tried: &tried,
            ..ctx(&kinds, &rates, &tried, &yes)
        };
        let Some(m) = all(&active, &c2, 2)
            .into_iter()
            .find(|m| m.name == "guidance")
        else {
            break;
        };
        tried.insert(harness_digest(&m.harness));
        tried_prompts.insert(m.harness.system_prompt);
    }
    let _ = c;
    assert!(tried_prompts.contains(&Harness::default().system_prompt));
    assert_eq!(tried_prompts.len(), 6);
}
