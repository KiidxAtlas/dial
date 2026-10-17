use super::*;

#[test]
fn dataset_manifest_accepts_append_but_detects_replacement_and_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("train.jsonl");
    fs::write(&path, "{\"n\":1}\n").unwrap();
    let original = training_prefix_digest(&path, 8).unwrap();
    fs::write(&path, "{\"n\":1}\n{\"n\":2}\n").unwrap();
    assert_eq!(training_prefix_digest(&path, 8).unwrap(), original);
    fs::write(&path, "{\"n\":3}\n{\"n\":2}\n").unwrap();
    assert_ne!(training_prefix_digest(&path, 8).unwrap(), original);
    fs::write(&path, "short").unwrap();
    assert!(training_prefix_digest(&path, 8).is_err());
}

#[test]
fn incremental_window_caps_delta_replay_and_preserves_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("train.jsonl");
    fs::write(
        &path,
        (0..40)
            .map(|i| format!("{{\"n\":{i}}}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let replay = (0..20)
        .map(|i| format!("{{\"r\":{i}}}"))
        .collect::<Vec<_>>();
    let (rows, cursor) = bounded_training_records(&path, 0, &replay).unwrap();
    assert_eq!(rows.len(), 48);
    assert_eq!(rows[15], "{\"r\":15}");
    assert_eq!(rows[16], "{\"n\":0}");
    assert_eq!(rows[47], "{\"n\":31}");
    let (next, _) = bounded_training_records(&path, cursor, &[]).unwrap();
    assert_eq!(next[0], "{\"n\":32}");
}

#[test]
fn cursor_reset_and_partial_line_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("train.jsonl");
    fs::write(&path, "{\"a\":1}\n{\"b\":2}").unwrap();
    let (rows, cursor) = bounded_training_records(&path, 99, &[]).unwrap();
    assert_eq!(rows, vec!["{\"a\":1}"]);
    assert_eq!(cursor, 8);
    assert!(bounded_training_records(&path, 1, &[]).is_err());
}

#[test]
fn validation_window_and_default_iterations_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("valid.jsonl");
    fs::write(
        &path,
        (0..40)
            .map(|i| format!("{{\"n\":{i}}}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let sample = bounded_validation_records(&path).unwrap();
    assert_eq!(sample.len(), 32);
    assert_eq!(sample[31], "{\"n\":31}");
    let training = crate::config::types::TrainingConfig::default();
    assert_eq!(training.iters, 32);
    assert_eq!(training.interval_secs, 6 * 3600);
}
#[test]
fn target_priorities_come_from_weak_verified_train_families() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        use_case: "test".into(),
        ..Config::default()
    };
    let store = Store::open(dir.path(), "test-model", &config.use_case, &config.harness).unwrap();
    let signals = Signals {
        selfplay_rates: vec![
            ("read_line".into(), 0, 4),
            ("create_file".into(), 4, 4),
            ("move_file".into(), 0, 4),
        ],
        tool_errors: BTreeMap::from([("read".into(), 2)]),
        ..Signals::default()
    };
    let priorities = targeted_selfplay_priorities(&store, &signals).unwrap();
    assert_eq!(priorities.first().map(String::as_str), Some("read_line"));
    assert!(priorities.contains(&"list_by_ext".to_owned()));
    assert!(
        !priorities.contains(&"move_file".to_owned()),
        "gate-only families cannot enter self-play"
    );
    let tasks = curriculum::generate_targeted(Role::SelfPlay, 12, 21, &priorities);
    assert!(tasks.iter().all(|task| {
        priorities.contains(&task.family.to_owned())
            || curriculum::families().iter().any(|family| {
                family.name == task.family
                    && family.split == curriculum::Split::Train
                    && family.needs_tools
            })
    }));
}

#[test]
fn practice_taxonomy_distinguishes_empty_results_and_behavioral_checks() {
    let no_hits = json!({"kind":"tool_result","data":{"name":"grep","output":"No matches"}});
    assert_eq!(practice_category(&no_hits, false), None);
    assert_eq!(practice_category(&no_hits, true), Some("negative_search"));
    let syntax = json!({"kind":"verification","data":{"passed":true,"category":"syntax_only"}});
    assert_eq!(practice_category(&syntax, true), None);
    let failed_syntax =
        json!({"kind":"verification","data":{"passed":false,"category":"syntax_only"}});
    assert_eq!(practice_category(&failed_syntax, true), None);
    let behavioral = json!({"kind":"verification","data":{"passed":false,"category":"behavioral"}});
    assert_eq!(
        practice_category(&behavioral, false),
        Some("semantic_validation")
    );
    let config = json!({"kind":"tool_error","data":{"name":"bash","output":"invalid configuration: unknown field PRIVATE_CODE"}});
    assert_eq!(practice_category(&config, false), Some("config_precedence"));
}

#[test]
fn recurring_failures_count_trajectories_without_copying_private_content() {
    use crate::config::types::Event;
    let dir = tempfile::tempdir().unwrap();
    let config = Config::default();
    let store = Store::open(dir.path(), "test-model", "test", &config.harness).unwrap();
    let active = store.active().unwrap();
    for _ in 0..2 {
        let id = store
            .begin_trajectory(active.id, "live", "task", "PRIVATE_USER_CODE")
            .unwrap();
        for step in 0..3 {
            store
                .event(&Event {
                    trajectory: id,
                    version: active.id,
                    domain: "live".into(),
                    task: "task".into(),
                    step,
                    kind: "tool_error".into(),
                    data: json!({"name":"bash","output":"invalid configuration PRIVATE_USER_CODE"}),
                    score: None,
                })
                .unwrap();
        }
        store.finish_trajectory(id, None, Some("failed")).unwrap();
    }
    let signals = collect_signals(&store, &active, None).unwrap();
    assert_eq!(
        signals.recurring_failures,
        BTreeMap::from([("config_precedence".into(), 2)])
    );
    assert_eq!(signals.tool_errors, BTreeMap::from([("bash".into(), 2)]));
    let priorities = targeted_selfplay_priorities(&store, &signals).unwrap();
    assert_eq!(
        priorities.first().map(String::as_str),
        Some("config_precedence")
    );
    let tasks = curriculum::generate_targeted(Role::SelfPlay, 12, 9, &priorities);
    assert!(tasks.iter().all(|task| {
        !task.prompt.contains("PRIVATE_USER_CODE")
            && task
                .files
                .values()
                .all(|text| !text.contains("PRIVATE_USER_CODE"))
    }));
}

#[test]
fn single_category_occurrence_does_not_override_practice_and_gate_labels_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::default();
    let store = Store::open(dir.path(), "test-model", "test", &config.harness).unwrap();
    let mut signals = Signals {
        recurring_failures: BTreeMap::from([
            ("config_precedence".into(), 1),
            ("config_layers".into(), 100),
            ("semantic_validation".into(), 2),
        ]),
        ..Signals::default()
    };
    assert_eq!(
        targeted_selfplay_priorities(&store, &signals).unwrap(),
        vec!["semantic_validation"]
    );
    signals
        .recurring_failures
        .insert("config_precedence".into(), 3);
    assert_eq!(
        targeted_selfplay_priorities(&store, &signals).unwrap(),
        vec!["config_precedence", "semantic_validation"]
    );
}
