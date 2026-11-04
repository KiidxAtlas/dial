use super::*;

fn config(root: &Path) -> Config {
    Config {
        state_dir: root.join("state"),
        ..Config::default()
    }
}

fn analyze(layers: &[(f64, f64)], loss: f64) -> Value {
    json!({
        "op": "analyze",
        "created_at": 1,
        "baseline_loss": loss,
        "summary": {"heads_by_role": {"induction": 1}},
        "layers": layers.iter().enumerate().map(|(layer, (importance, entropy))| json!({
            "layer": layer, "importance": importance, "experts": {"entropy": entropy},
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn kept_runs_are_listed_newest_first_shown_and_capped() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut ids = Vec::new();
    for at in 1..=4u64 {
        let op = if at % 2 == 0 { "stats" } else { "map" };
        let mut value = json!({"op": op, "created_at": at, "experiment": {"name": "e1"}});
        ids.push(keep_for_tests(&config, &mut value, 3));
        assert_eq!(value["run_id"], json!(ids.last().unwrap()));
    }
    let listed = history(&config, None);
    let listed_ids: Vec<&str> = listed.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(
        listed_ids,
        vec![&*ids[3], &*ids[2], &*ids[1]],
        "oldest pruned"
    );
    assert_eq!(listed[0]["experiment"], "e1");
    assert_eq!(history(&config, Some("map")).len(), 1);
    assert_eq!(show_run(&config, &ids[3]).unwrap()["created_at"], 4);
    let pruned = format!("{:#}", show_run(&config, &ids[0]).unwrap_err());
    assert!(pruned.contains("no weights run"), "{pruned}");
    for bad in ["../evaluate", "", "A-1", "x/y"] {
        assert!(show_run(&config, bad).is_err(), "{bad:?} accepted");
    }
    assert_eq!(prune(&config, 1).unwrap(), 2);
    assert_eq!(history(&config, None).len(), 1);
    assert_eq!(prune(&config, 1).unwrap(), 0);
}

#[test]
fn prune_without_history_removes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(prune(&config(dir.path()), 0).unwrap(), 0);
}

#[test]
fn analyses_compare_per_layer_and_refuse_mismatches() {
    let before = analyze(&[(0.5, 0.9), (0.1, 0.8)], 1.5);
    let after = analyze(&[(0.25, 0.9), (0.1, 1.0)], 1.25);
    let diff = compare_analyses(&before, &after).unwrap();
    assert_eq!(diff["baseline_loss"], json!({"before": 1.5, "after": 1.25}));
    assert_eq!(diff["layers"][0]["importance"], -0.25);
    assert_eq!(diff["layers"][1]["importance"], 0.0);
    let entropy = diff["layers"][1]["routing_entropy"].as_f64().unwrap();
    assert!((entropy - 0.2).abs() < 1e-9);
    let shorter = analyze(&[(0.5, 0.9)], 1.0);
    assert!(compare_analyses(&before, &shorter).is_err());
    let map = json!({"op": "map", "layers": []});
    let error = format!("{:#}", compare_analyses(&map, &after).unwrap_err());
    assert!(error.contains("only analyze runs"), "{error}");
}
