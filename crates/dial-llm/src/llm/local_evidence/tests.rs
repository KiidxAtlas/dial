use super::*;

fn current(provider: &str) -> Current {
    Current {
        provider: provider.into(),
        model: "m".into(),
        edit_mode: "apply_patch".into(),
        edit_mode_pinned: false,
        context_window: 32768,
        side_models: "warn".into(),
        ..Current::default()
    }
}

fn tally(ok: u64, failed: u64) -> Tally {
    Tally { ok, failed }
}

fn paths(list: &[Suggestion]) -> Vec<&str> {
    list.iter().map(|s| s.path.as_str()).collect()
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn legacy_bare_integer_stages_read_and_damaged_entries_are_skipped() {
    let dir = temp_dir();
    let legacy = r#"{
            "mlx/a": 2,
            "lm-studio/b": {"stage": 1, "rounds": {"ok": 3, "failed": 1}, "future_field": true},
            "ollama/c": "garbage",
            "llama-cpp/d": {"rounds": "not an object"},
            "mlx/huge": 9999
        }"#;
    std::fs::write(dir.path().join(FILE), legacy).unwrap();
    let stored = read(dir.path()).unwrap();
    assert_eq!(
        stored["mlx/a"],
        Evidence {
            stage: 2,
            ..Evidence::default()
        }
    );
    assert_eq!(stored["lm-studio/b"].stage, 1);
    assert_eq!(stored["lm-studio/b"].rounds, tally(3, 1));
    assert!(!stored.contains_key("ollama/c"));
    assert!(!stored.contains_key("llama-cpp/d"));
    assert_eq!(stored["mlx/huge"].stage, u8::MAX);
    assert!(read(&dir.path().join("absent")).unwrap().is_empty());
    // A file that is not an object is refused, so an update cannot silently replace it.
    std::fs::write(dir.path().join(FILE), "[1, 2]").unwrap();
    assert!(read(dir.path()).is_err());
    assert!(update(dir.path(), "mlx/a", &Evidence::default()).is_err());
}

#[test]
fn mean_ignores_bad_samples_and_combines_by_weight() {
    let mut mean = Mean::default();
    for value in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
        mean.add(value);
    }
    assert_eq!(mean, Mean::default());
    mean.add(10.0);
    mean.add(20.0);
    assert_eq!((mean.samples, mean.mean), (2, 15.0));
    let mut other = Mean {
        samples: 6,
        mean: 5.0,
    };
    other.combine(&Mean::default());
    assert_eq!(
        other,
        Mean {
            samples: 6,
            mean: 5.0
        }
    );
    mean.combine(&other);
    assert_eq!(mean.samples, 8);
    assert!((mean.mean - 7.5).abs() < 1e-9, "{}", mean.mean);
    let mut empty = Mean::default();
    empty.combine(&mean);
    assert_eq!(empty, mean);
}

#[test]
fn tally_counts_outcomes_and_rates() {
    let mut t = Tally::default();
    assert_eq!(t.rate(), None);
    t.add(true);
    t.add(false);
    t.add(false);
    assert_eq!((t.ok, t.failed, t.total()), (1, 2, 3));
    assert!((t.rate().unwrap() - 2.0 / 3.0).abs() < 1e-12);
}

#[test]
fn merge_adds_counters_and_keeps_the_newest_probe_and_stage_maximum() {
    let mut stored = Evidence {
        stage: 2,
        requests: 10,
        served_context: Some(4096),
        loads: 1,
        load_secs: 4.0,
        edits: BTreeMap::from([("replace".to_owned(), tally(5, 1))]),
        ..Evidence::default()
    };
    stored.decode_tps.add(40.0);
    let mut delta = Evidence {
        stage: 1,
        requests: 5,
        loads: 2,
        load_secs: 6.0,
        overflows: 3,
        served_context: Some(8192),
        edits: BTreeMap::from([
            ("replace".to_owned(), tally(0, 2)),
            ("apply_patch".to_owned(), tally(4, 0)),
        ]),
        ..Evidence::default()
    };
    delta.decode_tps.add(20.0);
    delta.decode_tps.add(20.0);
    stored.merge(&delta);
    assert_eq!(stored.stage, 2, "stage keeps the maximum");
    assert_eq!(stored.requests, 15);
    assert_eq!(
        (stored.loads, stored.load_secs, stored.overflows),
        (3, 10.0, 3)
    );
    assert_eq!(stored.served_context, Some(8192));
    assert_eq!(stored.edits["replace"], tally(5, 3));
    assert_eq!(stored.edits["apply_patch"], tally(4, 0));
    assert_eq!(stored.decode_tps.samples, 3);
    assert!((stored.decode_tps.mean - 80.0 / 3.0).abs() < 1e-9);
    // A delta without a probe keeps the stored window.
    stored.merge(&Evidence::default());
    assert_eq!(stored.served_context, Some(8192));
}

#[test]
fn merge_replaces_a_draft_trial_for_another_model_and_combines_the_same_one() {
    let trial = |draft: &str, base: f64, with: f64| DraftTrial {
        draft_model: draft.into(),
        baseline: Mean {
            samples: 1,
            mean: base,
        },
        with_draft: Mean {
            samples: 1,
            mean: with,
        },
    };
    let mut stored = Evidence {
        draft: Some(trial("small", 10.0, 12.0)),
        ..Evidence::default()
    };
    stored.merge(&Evidence {
        draft: Some(trial("small", 20.0, 24.0)),
        ..Evidence::default()
    });
    let kept = stored.draft.clone().unwrap();
    assert_eq!(
        kept.baseline,
        Mean {
            samples: 2,
            mean: 15.0
        }
    );
    assert_eq!(
        kept.with_draft,
        Mean {
            samples: 2,
            mean: 18.0
        }
    );
    stored.merge(&Evidence {
        draft: Some(trial("other", 30.0, 25.0)),
        ..Evidence::default()
    });
    assert_eq!(stored.draft, Some(trial("other", 30.0, 25.0)));
}

#[test]
fn update_round_trips_and_merges_under_the_key() {
    let dir = temp_dir();
    let key = model_key("mlx", "org/model");
    assert_eq!(key, "mlx/org/model");
    let one = Evidence {
        rounds: tally(2, 1),
        loads: 1,
        ..Evidence::default()
    };
    update(dir.path(), &key, &one).unwrap();
    update(dir.path(), &key, &one).unwrap();
    update(
        dir.path(),
        "ollama/other",
        &Evidence {
            stage: 3,
            ..Evidence::default()
        },
    )
    .unwrap();
    let stored = read(dir.path()).unwrap();
    assert_eq!(stored[&key].rounds, tally(4, 2));
    assert_eq!(stored[&key].loads, 2);
    assert_eq!(stored["ollama/other"].stage, 3);
    // Written over a legacy file, the bare integers become records and the others survive.
    std::fs::write(dir.path().join(FILE), r#"{"mlx/legacy": 2}"#).unwrap();
    update(dir.path(), &key, &one).unwrap();
    let stored = read(dir.path()).unwrap();
    assert_eq!(stored["mlx/legacy"].stage, 2);
    assert_eq!(stored[&key].rounds, tally(2, 1));
}

#[test]
fn edit_grammar_switches_at_three_in_ten_failures_with_a_half_rate_alternative() {
    let cur = current("mlx");
    let mut e = Evidence::default();
    // Current grammar at 2 of 10 failed: below the 30% threshold.
    e.edits.insert("apply_patch".into(), tally(8, 2));
    e.edits.insert("replace".into(), tally(9, 1));
    assert!(suggestions(&e, &cur).is_empty());
    // Exactly 3 of 10: at the threshold. Replace fails 1 of 10, at most half of 30%.
    e.edits.insert("apply_patch".into(), tally(7, 3));
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["edit_mode"]);
    assert_eq!(found[0].value, json!("replace"));
    assert!(
        found[0].evidence.contains("3 of 10"),
        "{}",
        found[0].evidence
    );
    // Alternative at 2 of 10 is above half of 30% (15%): no switch.
    e.edits.insert("replace".into(), tally(8, 2));
    assert!(suggestions(&e, &cur).is_empty());
    // Exactly half: 3 of 20 against 6 of 20 is 15% and 30%.
    e.edits.insert("apply_patch".into(), tally(14, 6));
    e.edits.insert("replace".into(), tally(17, 3));
    assert_eq!(paths(&suggestions(&e, &cur)), ["edit_mode"]);
    // An alternative with fewer than 10 edits does not count.
    e.edits.insert("replace".into(), tally(9, 0));
    assert!(suggestions(&e, &cur).is_empty());
}

#[test]
fn stage_pins_replace_only_when_the_grammar_is_not_pinned_and_not_already_replace() {
    let cur = current("mlx");
    let mut e = Evidence {
        stage: 1,
        ..Evidence::default()
    };
    assert!(suggestions(&e, &cur).is_empty());
    e.stage = 2;
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["edit_mode"]);
    assert_eq!(found[0].value, json!("replace"));
    let pinned = Current {
        edit_mode_pinned: true,
        ..current("mlx")
    };
    assert!(suggestions(&e, &pinned).is_empty());
    let replace = Current {
        edit_mode: "replace".into(),
        ..current("mlx")
    };
    assert!(suggestions(&e, &replace).is_empty());
}

#[test]
fn repetition_rule_needs_three_loops_and_two_successful_retries() {
    let cur = current("mlx");
    let mut e = Evidence {
        repetition_loops: 2,
        penalty_retries: tally(2, 0),
        ..Evidence::default()
    };
    assert!(suggestions(&e, &cur).is_empty());
    e.repetition_loops = 3;
    e.penalty_retries = tally(1, 5);
    assert!(
        suggestions(&e, &cur).is_empty(),
        "one successful retry is not enough"
    );
    e.penalty_retries = tally(2, 0);
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["local_sampling.presence_penalty"]);
    assert_eq!(found[0].value, json!(0.5));
    assert!(
        suggestions(&e, &current("llama-cpp")).is_empty(),
        "not a presence-penalty provider"
    );
    let set = Current {
        presence_penalty: Some(0.2),
        ..current("mlx")
    };
    assert!(suggestions(&e, &set).is_empty());
}

#[test]
fn context_rule_needs_two_overflows_and_a_smaller_served_window() {
    let cur = current("mlx");
    let mut e = Evidence {
        overflows: 1,
        served_context: Some(16384),
        ..Evidence::default()
    };
    assert!(suggestions(&e, &cur).is_empty());
    e.overflows = 2;
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["context_window"]);
    assert_eq!(found[0].value, json!(16384));
    e.served_context = Some(32768);
    assert!(suggestions(&e, &cur).is_empty(), "served equals configured");
    e.served_context = Some(32767);
    assert_eq!(paths(&suggestions(&e, &cur)), ["context_window"]);
    let pinned = Current {
        profile_context_window: Some(8192),
        ..current("mlx")
    };
    assert!(suggestions(&e, &pinned).is_empty());
    let no_probe = Evidence {
        overflows: 9,
        ..Evidence::default()
    };
    assert!(suggestions(&no_probe, &cur).is_empty());
}

#[test]
fn keep_alive_rule_needs_three_ollama_loads_and_no_profile_value() {
    let cur = current("ollama");
    let mut e = Evidence {
        loads: 2,
        load_secs: 20.0,
        ..Evidence::default()
    };
    assert!(suggestions(&e, &cur).is_empty());
    e.loads = 3;
    e.load_secs = 30.0;
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["keep_alive"]);
    assert_eq!(found[0].value, json!("30m"));
    assert!(
        found[0].evidence.contains("10.0 s"),
        "{}",
        found[0].evidence
    );
    assert!(suggestions(&e, &current("mlx")).is_empty());
    let set = Current {
        keep_alive: Some("5m".into()),
        ..current("ollama")
    };
    assert!(suggestions(&e, &set).is_empty());
}

#[test]
fn side_model_rule_needs_three_swaps_and_the_warn_setting() {
    let cur = current("mlx");
    let mut e = Evidence {
        side_model_swaps: 2,
        ..Evidence::default()
    };
    assert!(suggestions(&e, &cur).is_empty());
    e.side_model_swaps = 3;
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["assistance.local.side_models"]);
    assert!(found[0].global);
    assert_eq!(found[0].value, json!("reuse_main"));
    assert!(found[0].evidence.contains("not measured"));
    let reuse = Current {
        side_models: "reuse_main".into(),
        ..current("mlx")
    };
    assert!(suggestions(&e, &reuse).is_empty());
}

#[test]
fn draft_rules_offer_a_candidate_then_judge_the_trial_at_twenty_samples() {
    let mut cur = current("mlx");
    cur.draft_candidate = Some("/models/small".into());
    let e = Evidence::default();
    let found = suggestions(&e, &cur);
    assert_eq!(paths(&found), ["local_runtime.draft_model"]);
    assert_eq!(found[0].value, json!("/models/small"));
    assert!(
        suggestions(&e, &current("lm-studio")).is_empty(),
        "candidates are offered for mlx only"
    );
    let applied = Current {
        draft_model: Some("/models/small".into()),
        ..cur.clone()
    };
    assert!(suggestions(&e, &applied).is_empty());

    let trial = |n: u64, base: f64, with: f64| Evidence {
        draft: Some(DraftTrial {
            draft_model: "/models/small".into(),
            baseline: Mean {
                samples: n,
                mean: base,
            },
            with_draft: Mean {
                samples: n,
                mean: with,
            },
        }),
        ..Evidence::default()
    };
    // A trial that was already recorded for this candidate is not offered again.
    assert!(suggestions(&trial(20, 10.0, 12.0), &cur).is_empty());
    // Judged only once both speeds have twenty samples.
    assert!(suggestions(&trial(19, 10.0, 9.0), &applied).is_empty());
    // At exactly 1.05 times the baseline the draft is kept: the rule needs a strictly lower speed.
    assert!(suggestions(&trial(20, 10.0, 10.5), &applied).is_empty());
    let found = suggestions(&trial(20, 10.0, 10.49), &applied);
    assert_eq!(paths(&found), ["local_runtime.draft_model"]);
    assert_eq!(found[0].value, Value::Null);
    assert!(
        found[0].evidence.contains("over 20 samples"),
        "{}",
        found[0].evidence
    );
}

#[test]
fn suggestions_come_in_rule_order() {
    let cur = Current {
        provider: "ollama".into(),
        edit_mode: "apply_patch".into(),
        side_models: "warn".into(),
        context_window: 32768,
        ..Current::default()
    };
    let e = Evidence {
        stage: 2,
        edits: BTreeMap::from([
            ("apply_patch".into(), tally(0, 10)),
            ("replace".into(), tally(10, 0)),
        ]),
        repetition_loops: 3,
        penalty_retries: tally(2, 0),
        overflows: 2,
        served_context: Some(8192),
        loads: 3,
        load_secs: 3.0,
        side_model_swaps: 3,
        ..Evidence::default()
    };
    assert_eq!(
        paths(&suggestions(&e, &cur)),
        [
            "edit_mode",
            "local_sampling.presence_penalty",
            "context_window",
            "keep_alive",
            "assistance.local.side_models"
        ]
    );
}

/// A local main model that keeps failing rounds, with no coach role and automatic escalation off,
/// gets the missing `[escalation]` key; choosing the coach is left to /roles.
#[test]
fn coach_escalation_is_suggested_only_for_struggling_local_models_without_a_coach() {
    let local = |coach: CoachEscalation| Current {
        coach,
        ..current("ollama")
    };
    let off = CoachEscalation {
        main_local: true,
        ..CoachEscalation::default()
    };
    let failing = Evidence {
        stage: 1,
        rounds: tally(10, 6),
        ..Evidence::default()
    };
    let found = suggestions(&failing, &local(off.clone()));
    assert_eq!(paths(&found), ["escalation.enabled"]);
    assert!(found[0].global);
    assert_eq!(found[0].value, json!(true));
    assert!(
        found[0]
            .evidence
            .starts_with("6 of 16 tool rounds failed; escalation reached stage 1."),
        "{}",
        found[0].evidence
    );
    assert!(
        found[0].evidence.contains("/roles"),
        "{}",
        found[0].evidence
    );
    // Enabled without automatic consultations: the missing key, at its default.
    let no_rounds = CoachEscalation {
        enabled: true,
        max_auto_per_turn: 1,
        ..off.clone()
    };
    let found = suggestions(&failing, &local(no_rounds));
    assert_eq!(paths(&found), ["escalation.auto_after_failed_rounds"]);
    assert_eq!(found[0].value, json!(3));
    let no_budget = CoachEscalation {
        enabled: true,
        auto_after_failed_rounds: 3,
        ..off.clone()
    };
    let found = suggestions(&failing, &local(no_budget));
    assert_eq!(paths(&found), ["escalation.max_auto_per_turn"]);
    assert_eq!(found[0].value, json!(1));
    // Hosted main model, an assigned coach, or only the role missing: nothing to apply.
    for coach in [
        CoachEscalation::default(),
        CoachEscalation {
            coach_assigned: true,
            ..off.clone()
        },
        CoachEscalation {
            enabled: true,
            auto_after_failed_rounds: 3,
            max_auto_per_turn: 1,
            ..off.clone()
        },
    ] {
        assert!(
            suggestions(&failing, &local(coach.clone())).is_empty(),
            "{coach:?}"
        );
    }
    // Too few failed rounds, or too small a share of them.
    for rounds in [tally(0, 5), tally(20, 6)] {
        let e = Evidence {
            rounds,
            ..Evidence::default()
        };
        assert!(
            suggestions(&e, &local(off.clone())).is_empty(),
            "{rounds:?}"
        );
    }
}
