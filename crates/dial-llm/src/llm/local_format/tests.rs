use super::*;

fn gguf(path: &Path) {
    fs::write(path, b"GGUF\x03\0\0\0rest").unwrap();
}

#[test]
fn detects_each_format_from_layout() {
    let root = tempfile::tempdir().unwrap();
    let mlx = root.path().join("mlx");
    fs::create_dir(&mlx).unwrap();
    fs::write(mlx.join("config.json"), "{}").unwrap();
    fs::write(mlx.join("model.safetensors"), b"x").unwrap();
    assert_eq!(detect(&mlx), Some(LocalFormat::Mlx));

    let splash = root.path().join("splash");
    for dir in ["target", "tokenizer"] {
        fs::create_dir_all(splash.join(dir)).unwrap();
    }
    fs::write(splash.join("manifest.json"), "{}").unwrap();
    assert_eq!(detect(&splash), Some(LocalFormat::Splash));

    let folder = root.path().join("gguf");
    fs::create_dir(&folder).unwrap();
    gguf(&folder.join("model-Q4_K_M.gguf"));
    assert_eq!(detect(&folder), Some(LocalFormat::Gguf));
    assert_eq!(
        detect(&folder.join("model-Q4_K_M.gguf")),
        Some(LocalFormat::Gguf)
    );
    assert_eq!(detect(&root.path().join("missing")), None);
}

#[test]
fn gguf_selection_skips_projectors_and_starts_at_the_first_shard() {
    let root = tempfile::tempdir().unwrap();
    gguf(&root.path().join("mmproj-model-f16.gguf"));
    gguf(&root.path().join("m-00002-of-00002.gguf"));
    gguf(&root.path().join("m-00001-of-00002.gguf"));
    assert_eq!(
        gguf_file(root.path()).unwrap().file_name().unwrap(),
        "m-00001-of-00002.gguf"
    );
}

#[test]
fn incomplete_split_models_are_not_usable_cached_weights() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("m-00001-of-00002.gguf");
    let second = root.path().join("m-00002-of-00002.gguf");
    gguf(&first);
    assert_eq!(gguf_file(&first), None);
    assert_eq!(gguf_file(root.path()), None);
    fs::write(&second, b"partial").unwrap();
    assert_eq!(gguf_file(root.path()), None);
    gguf(&second);
    assert_eq!(gguf_file(&second), Some(first.clone()));
    assert_eq!(gguf_file(root.path()), Some(first));
}

#[test]
fn multiple_cached_quantizations_require_an_explicit_file() {
    let root = tempfile::tempdir().unwrap();
    let selected = root.path().join("m-Q4.gguf");
    gguf(&selected);
    gguf(&root.path().join("m-Q8.gguf"));
    assert_eq!(gguf_file(root.path()), None);
    assert_eq!(gguf_file(&selected), Some(selected));
    gguf(&root.path().join("mmproj-f16.gguf"));
    assert_eq!(gguf_file(&root.path().join("mmproj-f16.gguf")), None);
}

#[test]
fn a_gguf_extension_without_the_magic_is_not_a_model() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.gguf"), b"not a model").unwrap();
    assert_eq!(detect(root.path()), None);
}
