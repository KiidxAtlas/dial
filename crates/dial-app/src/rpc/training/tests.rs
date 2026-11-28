use super::*;

#[test]
fn preflight_checks_weights_not_names_and_refusals_have_no_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), root.path().join("user")).unwrap();
    let config = Config::default();
    let source = Session::ephemeral();
    let file = root.path().join("edition.gguf");
    std::fs::write(&file, b"GGUF").unwrap();
    let state = target(&layout, &config, file.to_str().unwrap())
        .unwrap()
        .state_dir;
    assert_eq!(
        status(&layout, &config, &json!({"model":file})).unwrap()["format"],
        "gguf"
    );
    for (action_name, confirm, code) in [
        ("start", false, ErrorCode::InvalidParams),
        ("start", true, ErrorCode::Refused),
        ("approve", true, ErrorCode::InvalidParams),
        ("stop", true, ErrorCode::NotFound),
    ] {
        let error = action(
            &layout,
            &config,
            &source,
            &json!({"model":file,"action":action_name,"confirm":confirm}),
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            code
        );
    }
    assert!(
        action(
            &layout,
            &config,
            &source,
            &json!({"model":file,"action":"approve","confirm":true,"session_id":"wrong-session"})
        )
        .unwrap_err()
        .to_string()
        .contains("session changed")
    );
    std::fs::write(&file, b"not weights").unwrap();
    let preflight = status(&layout, &config, &json!({"model":file})).unwrap();
    assert!(preflight["format"].is_null() && preflight["unavailable"].is_string());
    assert!(!state.exists());
}

#[test]
fn job_controls_target_only_the_reviewed_model_and_ignore_stale_state() {
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), root.path().join("user")).unwrap();
    let saved = Config {
        use_case: "coding".into(),
        ..Config::default()
    };
    let config = target(&layout, &saved, "selected-base").unwrap();
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    store
        .set_meta(
            "auto_status",
            r#"{"state":"running","stop_supported":true}"#,
        )
        .unwrap();
    let status_now = || status(&layout, &saved, &json!({"model":"selected-base"})).unwrap();
    let act = |op| {
        action(
            &layout,
            &saved,
            &Session::ephemeral(),
            &json!({"model":"selected-base","action":op,"confirm":true}),
        )
    };
    assert_eq!(status_now()["job"]["active"], false);
    assert!(act("stop").is_err());
    let lock = auto::lock_file(&config.state_dir.join("control.lock")).unwrap();
    lock.try_lock().unwrap();
    assert_eq!(status_now()["job"]["active"], true);
    assert_eq!(act("stop").unwrap()["stop_requested"], true);
    assert!(act("resume").is_err());
    drop(lock);
    assert_eq!(status_now()["paused"], true);
    act("resume").unwrap();
    assert_eq!(status_now()["paused"], false);
    assert!(
        !target(&layout, &saved, "other-base")
            .unwrap()
            .state_dir
            .exists()
    );
}

#[test]
fn training_status_reports_exl3_config_and_export_action_validation() {
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), root.path().join("user")).unwrap();
    let config = Config::default();
    let source = Session::ephemeral();

    // EXL3 checkpoint detection and structured training_status fields.
    let exl3 = root.path().join("qwen-exl3");
    std::fs::create_dir(&exl3).unwrap();
    std::fs::write(
        exl3.join("config.json"),
        r#"{"quantization_config":{"quant_method":"exl3","bits":4}}"#,
    )
    .unwrap();
    std::fs::write(exl3.join("model.safetensors"), b"x").unwrap();
    let st = status(&layout, &config, &json!({"model": exl3})).unwrap();
    assert_eq!(st["format"], "exl3");
    assert!(st["unavailable"].is_string());
    assert_eq!(st["config"]["fine_tune_type"], "lora");
    assert_eq!(st["config"]["target_modules"], "all-linear");
    assert_eq!(st["config"]["rank"], 16);
    assert_eq!(st["config"]["iters"], 100);
    assert_eq!(st["config"]["gate_mode"], "auto");
    assert_eq!(st["capabilities"]["peft_export"], true);
    assert_eq!(st["capabilities"]["gguf_lora_export"], true);

    // Export and prepare actions require an absolute, non-existing output path.
    let existing = root.path().join("already-here");
    std::fs::create_dir(&existing).unwrap();
    for (act, out, expected_code) in [
        ("export_peft", "relative/out", ErrorCode::InvalidParams),
        (
            "export_gguf_lora",
            existing.to_str().unwrap(),
            ErrorCode::Conflict,
        ),
        ("export_gguf", "relative/out.gguf", ErrorCode::InvalidParams),
        ("export_exl3", "relative/exl3", ErrorCode::InvalidParams),
        ("prepare_gguf", "relative/prep", ErrorCode::InvalidParams),
    ] {
        let err = action(
            &layout,
            &config,
            &source,
            &json!({"model": exl3, "action": act, "confirm": true, "output": out}),
        )
        .unwrap_err();
        assert_eq!(
            err.downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            expected_code,
            "action {act} with output {out}"
        );
    }
}
