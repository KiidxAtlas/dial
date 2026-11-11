use super::ModelSelection;

#[test]
fn a_local_gguf_model_reads_gguf_while_its_identity_keeps_mlx() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("qwen3-8b-Q4_K_M.gguf");
    std::fs::write(&file, b"GGUF\x03\0\0\0").unwrap();
    let selection = ModelSelection::new("mlx", file.to_str().unwrap());

    assert_eq!(selection.display_provider(), "gguf");
    assert_eq!(selection.display_label(), "gguf/qwen3-8b-Q4_K_M.gguf");
    assert_eq!(
        selection.display_name(),
        "mlx/qwen3-8b-Q4_K_M.gguf",
        "saved worker records compare display_name, so it keeps the provider id"
    );
    assert_eq!(selection.provider, "mlx");
}

#[test]
fn other_models_keep_their_provider() {
    let temp = tempfile::tempdir().unwrap();
    let mlx = temp.path().join("mlx-model");
    std::fs::create_dir(&mlx).unwrap();
    std::fs::write(mlx.join("config.json"), b"{}").unwrap();
    std::fs::write(mlx.join("model.safetensors"), b"x").unwrap();
    let mlx = ModelSelection::new("mlx", mlx.to_str().unwrap());
    assert_eq!(mlx.display_provider(), "mlx");
    assert_eq!(mlx.display_label(), mlx.display_name());

    let hosted = ModelSelection::new("openrouter", "qwen/qwen3-coder");
    assert_eq!(hosted.display_provider(), "openrouter");
    assert_eq!(hosted.display_label(), "openrouter/qwen/qwen3-coder");

    let lm_studio = ModelSelection::new("lm-studio", "qwen3-8b");
    assert_eq!(lm_studio.display_label(), "lm-studio/qwen3-8b");
}

#[test]
fn a_served_mlx_endpoint_is_not_relabelled() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("m.gguf");
    std::fs::write(&file, b"GGUF\x03\0\0\0").unwrap();
    let mut selection = ModelSelection::new("mlx", file.to_str().unwrap());
    selection.base_url = Some("http://127.0.0.1:8080/v1".into());
    assert_eq!(selection.display_provider(), "mlx");
}
