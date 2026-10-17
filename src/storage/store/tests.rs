use super::*;
use crate::config::types::SuiteScore;

fn harness() -> Harness {
    Harness {
        system_prompt: "test".into(),
        memory: vec![],
        tools: vec![],
        subagents: vec![],
        max_steps: 2,
        temperature: 0.0,
        max_tokens: 32,
        edit_mode: None,
    }
}

#[test]
fn oversized_tool_recovery_is_skipped_without_truncation_or_poisoning_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "repair", "input")
        .unwrap();
    for large in [
        json!({"context":"x".repeat(256 * 1024)}),
        json!({"context":"\u{0}".repeat(50000)}),
    ] {
        assert!(!store.tool_recovery(trajectory, version, &large).unwrap());
    }
    assert!(store.tool_recoveries().unwrap().is_empty());
    let small = json!({"context":[{"role":"user","content":"fix the call"}],"tools":[],"corrected":{"role":"assistant","content":"done"}});
    assert!(store.tool_recovery(trajectory, version, &small).unwrap());
    assert!(!store.tool_recovery(trajectory, version, &small).unwrap());
    let records = store.tool_recoveries().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1, small);
}

fn passing(candidate: &Version) -> Admission {
    Admission {
        candidate: candidate.id,
        baseline: candidate.parent.unwrap(),
        passed: true,
        reason: "evaluated".into(),
        scores: vec![
            SuiteScore {
                name: "primary".into(),
                primary: true,
                baseline: 50.0,
                candidate: 60.0,
                cases: 10,
                candidate_only: 0,
                baseline_only: 0,
                tier: None,
            },
            SuiteScore {
                name: "guard".into(),
                primary: false,
                baseline: 100.0,
                candidate: 100.0,
                cases: 10,
                candidate_only: 0,
                baseline_only: 0,
                tier: None,
            },
        ],
    }
}

fn training(cursor: u64) -> TrainingState {
    TrainingState {
        train_path: "/train.jsonl".into(),
        watermark: cursor,
        replay: vec![format!("{{\"n\":{cursor}}}")],
        prefix_digest: Some(format!("prefix-{cursor}")),
        validation_path: Some("/valid.jsonl".into()),
        train_digest: Some("train-window".into()),
        validation_digest: Some("validation-window".into()),
    }
}

#[test]
fn latest_lora_evaluation_keeps_scores_and_actual_activation_decision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    assert!(store.latest_lora_admission().unwrap().is_none());
    let base = store.active().unwrap();
    let candidate = store
        .candidate(base.id, "lora", &harness(), Some(Path::new("/adapter")))
        .unwrap();
    store.admit(&passing(&candidate)).unwrap();
    let (report, activated, _) = store.latest_lora_admission().unwrap().unwrap();
    assert!(activated);
    assert_eq!(report.scores[0].baseline, 50.0);
    assert_eq!(report.scores[0].candidate, 60.0);
    let rejected = store
        .candidate(
            candidate.id,
            "lora",
            &harness(),
            Some(Path::new("/rejected")),
        )
        .unwrap();
    store
        .admit(&Admission {
            candidate: rejected.id,
            baseline: candidate.id,
            passed: false,
            reason: "No improvement".into(),
            scores: vec![],
        })
        .unwrap();
    let harness_candidate = store
        .candidate(
            candidate.id,
            "harness",
            &harness(),
            candidate.adapter.as_deref(),
        )
        .unwrap();
    store.admit(&passing(&harness_candidate)).unwrap();
    let (report, activated, _) = store.latest_lora_admission().unwrap().unwrap();
    assert_eq!(
        report.candidate, rejected.id,
        "a harness admission must not replace the LoRA result"
    );
    assert!(!activated);
    assert_eq!(report.reason, "No improvement");
}

#[test]
fn training_state_follows_admission_harness_inheritance_and_rollback_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    #[cfg(windows)]
    assert!(
        crate::platform::fs::is_owner_only(
            &fs::File::open(dir.path().join("state.sqlite3")).unwrap()
        )
        .unwrap()
    );
    let base = store.active().unwrap();
    let first = store
        .candidate(base.id, "lora", &harness(), Some(Path::new("/adapter-1")))
        .unwrap();
    store
        .admit_with_training(&passing(&first), Some(&training(64)))
        .unwrap();
    let harness_version = store
        .candidate(first.id, "harness", &harness(), first.adapter.as_deref())
        .unwrap();
    store.admit(&passing(&harness_version)).unwrap();
    assert_eq!(
        store.training_state(harness_version.id).unwrap(),
        Some(training(64))
    );
    let second = store
        .candidate(
            harness_version.id,
            "lora",
            &harness(),
            Some(Path::new("/adapter-2")),
        )
        .unwrap();
    store
        .admit_with_training(&passing(&second), Some(&training(128)))
        .unwrap();
    drop(store);
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    assert_eq!(
        store.training_state(store.active().unwrap().id).unwrap(),
        Some(training(128))
    );
    store.rollback().unwrap();
    assert_eq!(
        store.training_state(store.active().unwrap().id).unwrap(),
        Some(training(64))
    );
    assert_eq!(
        store.training_state(second.id).unwrap(),
        Some(training(128))
    );
    store.rollback().unwrap();
    store.rollback().unwrap();
    assert_eq!(store.active().unwrap().id, base.id);
    assert_eq!(store.training_state(base.id).unwrap(), None);
}

#[test]
fn rejected_stale_and_failed_admissions_never_consume_training_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let base = store.active().unwrap();
    let rejected = store
        .candidate(base.id, "lora", &harness(), Some(Path::new("/rejected")))
        .unwrap();
    let mut report = passing(&rejected);
    report.passed = false;
    store
        .admit_with_training(&report, Some(&training(64)))
        .unwrap();
    assert_eq!(store.training_state(rejected.id).unwrap(), None);
    let stale = store
        .candidate(base.id, "lora", &harness(), Some(Path::new("/stale")))
        .unwrap();
    let winner = store
        .candidate(base.id, "lora", &harness(), Some(Path::new("/winner")))
        .unwrap();
    store
        .admit_with_training(&passing(&winner), Some(&training(64)))
        .unwrap();
    assert!(
        store
            .admit_with_training(&passing(&stale), Some(&training(128)))
            .is_err()
    );
    assert_eq!(store.training_state(stale.id).unwrap(), None);
    let failed = store
        .candidate(winner.id, "lora", &harness(), Some(Path::new("/failed")))
        .unwrap();
    // Fail after the training-state insert, before promotion commits.
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER fail_promotion BEFORE UPDATE OF status ON versions
            WHEN NEW.status='active' BEGIN SELECT RAISE(ABORT,'injected failure'); END;",
        )
        .unwrap();
    assert!(
        store
            .admit_with_training(&passing(&failed), Some(&training(128)))
            .is_err()
    );
    assert_eq!(store.active().unwrap().id, winner.id);
    assert_eq!(store.version(failed.id).unwrap().status, "candidate");
    assert_eq!(store.training_state(failed.id).unwrap(), None);
    assert_eq!(store.training_state(winner.id).unwrap(), Some(training(64)));
}

#[test]
fn legacy_cursor_is_migrated_only_to_current_adapter_and_never_to_ancestors() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let base = store.active().unwrap();
    let first = store
        .candidate(base.id, "lora", &harness(), Some(Path::new("/legacy-1")))
        .unwrap();
    store.admit(&passing(&first)).unwrap();
    let second = store
        .candidate(first.id, "lora", &harness(), Some(Path::new("/legacy-2")))
        .unwrap();
    store.admit(&passing(&second)).unwrap();
    store
        .set_meta(
            "lora_commit",
            r#"{"train_path":"/train.jsonl","watermark":128,"replay":["old"]}"#,
        )
        .unwrap();
    drop(store);
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let state = store.training_state(second.id).unwrap().unwrap();
    assert_eq!(state.watermark, 128);
    assert_eq!(state.replay, vec!["old"]);
    assert_eq!(state.prefix_digest, None);
    assert!(store.get_meta("lora_commit").unwrap().is_none());
    store.rollback().unwrap();
    assert!(store.training_state(first.id).unwrap().is_none());
}

#[test]
fn admission_is_atomic_and_stale_candidates_cannot_activate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let other = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let baseline = store.active().unwrap();
    let winner = store
        .candidate(baseline.id, "harness", &harness(), None)
        .unwrap();
    let stale = other
        .candidate(baseline.id, "harness", &harness(), None)
        .unwrap();
    store.admit(&passing(&winner)).unwrap();
    assert!(other.admit(&passing(&stale)).is_err());
    assert_eq!(other.active().unwrap().id, winner.id);
    assert_eq!(other.version(stale.id).unwrap().status, "rejected");
    assert_eq!(store.versions().unwrap().len(), 3);
    let audit: i64 = store
        .conn
        .query_row("SELECT count(*) FROM admissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(audit, 2);
    assert!(Store::open(dir.path(), "different", "case", &harness()).is_err());
    assert!(store.set_meta("active", "999").is_err());
}

fn frozen_bootstrap() -> Harness {
    let tools = crate::config::tools::default_tools()
        .into_iter()
        .filter(|tool| !["task", "skill", "finish"].contains(&tool.as_str()))
        .collect();
    Harness {
        tools,
        max_steps: 40,
        max_tokens: 8192,
        ..harness()
    }
}

#[test]
fn frozen_bootstrap_root_resolves_current_default_tools_without_rewriting_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &frozen_bootstrap()).unwrap();
    let active = store.active().unwrap();
    assert!(active.harness.tools.contains(&"task".to_string()));
    assert!(active.harness.tools.contains(&"skill".to_string()));
    // 40/8192 were shipped defaults, not operator choices: they follow today's unlimited defaults.
    assert_eq!(
        (active.harness.max_steps, active.harness.max_tokens),
        (0, 0)
    );
    let pinned_dir = tempfile::tempdir().unwrap();
    let pinned = Harness {
        max_steps: 25,
        ..frozen_bootstrap()
    };
    let pinned = Store::open(pinned_dir.path(), "model", "case", &pinned).unwrap();
    assert_eq!(pinned.active().unwrap().harness.max_steps, 25);
    let raw: String = store
        .conn
        .query_row(
            "SELECT harness FROM versions WHERE id=?1",
            [active.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!raw.contains("\"task\""));
}

#[test]
fn learned_child_keeps_its_own_fields_and_gains_new_default_tools() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &frozen_bootstrap()).unwrap();
    let parent = store.active().unwrap();
    let child_harness = Harness {
        temperature: 0.7,
        ..frozen_bootstrap()
    };
    let child = store
        .candidate(parent.id, "harness", &child_harness, None)
        .unwrap();
    assert_eq!(child.harness.temperature, 0.7);
    assert!(child.harness.tools.contains(&"task".to_string()));
    assert!(child.harness.tools.contains(&"skill".to_string()));
}

#[test]
fn rollback_walks_admitted_lineage_not_pending_or_rejected_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let original = store.active().unwrap();
    let first = store
        .candidate(original.id, "harness", &harness(), None)
        .unwrap();
    store.admit(&passing(&first)).unwrap();
    let rejected = store
        .candidate(first.id, "harness", &harness(), None)
        .unwrap();
    let mut report = passing(&rejected);
    report.passed = false;
    store.admit(&report).unwrap();
    let second = store
        .candidate(first.id, "harness", &harness(), None)
        .unwrap();
    store.admit(&passing(&second)).unwrap();
    let pending = store
        .candidate(second.id, "harness", &harness(), None)
        .unwrap();
    assert_eq!(store.rollback().unwrap().id, first.id);
    assert_eq!(store.rollback().unwrap().id, original.id);
    assert!(store.rollback().is_err());
    assert_eq!(store.version(pending.id).unwrap().status, "candidate");
    assert_eq!(store.version(rejected.id).unwrap().status, "rejected");
    assert_eq!(store.version(second.id).unwrap().status, "known_good");
    assert_eq!(store.versions().unwrap().len(), 5);
}

#[test]
fn incomplete_or_forged_lineage_report_never_activates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let initial = store.active().unwrap();
    let candidate = store
        .candidate(initial.id, "harness", &harness(), None)
        .unwrap();
    let mut report = passing(&candidate);
    report.scores.clear();
    assert!(store.admit(&report).is_err());
    let candidate = store
        .candidate(initial.id, "harness", &harness(), None)
        .unwrap();
    let mut report = passing(&candidate);
    report.baseline = 999;
    assert!(store.admit(&report).is_err());
    assert_eq!(store.active().unwrap().id, initial.id);
    assert!(
        store
            .conn
            .execute("UPDATE versions SET harness='{}' WHERE id=?1", [initial.id])
            .is_err()
    );
}

#[test]
fn outcomes_require_completion_and_unknown_scores_remain_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let id = store
        .begin_trajectory(version, "coding", "repair", "input")
        .unwrap();
    let mut outcome = Outcome {
        trajectory: id,
        score: 100.0,
        source: "human".into(),
        reference: None,
    };
    assert!(store.outcome(&outcome).is_err());
    let mut event = Event {
        trajectory: id,
        version,
        domain: "coding".into(),
        task: "repair".into(),
        step: 1,
        kind: "generation".into(),
        data: json!({"tokens":5}),
        score: None,
    };
    store.event(&event).unwrap();
    event.version = 999;
    assert!(store.event(&event).is_err());
    store.finish_trajectory(id, Some("answer"), None).unwrap();
    assert!(store.metrics().unwrap()["outcomes"][0]["mean_score"].is_null());
    assert!(store.metrics().unwrap()["events"][0]["mean_score"].is_null());
    assert!(store.recent(1).unwrap()[0]["outcome"].is_null());
    store.outcome(&outcome).unwrap();
    outcome.score = 40.0;
    store.outcome(&outcome).unwrap();
    assert_eq!(store.metrics().unwrap()["outcomes"][0]["mean_score"], 40.0);
    assert_eq!(store.metrics().unwrap()["outcomes"][0]["scored"], 1);
    assert_eq!(
        store.recent(1).unwrap()[0]["events"][0]["score"],
        Value::Null
    );
    for score in [f64::NAN, f64::INFINITY, -0.1, 100.1] {
        outcome.score = score;
        assert!(store.outcome(&outcome).is_err());
    }
    outcome.score = 0.0;
    outcome.source = " ".into();
    assert!(store.outcome(&outcome).is_err());
    assert!(store.finish_trajectory(id, Some("rewrite"), None).is_err());
}

#[test]
fn approving_a_session_credits_only_its_usable_unjudged_turns() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let turn = |task: &str, prompt: &str, failed: bool, saved: bool| {
        let id = store
            .begin_trajectory(version, "coding", task, prompt)
            .unwrap();
        if failed {
            store.finish_trajectory(id, None, Some("boom")).unwrap();
        } else {
            store.finish_trajectory(id, Some("done"), None).unwrap();
        }
        if saved {
            store.save_messages(id, &json!({"examples":[]})).unwrap();
        }
        id
    };
    let plain = turn("session:abc", "a", false, true);
    let checked = turn("session:abc", "b", false, true);
    let rejected = turn("session:abc", "c", false, true);
    turn("session:abc", "d", true, true);
    turn("session:abc", "e", false, false);
    let other = turn("session:other", "f", false, true);
    let unfinished = store
        .begin_trajectory(version, "coding", "session:abc", "g")
        .unwrap();
    for (id, score, source) in [
        (checked, 100.0, "checks:behavioral"),
        (rejected, 20.0, "user"),
    ] {
        store
            .outcome(&Outcome {
                trajectory: id,
                score,
                source: source.into(),
                reference: None,
            })
            .unwrap();
    }

    let approval = store.approve_session("abc").unwrap();
    assert_eq!(
        approval,
        SessionApproval {
            approved: 1,
            already: 1,
            judged: 1,
            unusable: 2,
        }
    );
    let score = |id: i64| -> Option<(f64, String)> {
        store
            .conn
            .query_row(
                "SELECT score,source FROM outcomes WHERE trajectory=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap()
    };
    assert_eq!(score(plain), Some((100.0, "user:session".into())));
    assert_eq!(score(rejected), Some((20.0, "user".into())));
    assert_eq!(score(other), None);
    assert_eq!(score(unfinished), None);
    assert_eq!(store.verified_live_turns().unwrap(), 2);
    // Approving again changes nothing.
    assert_eq!(store.approve_session("abc").unwrap().approved, 0);
    assert!(store.approve_session(" ").is_err());
}

#[test]
fn checks_outcome_counts_as_verified_live_turn_and_user_undo_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let id = store
        .begin_trajectory(version, "coding", "session:1", "fix it")
        .unwrap();
    store.finish_trajectory(id, Some("done"), None).unwrap();
    assert_eq!(store.verified_live_turns().unwrap(), 0);
    store
        .outcome(&Outcome {
            trajectory: id,
            score: 100.0,
            source: "checks:behavioral".into(),
            reference: None,
        })
        .unwrap();
    assert_eq!(store.verified_live_turns().unwrap(), 1);
    store
        .outcome(&Outcome {
            trajectory: id,
            score: 0.0,
            source: "user:undo".into(),
            reference: None,
        })
        .unwrap();
    assert_eq!(store.verified_live_turns().unwrap(), 0);
}

#[test]
fn a_commit_never_overrides_a_persons_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let turn = |prompt: &str| {
        let id = store
            .begin_trajectory(version, "coding", "session:1", prompt)
            .unwrap();
        store.finish_trajectory(id, Some("done"), None).unwrap();
        id
    };
    let commit = |trajectory| Outcome {
        trajectory,
        score: 100.0,
        source: "git:commit".into(),
        reference: None,
    };
    let (checked, rejected, plain) = (turn("a"), turn("b"), turn("c"));
    for (id, source, score) in [
        (checked, "checks:behavioral", 100.0),
        (rejected, "user:rating", 0.0),
    ] {
        store
            .outcome(&Outcome {
                trajectory: id,
                score,
                source: source.into(),
                reference: None,
            })
            .unwrap();
    }
    assert!(store.implicit_outcome(&commit(checked)).unwrap());
    assert!(!store.implicit_outcome(&commit(rejected)).unwrap());
    assert!(store.implicit_outcome(&commit(plain)).unwrap());
    assert_eq!(
        store.verified_live_turns().unwrap(),
        2,
        "commits count as verified; the rejected turn does not"
    );
}

#[test]
fn recent_excludes_pending_work_and_bounds_events_without_losing_latest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let old = store
        .begin_trajectory(version, "coding", "repair", "old")
        .unwrap();
    store
        .finish_trajectory(old, Some("old answer"), None)
        .unwrap();
    let latest = store
        .begin_trajectory(version, "coding", "repair", "latest")
        .unwrap();
    for step in 0..130 {
        store
            .event(&Event {
                trajectory: latest,
                version,
                domain: "coding".into(),
                task: "repair".into(),
                step,
                kind: "generation".into(),
                data: json!({}),
                score: None,
            })
            .unwrap();
    }
    store
        .finish_trajectory(latest, None, Some("failed"))
        .unwrap();
    store
        .begin_trajectory(version, "coding", "repair", "unfinished")
        .unwrap();
    let recent = store.recent(1).unwrap();
    let rows = recent.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], latest);
    let events = rows[0]["events"].as_array().unwrap();
    assert_eq!(events.len(), 128);
    assert_eq!(events[0]["step"], 2);
    assert_eq!(events[127]["step"], 129);
    assert_eq!(store.recent(0).unwrap(), json!([]));
}
#[test]
fn recent_tool_recoveries_are_newest_first_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "repair", "prompt")
        .unwrap();
    for n in 0..4 {
        let project_root = if n % 2 == 0 {
            "/workspace/a"
        } else {
            "/workspace/b"
        };
        store
            .tool_recovery(
                trajectory,
                version,
                &json!({"n": n, "project_root": project_root}),
            )
            .unwrap();
    }
    let recent = store.recent_tool_recoveries("/workspace/a", 2).unwrap();
    assert_eq!(
        recent
            .iter()
            .map(|(_, payload)| payload["n"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2, 0]
    );
    assert_eq!(
        store
            .recent_tool_recoveries("/workspace/b", 2)
            .unwrap()
            .iter()
            .map(|(_, payload)| payload["n"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![3, 1]
    );
    assert!(store.recent_tool_recoveries("/workspace/a", 1025).is_err());
}

#[test]
fn executable_repairs_are_scoped_and_never_training_success_labels() {
    use crate::agent::repair_evidence::{CheckKind, CheckRun, FileChange, Tracker};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "case", &harness()).unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "repair", "prompt")
        .unwrap();
    let mut tracker = Tracker::default();
    let run = |sequence, exit_code| CheckRun {
        command: "cargo check".into(),
        cwd: "/workspace/a".into(),
        objective: "type correctness".into(),
        kind: CheckKind::SyntaxOnly,
        tool_call_id: format!("check-{sequence}"),
        evidence_ref: format!("artifact://{sequence}"),
        exit_code: Some(exit_code),
        sequence,
    };
    tracker.observe(run(1, 1), vec![]);
    let record = tracker
        .observe(
            run(2, 0),
            vec![FileChange {
                path: "src/lib.rs".into(),
                before_sha256: Some("a".repeat(64)),
                after_sha256: Some("b".repeat(64)),
                before_ref: Some("/state/before".into()),
                after_ref: Some("/state/after".into()),
            }],
        )
        .unwrap();
    let payload = serde_json::to_value(record).unwrap();
    assert!(
        store
            .executable_repair(trajectory, version, &payload)
            .unwrap()
    );
    assert!(
        !store
            .executable_repair(trajectory, version, &payload)
            .unwrap()
    );
    assert_eq!(
        store.recent_executable_repairs("/workspace/a", 1).unwrap()[0].1,
        payload
    );
    assert!(
        store
            .recent_executable_repairs("/workspace/b", 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .recent_executable_repairs("/workspace/a", 1025)
            .is_err()
    );
    assert!(store.tool_recoveries().unwrap().is_empty());
    store
        .finish_trajectory(trajectory, Some("done"), None)
        .unwrap();
    assert!(store.verified_successes(100.0, 100).unwrap().is_empty());
    let mut invalid = payload;
    invalid["before"]["exit_code"] = json!(0);
    assert!(
        store
            .executable_repair(trajectory, version, &invalid)
            .is_err()
    );
}
