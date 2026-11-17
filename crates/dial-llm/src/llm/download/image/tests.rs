use super::*;
use serde_json::json;

fn listing(files: &[(&str, u64)]) -> Vec<(String, u64)> {
    files
        .iter()
        .map(|(path, size)| ((*path).to_owned(), *size))
        .collect()
}

#[test]
fn search_lists_diffusers_text_to_image_repositories() {
    let params = image_search_query("  flux  ");
    assert!(params.contains(&("filter", "diffusers")));
    assert!(params.contains(&("pipeline_tag", "text-to-image")));
    assert!(params.contains(&("search", "flux")));
    assert!(
        !image_search_query("")
            .iter()
            .any(|(key, _)| *key == "search")
    );
}

#[test]
fn flux_klein_layout_fetches_the_root_original_instead_of_the_diffusers_transformer() {
    // The file list of black-forest-labs/FLUX.2-klein-4B.
    let repo = listing(&[
        (".gitattributes", 1580),
        ("README.md", 9991),
        ("editing.jpg", 2_506_178),
        ("flux-2-klein-4b.safetensors", 7_751_105_712),
        ("model_index.json", 446),
        ("scheduler/scheduler_config.json", 486),
        ("text_encoder/config.json", 1536),
        ("text_encoder/generation_config.json", 214),
        (
            "text_encoder/model-00001-of-00002.safetensors",
            4_967_215_360,
        ),
        (
            "text_encoder/model-00002-of-00002.safetensors",
            3_077_766_632,
        ),
        ("text_encoder/model.safetensors.index.json", 32855),
        ("tokenizer/chat_template.jinja", 4168),
        ("tokenizer/merges.txt", 1_671_853),
        ("tokenizer/tokenizer.json", 11_422_654),
        ("transformer/config.json", 541),
        (
            "transformer/diffusion_pytorch_model.safetensors",
            7_751_109_744,
        ),
        ("vae/config.json", 821),
        ("vae/diffusion_pytorch_model.safetensors", 168_120_878),
    ]);
    let (files, bytes) = files(&repo).unwrap();
    assert!(files.contains(&"model_index.json".to_owned()));
    assert!(files.contains(&"text_encoder/model.safetensors.index.json".to_owned()));
    assert!(files.contains(&"text_encoder/model-00002-of-00002.safetensors".to_owned()));
    // stable-diffusion.cpp misreads the Diffusers FLUX.2 transformer; the original loads.
    assert!(files.contains(&"flux-2-klein-4b.safetensors".to_owned()));
    assert!(files.contains(&"transformer/config.json".to_owned()));
    assert!(!files.contains(&"transformer/diffusion_pytorch_model.safetensors".to_owned()));
    assert!(files.contains(&"vae/diffusion_pytorch_model.safetensors".to_owned()));
    assert!(!files.iter().any(|file| file.ends_with(".jpg")));
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(files, sorted);
    let weights = 4_967_215_360 + 3_077_766_632 + 7_751_105_712 + 168_120_878;
    assert!(bytes > weights && bytes < weights + 20_000_000, "{bytes}");
}

#[test]
fn several_root_files_or_a_full_checkpoint_keep_the_diffusers_transformer() {
    // FLUX.1-dev keeps its transformer and its autoencoder at the root.
    let flux1 = listing(&[
        ("model_index.json", 1),
        ("ae.safetensors", 335),
        ("flux1-dev.safetensors", 23_802),
        ("transformer/diffusion_pytorch_model.safetensors", 23_802),
        ("vae/diffusion_pytorch_model.safetensors", 168),
    ]);
    let (plan, _) = files(&flux1).unwrap();
    assert!(plan.contains(&"transformer/diffusion_pytorch_model.safetensors".to_owned()));
    assert!(
        !plan
            .iter()
            .any(|file| !file.contains('/') && file != "model_index.json")
    );
    // A single-file checkpoint that also holds the VAE is larger than the transformer.
    let full = listing(&[
        ("model_index.json", 1),
        ("sd3.5_large.safetensors", 16_500),
        ("transformer/diffusion_pytorch_model.safetensors", 16_300),
        ("vae/diffusion_pytorch_model.safetensors", 168),
    ]);
    let (plan, _) = files(&full).unwrap();
    assert!(plan.contains(&"transformer/diffusion_pytorch_model.safetensors".to_owned()));
    assert!(!plan.contains(&"sd3.5_large.safetensors".to_owned()));
}

#[test]
fn half_precision_variants_are_preferred_then_default_weights() {
    let repo = listing(&[
        ("model_index.json", 1),
        ("unet/diffusion_pytorch_model.safetensors", 10),
        ("unet/diffusion_pytorch_model.fp16.safetensors", 5),
        ("vae/diffusion_pytorch_model.safetensors", 6),
        ("vae/diffusion_pytorch_model.bf16.safetensors", 3),
        ("text_encoder/model.fp16-00001-of-00002.safetensors", 2),
        ("text_encoder/model.fp16-00002-of-00002.safetensors", 2),
        ("text_encoder/model.safetensors.index.fp16.json", 1),
        ("text_encoder/model.safetensors.index.json", 1),
        ("text_encoder/pytorch_model.bin", 99),
    ]);
    let (files, bytes) = files(&repo).unwrap();
    assert_eq!(
        files,
        [
            "model_index.json",
            "text_encoder/model.fp16-00001-of-00002.safetensors",
            "text_encoder/model.fp16-00002-of-00002.safetensors",
            "text_encoder/model.safetensors.index.fp16.json",
            "unet/diffusion_pytorch_model.fp16.safetensors",
            "vae/diffusion_pytorch_model.safetensors",
        ]
    );
    assert_eq!(bytes, 1 + 2 + 2 + 1 + 5 + 6);
}

#[test]
fn non_diffusers_repositories_have_no_image_plan() {
    assert!(files(&listing(&[("model.safetensors", 1), ("config.json", 1)])).is_none());
    // A Diffusers layout with only PyTorch .bin weights has nothing sd-server is given.
    assert!(
        files(&listing(&[
            ("model_index.json", 1),
            ("unet/diffusion_pytorch_model.bin", 1)
        ]))
        .is_none()
    );
}

#[test]
fn snapshot_root_strips_the_repository_relative_file() {
    let root = snapshot_root(
        "/cache/models--a--b/snapshots/abc/vae/config.json",
        "vae/config.json",
    )
    .unwrap();
    assert_eq!(root, "/cache/models--a--b/snapshots/abc");
    assert!(snapshot_root("relative/vae/config.json", "vae/config.json").is_err());
}

#[test]
fn installed_paths_name_their_repository() {
    let path = Path::new(
        "/Users/me/.cache/huggingface/hub/models--Runpod--FLUX.2-klein-4B-mflux-4bit/snapshots/abc",
    );
    assert_eq!(
        repo_of(path).as_deref(),
        Some("Runpod/FLUX.2-klein-4B-mflux-4bit")
    );
    assert_eq!(repo_of(Path::new("/tmp/model")), None);
}

#[test]
fn base_model_is_read_as_a_string_or_a_list() {
    let one = json!({"cardData": {"base_model": "black-forest-labs/FLUX.2-klein-4B"}});
    assert_eq!(base_models(&one), ["black-forest-labs/FLUX.2-klein-4B"]);
    let many = json!({"cardData": {"base_model": ["org/a", "not an id", "org/b"]}});
    assert_eq!(base_models(&many), ["org/a", "org/b"]);
    assert!(base_models(&json!({})).is_empty());
}

/// Live: needs network access to huggingface.co.
#[test]
#[ignore = "network"]
fn mflux_pack_resolves_to_its_diffusers_original() {
    let original = runnable_original("Runpod/FLUX.2-klein-4B-mflux-4bit").unwrap();
    assert_eq!(original.repo, "black-forest-labs/FLUX.2-klein-4B");
    assert!(
        original
            .files
            .contains(&"flux-2-klein-4b.safetensors".to_owned())
    );
    assert!(original.bytes > 15_000_000_000, "{}", original.bytes);
}

/// Live: downloads a 9 MB Diffusers pipeline with the Python in `DIAL_TEST_PYTHON`.
#[test]
#[ignore = "network"]
fn downloads_only_the_files_sd_server_loads_and_returns_the_snapshot() {
    let python = std::env::var("DIAL_TEST_PYTHON").expect("DIAL_TEST_PYTHON");
    let repo = "hf-internal-testing/tiny-sd-pipe";
    let (files, _) = files(&super::super::tree(repo).unwrap()).unwrap();
    let root = download(
        Path::new(&python),
        repo,
        &files,
        &AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
    let root = Path::new(&root);
    assert!(root.join("model_index.json").is_file());
    assert!(!root.join("unet/diffusion_pytorch_model.bin").exists());
    let parts = crate::llm::server::image::components(root).unwrap();
    assert!(!parts.is_empty());
}
