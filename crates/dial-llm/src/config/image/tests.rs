use super::*;

fn settings(model: &str) -> ImageModelSettings {
    ImageModelSettings {
        model: model.into(),
        ..Default::default()
    }
}

#[test]
fn settings_are_found_by_exact_model_and_round_trip_through_toml() {
    let mut config = ImageConfig::default();
    assert!(!config.allow_weight_edits);
    config.allow_weight_edits = true;
    let mut flux = settings("mlx-community/flux2-klein-9b-4bit");
    flux.steps = Some(8);
    flux.seed = Some(4_241_330_180);
    flux.size = Some("1536x1024".into());
    flux.loras = vec![ImageLora {
        path: "style.safetensors".into(),
        scale: 1.25,
    }];
    config.models.insert("flux".into(), flux.clone());
    assert_eq!(
        config.settings_for("mlx-community/flux2-klein-9b-4bit"),
        Some(&flux)
    );
    assert!(config.settings_for("flux2-klein-9b-4bit").is_none());
    config.validate().unwrap();

    let text = toml::to_string(&config).unwrap();
    let back: ImageConfig = toml::from_str(&text).unwrap();
    assert_eq!(back, config);
    // Unset values are omitted rather than written as placeholders.
    assert!(!text.contains("quality"), "{text}");
    // A LoRA without a scale uses full strength.
    let lora: ImageLora = toml::from_str("path = \"a.safetensors\"").unwrap();
    assert_eq!(lora.scale, 1.0);
}

#[test]
fn invalid_values_and_duplicate_models_are_refused() {
    let base = settings("m");
    let cases: Vec<(ImageModelSettings, &str)> = vec![
        (settings(" "), "identifier"),
        (
            ImageModelSettings {
                size: Some("512x512".into()),
                ..base.clone()
            },
            "size",
        ),
        (
            ImageModelSettings {
                quality: Some("ultra".into()),
                ..base.clone()
            },
            "quality",
        ),
        (
            ImageModelSettings {
                background: Some("clear".into()),
                ..base.clone()
            },
            "background",
        ),
        (
            ImageModelSettings {
                steps: Some(0),
                ..base.clone()
            },
            "steps",
        ),
        (
            ImageModelSettings {
                seed: Some(u64::MAX),
                ..base.clone()
            },
            "seed",
        ),
        (
            ImageModelSettings {
                loras: vec![ImageLora {
                    path: "a.bin".into(),
                    scale: 1.0,
                }],
                ..base.clone()
            },
            "safetensors",
        ),
        (
            ImageModelSettings {
                loras: vec![ImageLora {
                    path: "a.safetensors".into(),
                    scale: f64::NAN,
                }],
                ..base.clone()
            },
            "scale",
        ),
    ];
    for (settings, needle) in cases {
        let error = settings.validate().unwrap_err().to_string();
        assert!(error.contains(needle), "{needle}: {error}");
    }

    let mut config = ImageConfig::default();
    config.models.insert("a".into(), settings("same"));
    config.models.insert("b".into(), settings("same"));
    assert!(config.validate().is_err());
}

#[test]
fn lora_names_resolve_inside_the_mlx_serve_directory() {
    assert_eq!(
        lora_path("/abs/style.safetensors"),
        PathBuf::from("/abs/style.safetensors")
    );
    if let Some(dir) = mlx_serve_lora_dir() {
        assert_eq!(
            lora_path("style.safetensors"),
            dir.join("style.safetensors")
        );
    }
    assert!(settings("m").is_empty());
    let mut steps = settings("m");
    steps.steps = Some(4);
    assert!(!steps.is_empty());
}

#[test]
fn panel_entry_reuses_the_existing_key_or_derives_a_stable_bare_one() {
    let mut config = ImageConfig::default();
    let (key, fresh) = config.entry_for("org/model");
    assert_eq!(fresh, settings("org/model"));
    assert_eq!(key, config.entry_for("org/model").0);
    assert!(key.starts_with("image_") && key.len() == 22, "{key}");
    assert!(key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    assert_ne!(key, config.entry_for("org/other").0);

    let mut mine = settings("org/model");
    mine.steps = Some(4);
    config.models.insert("mine".into(), mine.clone());
    assert_eq!(config.entry_for("org/model"), ("mine".into(), mine));
}

#[test]
fn native_protocols_and_provider_inputs_round_trip_without_changing_legacy_defaults() {
    assert_eq!(ImageConfig::default().api, None);
    for api in [
        "images",
        "mlx-serve",
        "gemini",
        "imagen",
        "black-forest-labs",
        "comfyui",
        "fal",
        "replicate",
        "disabled",
    ] {
        let text = format!(
            "api = {api:?}\n[models.native]\nmodel = 'owner/model'\ninput = {{ guidance_scale = 3.5, nested = {{ enabled = true }} }}\n"
        );
        let config: ImageConfig = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        assert_eq!(api_name(config.api), api);
        let saved = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<ImageConfig>(&saved).unwrap(), config);
    }
    assert_eq!(toml::from_str::<ImageConfig>("api = ''").unwrap().api, None);
    assert!(toml::from_str::<ImageConfig>("api = 'imaginary'").is_err());
    assert!(toml::from_str::<ImageConfig>("[models.a]\nmodel='m'\nunknown=true").is_err());
}

#[test]
fn comfy_workflow_validates_both_nodes_and_round_trips() {
    let graph = serde_json::json!({
        "6": {"class_type":"CLIPTextEncode", "inputs":{"text":"old prompt"}},
        "9": {"class_type":"SaveImage", "inputs":{"images":["8",0]}}
    });
    let workflow = ComfyWorkflow {
        workflow: graph,
        prompt_node: "6".into(),
        output_node: "9".into(),
    };
    workflow.validate().unwrap();
    let mut model = settings("my-workflow");
    model.comfyui = Some(workflow.clone());
    let encoded = toml::to_string(&model).unwrap();
    assert_eq!(
        toml::from_str::<ImageModelSettings>(&encoded).unwrap(),
        model
    );
    for invalid in [
        ComfyWorkflow {
            prompt_node: "missing".into(),
            ..workflow.clone()
        },
        ComfyWorkflow {
            output_node: "missing".into(),
            ..workflow.clone()
        },
        ComfyWorkflow {
            workflow: serde_json::json!([]),
            ..workflow.clone()
        },
        ComfyWorkflow {
            workflow: serde_json::json!({"6":{"inputs":{"text":42}}, "9":{}}),
            ..workflow
        },
    ] {
        assert!(invalid.validate().is_err(), "accepted {invalid:?}");
    }
}
