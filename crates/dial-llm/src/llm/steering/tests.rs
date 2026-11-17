use super::*;

#[test]
fn display_model_name_is_not_the_runtime_weight_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig {
        state_dir: root.path().to_path_buf(),
        ..Default::default()
    };
    config.steering.enabled = true;
    config.steering.strength = 0.5;
    let id = "a".repeat(64);
    let bundle = serde_json::json!({
        "version":1,"model_name":"org/download-id","model":"b".repeat(64),"adapter":null,
        "id":id,"dimension":2,"layer_count":2,"layer":0,"vector":[1.0,0.0],"pairs":8,
        "evaluation":{"passed":true,"scope":"held-out response contracts","artifact":id,
        "strength":0.5,"cases":8,"baseline_passes":4,"steered_passes":7,
        "tool_nonregression":true,"repetition_nonregression":true,"latency_nonregression":true}
    });
    let path = vector_path(&config);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    // Weight and adapter fingerprints are validated by Python bind before installing any hook.
    // The server sees a resolved snapshot path, not necessarily the original download ID.
    let from_id = specification(&config, "org/download-id").unwrap().unwrap();
    let from_snapshot = specification(&config, "/cache/models/snapshots/resolved")
        .unwrap()
        .unwrap();
    assert_eq!(from_id, from_snapshot);
}
