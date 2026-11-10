use super::*;

fn atlas(id: &str) -> Value {
    json!({"op":"map", "model_id":id, "tensors":[{"name":"model.layers.0.self_attn.q_proj.weight","role":"query"}],
        "knowledge":{"roles":{"query":{"function":"query projection","change_effect":"changes scores"}}}})
}

fn observation(steps: u64) -> Observation<'static> {
    Observation {
        outcome: "completed",
        steps,
        elapsed: Duration::from_secs(2),
    }
}

#[test]
fn observations_are_bounded_content_free_and_reset_on_identity_change() {
    let config = Config::default();
    let mut value = Value::Null;
    for step in 0..70 {
        value = merge(&config, value, atlas("a"), Some(observation(step)));
    }
    let rows = value["observations"].as_array().unwrap();
    assert_eq!(rows.len(), MAX_OBSERVATIONS);
    assert_eq!(rows[0]["steps"], 6);
    assert_eq!(rows[63]["steps"], 69);
    assert_eq!(rows[0]["causal_attribution"], false);
    assert!(rows[0].get("prompt").is_none());
    assert!(rows[0].get("answer").is_none());
    let refreshed = merge(&config, value, atlas("a"), None);
    assert_eq!(
        refreshed["observations"].as_array().unwrap().len(),
        MAX_OBSERVATIONS
    );
    let changed = merge(&config, refreshed, atlas("b"), Some(observation(1)));
    assert_eq!(changed["observations"].as_array().unwrap().len(), 1);
}

#[test]
fn configured_route_is_not_attributed_to_the_target() {
    let mut config = Config {
        model: "hosted-driver".into(),
        provider: Some("openai".into()),
        ..Config::default()
    };
    config.weights.model = "/local/checkpoint".into();
    let value = merge(&config, Value::Null, atlas("a"), Some(observation(1)));
    assert_eq!(value["target"], "/local/checkpoint");
    assert_eq!(
        value["observations"][0]["configured_model"],
        "hosted-driver"
    );
    assert_eq!(value["observations"][0]["causal_attribution"], false);
    let text = render(&value);
    assert!(text.contains("structural inference"));
    assert!(text.contains("not individual weights or model quality"));
    assert!(text.contains("External serving identity"));
}

#[test]
fn stored_snapshot_labels_target_mismatch_and_rejects_unknown_versions() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    let value = merge(&config, Value::Null, atlas("a"), None);
    crate::platform::atomic::private_dir(&directory(&config)).unwrap();
    let path = directory(&config).join("knowledge.json");
    write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        stored_report(&config).unwrap()["configured_target_matches"],
        true
    );
    config.weights.model = "another-checkpoint".into();
    let stored = stored_report(&config).unwrap();
    assert_eq!(stored["configured_target_matches"], false);
    assert!(render(&stored).contains("different configured target"));
    write_private(&path, br#"{"version":999}"#).unwrap();
    assert!(stored_report(&config).is_err());
}

#[test]
fn disabled_collection_does_not_access_models_or_create_state() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        state_dir: dir.path().join("absent"),
        model: "/missing/model".into(),
        ..Config::default()
    };
    assert!(!config.weights.knowledge);
    observe(&config, observation(1), &AtomicBool::new(false)).unwrap();
    assert!(!config.state_dir.exists());
    assert!(stored_report(&config).unwrap().is_null());
    assert!(render(&Value::Null).contains("--refresh"));
}

#[test]
fn knowledge_config_is_opt_in_and_strict() {
    let config: crate::config::weights::Config = toml::from_str("knowledge = true").unwrap();
    assert!(config.knowledge);
    assert!(toml::from_str::<crate::config::weights::Config>("knowlege = true").is_err());
}

#[cfg(unix)]
#[test]
fn installed_worker_maps_and_collects_without_inference_or_checkpoint_writes() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let config_path = model.join("config.json");
    std::fs::write(&config_path, r#"{"model_type":"gemma4"}"#).unwrap();
    let header = serde_json::to_vec(&json!({
        "model.layers.0.self_attn.q_proj.weight": {
            "dtype":"BF16", "shape":[2,2], "data_offsets":[0,8]
        }
    }))
    .unwrap();
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&[0; 8]);
    let shard = model.join("model.safetensors");
    std::fs::write(&shard, &bytes).unwrap();
    let user_dir = dir.path().join("user");
    let worker = crate::config::paths::python_dir(&user_dir).join("dial_worker.py");
    crate::platform::atomic::private_dir(&crate::config::paths::python_dir(&user_dir)).unwrap();
    let mut config = Config {
        model: model.to_string_lossy().into_owned(),
        state_dir: dir.path().join("state"),
        user_dir,
        worker,
        python: "/usr/bin/python3".into(),
        ..Config::default()
    };
    config.weights.knowledge = true;
    let cancel = AtomicBool::new(false);
    let mapped = refresh(&config, &cancel).unwrap();
    assert_eq!(mapped["atlas"]["tensors"][0]["role"], "query");
    assert!(mapped["observations"].as_array().unwrap().is_empty());
    observe(&config, observation(1), &cancel).unwrap();
    observe(&config, observation(2), &cancel).unwrap();
    let observed = stored_report(&config).unwrap();
    assert_eq!(observed["observations"].as_array().unwrap().len(), 2);
    assert_eq!(observed["atlas"], mapped["atlas"]);
    assert_eq!(std::fs::read(&shard).unwrap(), bytes);
    assert!(!directory(&config).join("runs").exists());

    std::fs::write(&config_path, r#"{"model_type":"gemma4","hidden_size":2}"#).unwrap();
    observe(&config, observation(3), &cancel).unwrap();
    let changed = stored_report(&config).unwrap();
    assert_eq!(changed["observations"].as_array().unwrap().len(), 1);
    assert_ne!(changed["atlas"]["model_id"], mapped["atlas"]["model_id"]);
    std::fs::write(&config_path, b"not JSON").unwrap();
    assert!(observe(&config, observation(4), &cancel).is_err());
    assert_eq!(stored_report(&config).unwrap(), changed);
    assert_eq!(std::fs::read(&shard).unwrap(), bytes);
}
