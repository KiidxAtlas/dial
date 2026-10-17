use super::*;
#[test]
fn upgrades_managed_worker_without_touching_custom_workers() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config {
        user_dir: root.path().to_owned(),
        worker: root.path().join("python/dial_worker.py"),
        ..Config::default()
    };
    std::fs::create_dir(root.path().join("python")).unwrap();
    std::fs::write(&config.worker, "old worker").unwrap();
    refresh_worker(&config).unwrap();
    assert_eq!(std::fs::read_to_string(&config.worker).unwrap(), WORKER);
    config.worker = root.path().join("custom.py");
    std::fs::write(&config.worker, "custom worker").unwrap();
    refresh_worker(&config).unwrap();
    assert_eq!(
        std::fs::read_to_string(&config.worker).unwrap(),
        "custom worker"
    );
}

#[test]
fn model_setup_verifies_exact_candidate_before_saving() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    use settings_store::Change;
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[Change::Set {
            path: "model".into(),
            value: "old-global".into(),
        }],
    )
    .unwrap();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "model".into(),
            value: "project-override".into(),
        }],
    )
    .unwrap();
    let before: Vec<_> = layout
        .config_files()
        .iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    for supported in [false, true] {
        let result = select_model(&layout, "org/requested", |candidate| {
            assert_eq!(candidate.model, "org/requested");
            assert!(candidate.endpoint.is_none());
            for (path, contents) in layout.config_files().iter().zip(&before) {
                assert_eq!(&std::fs::read(path).unwrap(), contents);
            }
            Ok(serde_json::json!({"inference": supported, "reason": "unsupported", "lora": false}))
        });
        assert_eq!(result.is_ok(), supported);
        if !supported {
            for (path, contents) in layout.config_files().iter().zip(&before) {
                assert_eq!(&std::fs::read(path).unwrap(), contents);
            }
        }
    }
    assert_eq!(
        settings_store::load_config(&layout, &toml::Table::new())
            .unwrap()
            .model,
        "org/requested"
    );
}

#[test]
fn cancelled_runtime_install_does_not_create_files() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    assert!(
        ensure_runtime_with(&layout, &AtomicBool::new(true), &|_| panic!("no installer")).is_err()
    );
    assert!(!layout.user_dir.exists());
}
