use super::{
    Component, components, is_checkpoint_path, is_mlx_pack, loaded, mlx_serve_args, served_model,
    server_args, transformer_source,
};
use serde_json::json;
use std::fs;
use std::path::Path;

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A minimal safetensors file holding only a header naming `tensors`.
fn safetensors(path: &Path, tensors: &[&str]) {
    let mut header = serde_json::Map::new();
    for name in tensors {
        header.insert(
            (*name).to_owned(),
            json!({"dtype": "F16", "shape": [1], "data_offsets": [0, 2]}),
        );
    }
    let header = serde_json::to_vec(&header).unwrap();
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&[0, 0]);
    write(path, &bytes);
}

#[test]
fn single_file_checkpoint_loads_as_model() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sd15.safetensors");
    safetensors(&file, &["model.diffusion_model.x"]);
    assert_eq!(
        components(&file).unwrap(),
        vec![Component::new("--model", file.clone())]
    );
    assert!(is_checkpoint_path(&file));
    let gguf = dir.path().join("chat.gguf");
    write(&gguf, b"GGUF");
    assert!(!is_checkpoint_path(&gguf));
}

#[test]
fn diffusers_unet_layout_loads_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    safetensors(
        &dir.path().join("unet/diffusion_pytorch_model.safetensors"),
        &["conv_in.weight"],
    );
    safetensors(
        &dir.path().join("vae/diffusion_pytorch_model.safetensors"),
        &["decoder.x"],
    );
    write(&dir.path().join("text_encoder/config.json"), b"{}");
    assert_eq!(
        components(dir.path()).unwrap(),
        vec![Component::new("--model", dir.path().to_path_buf())]
    );
    assert!(is_checkpoint_path(dir.path()));
}

#[test]
fn transformer_layout_maps_each_component_by_declared_encoder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    safetensors(
        &root.join("transformer/diffusion_pytorch_model.safetensors"),
        &["single_transformer_blocks.0.x"],
    );
    write(
        &root.join("transformer/diffusion_pytorch_model.fp16.safetensors"),
        b"",
    );
    safetensors(
        &root.join("vae/diffusion_pytorch_model.safetensors"),
        &["decoder.x"],
    );
    write(
        &root.join("text_encoder/config.json"),
        br#"{"architectures":["CLIPTextModel"],"hidden_size":768}"#,
    );
    safetensors(&root.join("text_encoder/model.safetensors"), &["a"]);
    write(
        &root.join("text_encoder_2/config.json"),
        br#"{"architectures":["T5EncoderModel"]}"#,
    );
    write(
        &root.join("text_encoder_2/model.safetensors.index.json"),
        br#"{"weight_map":{"a":"model-00001-of-00002.safetensors"}}"#,
    );
    let plan = components(root).unwrap();
    assert_eq!(
        plan,
        vec![
            Component::new(
                "--diffusion-model",
                root.join("transformer/diffusion_pytorch_model.safetensors")
            ),
            Component::new(
                "--vae",
                root.join("vae/diffusion_pytorch_model.safetensors")
            ),
            Component::new("--clip_l", root.join("text_encoder/model.safetensors")),
            Component::new(
                "--t5xxl",
                root.join("text_encoder_2/model.safetensors.index.json")
            ),
        ]
    );
    let args = server_args(&plan, root, 4321);
    assert_eq!(
        args[..4],
        ["--listen-ip", "127.0.0.1", "--listen-port", "4321"].map(std::ffi::OsString::from)
    );
    assert!(args.contains(&"--t5xxl".into()));
}

#[test]
fn language_model_encoder_loads_as_llm() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    safetensors(
        &root.join("diffusion_pytorch_model.safetensors"),
        &["blocks.0.x"],
    );
    safetensors(
        &root.join("vae/diffusion_pytorch_model.safetensors"),
        &["d"],
    );
    write(
        &root.join("text_encoder/config.json"),
        br#"{"architectures":["Qwen3ForCausalLM"]}"#,
    );
    safetensors(&root.join("text_encoder/model.safetensors"), &["a"]);
    let plan = components(root).unwrap();
    assert_eq!(plan[0].flag, "--diffusion-model");
    assert_eq!(
        plan[2],
        Component::new("--llm", root.join("text_encoder/model.safetensors"))
    );
}

/// FLUX.2 klein as Dial downloads it: Diffusers VAE and Qwen3 encoder, with the transformer as the
/// repository-root original because stable-diffusion.cpp misreads its Diffusers tensor names.
fn flux2_layout(root: &Path, diffusers_transformer: bool) {
    write(
        &root.join("transformer/config.json"),
        br#"{"_class_name":"Flux2Transformer2DModel"}"#,
    );
    if diffusers_transformer {
        safetensors(
            &root.join("transformer/diffusion_pytorch_model.safetensors"),
            &["double_stream_modulation_img.linear.weight"],
        );
    }
    safetensors(
        &root.join("vae/diffusion_pytorch_model.safetensors"),
        &["decoder.conv_in.weight"],
    );
    write(
        &root.join("text_encoder/config.json"),
        br#"{"architectures":["Qwen3ForCausalLM"]}"#,
    );
    safetensors(&root.join("text_encoder/model.safetensors"), &["a"]);
}

#[test]
fn flux2_loads_the_root_original_not_the_diffusers_transformer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    flux2_layout(root, false);
    let original = root.join("flux-2-klein-4b.safetensors");
    safetensors(&original, &["double_blocks.0.img_attn.qkv.weight"]);
    assert_eq!(transformer_source(root), root);
    let plan = components(root).unwrap();
    assert_eq!(plan[0], Component::new("--diffusion-model", original));
    assert_eq!(
        plan[1],
        Component::new(
            "--vae",
            root.join("vae/diffusion_pytorch_model.safetensors")
        )
    );
    assert_eq!(
        plan[2],
        Component::new("--llm", root.join("text_encoder/model.safetensors"))
    );
}

#[test]
fn flux2_without_the_root_original_is_refused_with_how_to_fetch_it() {
    let dir = tempfile::tempdir().unwrap();
    flux2_layout(dir.path(), true);
    let error = format!("{:#}", components(dir.path()).unwrap_err());
    assert!(
        error.contains("cannot read the Diffusers weights"),
        "{error}"
    );
    assert!(error.contains("Get models"), "{error}");
}

#[test]
fn a_transformer_folder_without_weights_reads_the_root_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("transformer/config.json"), b"{}");
    safetensors(&root.join("model.safetensors"), &["x"]);
    safetensors(
        &root.join("vae/diffusion_pytorch_model.safetensors"),
        &["d"],
    );
    assert_eq!(transformer_source(root), root);
    assert_eq!(
        components(root).unwrap()[0],
        Component::new("--diffusion-model", root.join("model.safetensors"))
    );
    // Diffusers weights in an architecture stable-diffusion.cpp maps stay in use.
    safetensors(
        &root.join("transformer/diffusion_pytorch_model.safetensors"),
        &["x"],
    );
    assert_eq!(transformer_source(root), root.join("transformer"));
}

#[test]
fn mlx_quantized_packs_are_refused_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    safetensors(
        &root.join("transformer/0.safetensors"),
        &["blocks.0.weight", "blocks.0.scales", "blocks.0.biases"],
    );
    safetensors(&root.join("vae/0.safetensors"), &["d"]);
    let error = components(root).unwrap_err().to_string();
    assert!(error.contains("MLX-quantized"), "{error}");

    let declared = tempfile::tempdir().unwrap();
    write(
        &declared.path().join("config.json"),
        br#"{"quantization":{"bits":4,"method":"mflux"}}"#,
    );
    let error = components(declared.path()).unwrap_err().to_string();
    assert!(error.contains("declares mflux quantization"), "{error}");
}

#[test]
fn ambiguous_or_empty_directories_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        components(dir.path())
            .unwrap_err()
            .to_string()
            .contains("no image weights")
    );
    safetensors(&dir.path().join("a.safetensors"), &["x"]);
    safetensors(&dir.path().join("b.safetensors"), &["x"]);
    assert!(
        components(dir.path())
            .unwrap_err()
            .to_string()
            .contains("several")
    );
}

#[test]
fn model_status_reports_ready_and_load_failures() {
    assert!(loaded(&json!({"state": "ready"})).unwrap());
    assert!(!loaded(&json!({"state": "loading"})).unwrap());
    assert!(!loaded(&json!({"state": "empty"})).unwrap());
    let error = loaded(&json!({"state": "error", "error": "unknown arch"})).unwrap_err();
    assert!(error.to_string().contains("unknown arch"));
}

#[test]
fn mlx_packs_are_told_apart_from_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("pack");
    safetensors(
        &pack.join("transformer/model.safetensors"),
        &["blocks.0.weight", "blocks.0.scales", "blocks.0.biases"],
    );
    fs::create_dir_all(pack.join("vae")).unwrap();
    fs::create_dir_all(pack.join("text_encoder")).unwrap();
    assert!(is_mlx_pack(&pack));
    let plain = dir.path().join("plain");
    safetensors(
        &plain.join("transformer/model.safetensors"),
        &["blocks.0.weight"],
    );
    assert!(!is_mlx_pack(&plain));
    assert!(!is_mlx_pack(&dir.path().join("missing")));
}

#[test]
fn mlx_serve_listens_on_loopback_only() {
    let args: Vec<String> = mlx_serve_args(Path::new("/m/pack"), 4321)
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect();
    assert_eq!(
        args,
        [
            "--model",
            "/m/pack",
            "--serve",
            "--host",
            "127.0.0.1",
            "--port",
            "4321"
        ]
    );
}

#[test]
fn mlx_serve_listing_yields_the_ready_image_model() {
    assert_eq!(served_model(&json!({"data": []})).unwrap(), None);
    assert_eq!(
        served_model(&json!({"data": [{"id": "p", "state": "loading"}]})).unwrap(),
        None
    );
    // The listing MLX-Serve 26.11.3 returned for a loaded model, trimmed.
    let listing = json!({"object": "list", "data": [
        {"id": "chat", "owned_by": "mlx-serve", "loaded": true, "state": "ready", "capabilities": ["chat"]},
        {"id": "flux2-klein-9b-4bit", "owned_by": "mlx-serve", "loaded": true, "state": "ready", "capabilities": ["image"]}
    ]});
    assert_eq!(
        served_model(&listing).unwrap().as_deref(),
        Some("flux2-klein-9b-4bit")
    );
    let one = json!({"data": [{"id": "a5339a41", "loaded": true, "state": "ready"}]});
    assert_eq!(served_model(&one).unwrap().as_deref(), Some("a5339a41"));
    let error =
        served_model(&json!({"data": [{"id": "p", "state": "error", "error": "out of memory"}]}))
            .unwrap_err();
    assert!(error.to_string().contains("out of memory"), "{error}");
}

#[test]
#[ignore = "starts the real mlx-serve (DIAL_MLX_SERVE or Dial's install) on DIAL_TEST_MLX_MODEL and waits until it is ready"]
fn starts_mlx_serve_and_reports_its_model() {
    let model = std::env::var("DIAL_TEST_MLX_MODEL").expect("downloaded MLX model folder");
    let interrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = super::ImageServer::start_mlx_serve(Path::new(&model), 120, interrupt).unwrap();
    assert!(server.base_url().starts_with("http://127.0.0.1:"));
    assert!(!server.model().unwrap().is_empty());
    let url = format!("{}/models", server.base_url());
    drop(server);
    // Dropping the server stops it.
    assert!(ureq::get(&url).call().is_err());
}
