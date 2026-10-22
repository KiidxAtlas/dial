use crate::config::steering::Config;
use crate::llm::steering::{MAX_STRENGTH, check, specification};

use super::*;
fn candidate() -> Value {
    json!({"version":1,"id":"a".repeat(64),"model_name":"test","dimension":2,"layer":0,"layer_count":2,"vector":[1.0,0.0],"pairs":8,"evaluation":{"passed":true,"artifact":"a".repeat(64),"strength":0.5,"cases":8,"baseline_passes":4,"steered_passes":7,"scope":"held-out response contracts","tool_nonregression":true,"repetition_nonregression":true,"latency_nonregression":true}})
}
#[test]
fn promotion_uses_the_same_admission_gate_as_runtime_loading() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        model: "test".into(),
        state_dir: root.path().into(),
        ..Default::default()
    };
    let pending = pending_path(&config, "imported-test").unwrap();
    let active = vector_path(&config);
    crate::platform::atomic::private_dir(pending.parent().unwrap()).unwrap();
    write_private(&active, b"existing vector").unwrap();
    for (pointer, value) in [
        ("/id", Value::Null),
        ("/evaluation/artifact", json!("another artifact")),
        ("/evaluation/tool_nonregression", json!(false)),
        ("/evaluation/strength", json!(0.25)),
        ("/model_name", json!("another model")),
    ] {
        let mut bundle = candidate();
        *bundle.pointer_mut(pointer).unwrap() = value;
        let bytes = serde_json::to_vec(&bundle).unwrap();
        write_private(&pending, &bytes).unwrap();
        assert!(
            promote(&config, "imported-test", true).is_err(),
            "{pointer}"
        );
        assert_eq!(fs::read(&pending).unwrap(), bytes);
        assert_eq!(fs::read(&active).unwrap(), b"existing vector");
    }
}

#[test]
fn promotion_locks_both_vectors_and_preserves_pending_on_refusal() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        model: "test".into(),
        state_dir: root.path().into(),
        ..Default::default()
    };
    let pending = pending_path(&config, "imported-test").unwrap();
    let active = vector_path(&config);
    let bytes = serde_json::to_vec(&candidate()).unwrap();
    {
        let _lock = lock_vector(&pending).unwrap();
        write_private(&pending, &bytes).unwrap();
        assert!(promote(&config, "imported-test", false).is_err());
        assert_eq!(fs::read(&pending).unwrap(), bytes);
        assert!(!active.exists());
    }
    {
        let _lock = lock_vector(&active).unwrap();
        assert!(promote(&config, "imported-test", false).is_err());
        assert_eq!(fs::read(&pending).unwrap(), bytes);
        assert!(!active.exists());
    }
    // Existing active bytes require explicit replacement even once both locks are available.
    write_private(&active, b"existing vector").unwrap();
    assert!(promote(&config, "imported-test", false).is_err());
    assert_eq!(fs::read(&active).unwrap(), b"existing vector");
    assert_eq!(fs::read(&pending).unwrap(), bytes);
    // Parallel subprocess tests can briefly inherit a lease until exec closes it.
    // Only wait after the intentional contention checks above; real errors still fail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match promote(&config, "imported-test", true) {
            Ok(_) => break,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("promotion after lease release failed: {error:#}"),
        }
    }
    assert_eq!(fs::read(&active).unwrap(), bytes);
    assert!(!pending.exists());
}

#[test]
fn promotion_revalidates_pending_after_evaluation_releases_its_lock() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        model: "test".into(),
        state_dir: root.path().into(),
        ..Default::default()
    };
    let pending = pending_path(&config, "imported-test").unwrap();
    let mut bundle = candidate();
    let lock = lock_vector(&pending).unwrap();
    write_private(&pending, &serde_json::to_vec(&bundle).unwrap()).unwrap();
    assert!(promote(&config, "imported-test", false).is_err());
    bundle["evaluation"]["passed"] = json!(false);
    let bytes = serde_json::to_vec(&bundle).unwrap();
    write_private(&pending, &bytes).unwrap();
    drop(lock);
    assert!(promote(&config, "imported-test", false).is_err());
    assert_eq!(fs::read(&pending).unwrap(), bytes);
    assert!(!vector_path(&config).exists());
    // As above, a concurrently forked subprocess can briefly inherit the released lease.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while lock_vector(&pending).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "refusal releases its leases"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn at(strength: f64) -> Value {
    let mut bundle = candidate();
    bundle["evaluation"]["strength"] = json!(strength);
    bundle
}
fn swept(strength: f64) -> Value {
    json!({"state":"awaiting evaluation","id":"a".repeat(64),"dimension":2,"layer":0,"pairs":8,"strength":strength,"margin":0.4,"baseline_margin":0.1,"sweep":[{"layer":0,"strength":strength,"margin":0.4}]})
}
fn auto_fixture() -> (tempfile::TempDir, AppConfig, crate::storage::store::Store) {
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        model: "test".into(),
        state_dir: dir.path().join("state"),
        ..Default::default()
    };
    let store =
        crate::storage::store::Store::open(&config.state_dir, "test", "case", &Default::default())
            .unwrap();
    (dir, config, store)
}
fn seed_pairs(config: &mut AppConfig, store: &crate::storage::store::Store) {
    seed_count(config, store, 8);
}
fn seed_count(config: &mut AppConfig, store: &crate::storage::store::Store, count: usize) {
    config.training.train_data = config.state_dir.join("train.jsonl");
    let version = store.active().unwrap().id;
    let mut lines = String::new();
    for n in 0..count {
        let context = json!([{"role":"user","content":format!("repair prompt {n}")}]);
        let tools =
            json!([{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]);
        let chosen = json!({"role":"assistant","content":format!("corrected {n}")});
        let rejected = json!({"role":"assistant","content":format!("incorrect {n}")});
        let id = store
            .begin_trajectory(version, "selfplay:files", "task", "prompt")
            .unwrap();
        store.tool_recovery(id,version,&json!({"project_root":config.project_root.to_string_lossy(),"context":context,"tools":tools,"corrected":chosen,"rejected":[rejected]})).unwrap();
        lines.push_str(&json!({"preference_pairs":[{"context":context,"tools":tools,"chosen":chosen,"rejected":rejected}]}).to_string());
        lines.push('\n');
    }
    fs::write(&config.training.train_data, lines).unwrap();
}
#[test]
fn automatic_prefers_newest_verified_repairs_with_a_bounded_pair_budget() {
    let (_dir, mut config, store) = auto_fixture();
    seed_count(&mut config, &store, 40);
    let mut observed = false;
    automatic_with(&config, &store, None, &mut |request| {
        assert_eq!(request["op"], "steering_extract");
        let pairs = request["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 32);
        assert_eq!(pairs[0]["positive"][0]["content"], "repair prompt 39");
        assert_eq!(pairs[31]["positive"][0]["content"], "repair prompt 8");
        observed = true;
        anyhow::bail!("stop fixture before inference")
    })
    .unwrap();
    assert!(observed);
}
#[test]
fn automatic_waits_for_provenance_and_never_loads_model_without_data() {
    let (_dir, mut config, store) = auto_fixture();
    config.training.train_data = config.state_dir.join("unverified.jsonl");
    fs::write(&config.training.train_data, json!({"preference_pairs":[{"context":[{"role":"user","content":"unverified"}],"chosen":{"role":"assistant","content":"good"},"rejected":{"role":"assistant","content":"bad"}}]}).to_string()).unwrap();
    let result = automatic_with(&config, &store, None, &mut |_| {
        panic!("no compute should start")
    })
    .unwrap();
    assert_eq!(result["pairs"], 0);
    assert!(!vector_path(&config).exists());
    config.steering.automatic = false;
    assert_eq!(
        automatic_with(&config, &store, None, &mut |_| panic!("disabled")).unwrap()["phase"],
        "disabled or unsupported"
    );
}
#[test]
fn vision_models_keep_the_evaluation_gate_and_normal_dataset_validation() {
    let (dir, mut config, _store) = auto_fixture();
    let model = dir.path().join("vision-model");
    fs::create_dir(&model).unwrap();
    fs::write(model.join("config.json"), r#"{"vision_config":{}}"#).unwrap();
    config.model = model.to_string_lossy().into_owned();
    config.steering.enabled = true;
    config.steering.automatic = false;
    let path = vector_path(&config);
    crate::platform::atomic::private_dir(path.parent().unwrap()).unwrap();
    let mut vector = candidate();
    vector["model_name"] = json!(config.model);
    vector["origin"] = json!("automatic verified repairs");
    write_private(&path, &serde_json::to_vec(&vector).unwrap()).unwrap();
    assert_eq!(status(&config)["state"], "configured");
    assert!(specification(&config, &config.model).unwrap().is_some());
    // Eligible vision models still reject missing datasets before starting any worker.
    assert!(
        run(
            &config,
            "extract",
            &dir.path().join("missing.json"),
            None,
            None
        )
        .is_err()
    );
    vector["evaluation"]["passed"] = json!(false);
    write_private(&path, &serde_json::to_vec(&vector).unwrap()).unwrap();
    assert!(specification(&config, &config.model).is_err());
}
#[test]
fn automatic_admits_only_passing_candidates_and_preserves_last_good_on_failure() {
    let (_dir, mut config, store) = auto_fixture();
    seed_pairs(&mut config, &store);
    let mut calls = 0;
    let result = automatic_with(&config, &store, None, &mut |request| {
        calls += 1;
        let output = Path::new(request["output"].as_str().unwrap());
        write_private(output, &serde_json::to_vec(&at(37.5))?)?;
        if request["op"] == "steering_extract" {
            assert!(request["layer"].is_null() && request["sweep"] == true);
            assert_eq!(request["pairs"].as_array().unwrap().len(), 8);
            return Ok(swept(37.5));
        }
        // The swept absolute strength reaches evaluation unchanged, beyond the 0..2 setting.
        assert_eq!(request["strength"], 37.5);
        assert_eq!(request["cases"].as_array().unwrap().len(), 8);
        Ok(at(37.5)["evaluation"].clone())
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert!(result["phase"].as_str().unwrap().starts_with("admitted"));
    assert_eq!(result["sweep"]["strength"], 37.5);
    let admitted = specification(&config, "test").unwrap().unwrap();
    assert_eq!(admitted.strength, 37.5);
    automatic_with(&config, &store, None, &mut |_| {
        panic!("cooldown should skip")
    })
    .unwrap();
    let active = fs::read(vector_path(&config)).unwrap();
    fs::remove_file(vector_path(&config).with_extension("automatic.json")).unwrap();
    let result = automatic_with(&config, &store, None, &mut |request| {
        let mut bad = candidate();
        bad["evaluation"]["passed"] = json!(false);
        write_private(
            Path::new(request["output"].as_str().unwrap()),
            &serde_json::to_vec(&bad)?,
        )?;
        Ok(bad["evaluation"].clone())
    })
    .unwrap();
    assert!(result["phase"].as_str().unwrap().contains("not activated"));
    assert_eq!(fs::read(vector_path(&config)).unwrap(), active);
    config.steering.automatic = false;
    assert!(specification(&config, "test").unwrap().is_none());
}
#[test]
fn passing_candidate_cannot_reduce_incumbent_score() {
    let (_dir, mut config, store) = auto_fixture();
    seed_pairs(&mut config, &store);
    let path = vector_path(&config);
    crate::platform::atomic::private_dir(path.parent().unwrap()).unwrap();
    let mut incumbent = candidate();
    incumbent["origin"] = json!("automatic verified repairs");
    incumbent["automatic_adapter"] = Value::Null;
    incumbent["evaluation"]["steered_passes"] = json!(8);
    write_private(&path, &serde_json::to_vec(&incumbent).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();
    let result = automatic_with(&config, &store, None, &mut |request| {
        let output = Path::new(request["output"].as_str().unwrap());
        if output == path.with_extension("incumbent.json") {
            assert_eq!(request["strength"], 0.5);
            return Ok(incumbent["evaluation"].clone());
        }
        write_private(output, &serde_json::to_vec(&at(3.0))?)?;
        if request["op"] == "steering_extract" {
            return Ok(swept(3.0));
        }
        Ok(at(3.0)["evaluation"].clone())
    })
    .unwrap();
    assert!(result["phase"].as_str().unwrap().contains("regressed"));
    assert_eq!(fs::read(&path).unwrap(), before);
}
#[test]
fn manual_vectors_are_preserved_and_heldout_cases_are_varied_exact_contracts() {
    let (_dir, mut config, store) = auto_fixture();
    seed_pairs(&mut config, &store);
    crate::platform::atomic::private_dir(vector_path(&config).parent().unwrap()).unwrap();
    write_private(
        &vector_path(&config),
        &serde_json::to_vec(&candidate()).unwrap(),
    )
    .unwrap();
    let result = automatic_with(&config, &store, None, &mut |_| {
        panic!("manual vector must be preserved")
    })
    .unwrap();
    assert!(
        result["phase"]
            .as_str()
            .unwrap()
            .contains("manual vector preserved")
    );
    assert!(specification(&config, "test").unwrap().is_none());
    let cases = automatic_cases();
    assert_eq!(cases.len(), 8);
    assert_eq!(
        cases
            .iter()
            .map(|c| c["expected_tool"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
    assert!(
        cases
            .iter()
            .all(|c| !c["expected_arguments"].as_object().unwrap().is_empty())
    );
}
#[test]
fn heldout_prompt_is_excluded_even_with_added_system_context() {
    let cases = automatic_cases();
    let pair = json!({"context":[{"role":"system","content":"follow rules"},cases[0]["messages"][0]],"chosen":{"role":"assistant","content":"good"},"rejected":{"role":"assistant","content":"bad"}});
    assert!(contrast_pair(&pair).is_none());
    let config = AppConfig::default();
    for role in ["small", "coach"] {
        assert!(!config.for_role(role).steering.automatic);
    }
}
#[test]
fn disabled_never_requires_a_vector_and_names_cannot_escape_state() {
    let config = AppConfig::default();
    assert!(specification(&config, "test").unwrap().is_none());
    for name in ["../x", "", "x/y"] {
        let config = Config {
            vector: name.into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
    assert!(
        Config {
            strength: f64::NAN,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}
#[test]
fn evaluation_must_improve_and_pass_all_gates_at_exact_strength() {
    let original = candidate();
    check(&original, 0.5).unwrap();
    assert!(check(&original, 1.).is_err());
    check(&at(MAX_STRENGTH), MAX_STRENGTH).unwrap();
    for strength in [MAX_STRENGTH * 2.0, f64::INFINITY, 0.0] {
        assert!(check(&at(strength), strength).is_err(), "{strength}");
    }
    for (key, value) in [
        ("steered_passes", json!(4)),
        ("cases", json!(7)),
        ("tool_nonregression", json!(false)),
        ("latency_nonregression", json!(false)),
        ("repetition_nonregression", json!(false)),
    ] {
        let mut item = original.clone();
        item["evaluation"][key] = value;
        assert!(check(&item, 0.5).is_err(), "{key}");
    }
}
#[test]
fn missing_vector_explains_path_and_recovery_without_creating_it() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        state_dir: root.path().into(),
        steering: Config {
            enabled: true,
            automatic: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let path = vector_path(&config);
    let reason = specification(&config, "test").unwrap_err().to_string();
    assert!(reason.contains("selected vector 'coding' is missing"));
    assert!(reason.contains(&path.display().to_string()));
    assert!(reason.contains("Settings > Model > Activation steering"));
    assert!(!path.exists());
}
#[test]
fn admission_is_model_scoped_and_content_change_restarts_profile() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        model: "test".into(),
        state_dir: root.path().into(),
        steering: Config {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let path = vector_path(&config);
    crate::platform::atomic::private_dir(path.parent().unwrap()).unwrap();
    let mut item = candidate();
    write_private(&path, &serde_json::to_vec(&item).unwrap()).unwrap();
    let first = specification(&config, "test").unwrap().unwrap();
    assert!(specification(&config, "another").is_err());
    item["evaluation"]["baseline_passes"] = json!(3);
    write_private(&path, &serde_json::to_vec(&item).unwrap()).unwrap();
    assert_ne!(first, specification(&config, "test").unwrap().unwrap());
}
