use super::*;

#[test]
fn reports_a_working_route_and_why_a_broken_one_fails() {
    let mut config = Config {
        user_dir: tempfile::tempdir().unwrap().keep(),
        ..Config::default()
    };
    config.image.base_url = "http://127.0.0.1:9/v1".into();
    config.image.model = "local-painter".into();
    config.image.api = Some(crate::llm::profiles::ImageGenerationApi::MlxServe);
    let page = settings(&config);
    assert_eq!(page["ok"], true, "{page}");
    assert_eq!(page["model"], "local-painter");
    assert_eq!(page["api"], "MLX-Serve");
    assert!(
        page["settings"]
            .as_array()
            .unwrap()
            .contains(&json!("steps"))
    );
    assert!(page["key"].as_str().unwrap().starts_with("image_"));
    assert_eq!(page["entry"]["model"], "local-painter");
    assert_eq!(page["options"]["max_steps"], MAX_STEPS);

    let mut broken = Config {
        user_dir: tempfile::tempdir().unwrap().keep(),
        ..Config::default()
    };
    broken.model_roles.insert(
        "image".into(),
        crate::llm::model_config::ModelSelection::new("mlx", "/nowhere/painter"),
    );
    let page = settings(&broken);
    assert_eq!(page["ok"], false);
    assert!(!page["error"].as_str().unwrap().is_empty());
    assert_eq!(page["role"]["model"], "/nowhere/painter");
    assert!(page["options"]["sizes"].as_array().unwrap().len() > 1);
}
