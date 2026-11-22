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

#[test]
fn preview_only_returns_an_intact_raster_from_the_project_store() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(dir.path()).unwrap();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jBmcAAAAASUVORK5CYII=";
    let mut spool = store.spool().unwrap();
    spool.write_all(&STANDARD.decode(png).unwrap()).unwrap();
    let uri = spool.finish().unwrap();
    let read = |uri: &str| artifact(dir.path(), &json!({"uri":uri}));
    assert_eq!(
        read(&uri).unwrap(),
        json!({"media_type":"image/png","data":png})
    );
    assert!(read("artifact://../other").is_err());
    assert!(read(&store.retain("not an image").unwrap()).is_err());
}
