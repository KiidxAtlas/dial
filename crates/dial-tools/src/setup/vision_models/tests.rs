use super::*;

#[test]
fn bare_ids_name_the_curated_defaults() {
    assert_eq!(
        parse_id("grounding"),
        Some((Kind::Grounding, "mlx-community/UI-Venus-1.5-2B-6bit"))
    );
    assert_eq!(
        parse_id("image-embeddings"),
        Some((
            Kind::ImageEmbedding,
            "mlx-community/Qwen3-VL-Embedding-2B-8bit"
        ))
    );
}

#[test]
fn explicit_ids_round_trip_and_malformed_repos_are_refused() {
    let id = setup_id(Kind::Grounding, "mlx-community/UI-TARS-1.5-7B-4bit");
    assert_eq!(
        parse_id(&id),
        Some((Kind::Grounding, "mlx-community/UI-TARS-1.5-7B-4bit"))
    );
    for bad in [
        "grounding:",
        "grounding:noslash",
        "grounding:a/b/c",
        "grounding:-x/y",
        "grounding:a/b;rm",
        "groundingx",
        "embeddings",
    ] {
        assert_eq!(parse_id(bad), None, "{bad}");
    }
}

#[test]
fn every_curated_model_is_a_valid_repo_and_feature_exists() {
    for kind in Kind::ALL {
        let config: Config = toml::from_str(&format!(
            "[vision.{}]\nprovider = 'mlx'\nmodel = '{}'",
            kind.feature(),
            kind.default_model()
        ))
        .unwrap();
        assert!(config.vision_selection(kind.feature()).is_some());
        for model in kind.catalog() {
            validate_repo(model.repo).unwrap();
            assert!(catalog_text(kind).contains(model.repo));
        }
    }
}

#[test]
fn preview_ends_with_the_confirm_command() {
    let dir = tempfile::tempdir().unwrap();
    let text = preview_text(Kind::Grounding, Kind::Grounding.default_model(), dir.path());
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        assert!(text.ends_with(&format!(
            "Confirm with: /setup grounding:{} yes",
            Kind::Grounding.default_model()
        )));
        assert!(text.contains("vision.grounding"));
    } else {
        assert!(text.contains("OpenAI-compatible endpoint"));
    }
}

#[test]
fn setup_replaces_stale_endpoints_and_preserves_project_selection() {
    for kind in Kind::ALL {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let path = format!("vision.{}", kind.feature());
        let mut old = ModelSelection::new("custom", "remote-model");
        old.base_url = Some("http://127.0.0.1:1234/v1".into());
        old.api_key_env = Some("OLD_KEY".into());
        old.protocol = Some(crate::llm::model_config::ApiProtocol::ChatCompletions);
        settings_store::save_config(
            &layout,
            Scope::Global,
            &[Change::Set {
                path: path.clone(),
                value: toml::Value::try_from(&old).unwrap(),
            }],
        )
        .unwrap();
        let saved = save_selection(&layout, kind, kind.default_model()).unwrap();
        let selected = saved.vision_selection(kind.feature()).unwrap();
        assert_eq!(selected, &ModelSelection::new("mlx", kind.default_model()));
        assert!(saved.for_vision(kind.feature()).endpoint.is_none());
        assert!(!saved.model_roles.contains_key(kind.feature()));
        settings_store::save_config(
            &layout,
            Scope::Project,
            &[Change::Set {
                path,
                value: toml::Value::try_from(&old).unwrap(),
            }],
        )
        .unwrap();
        let saved = save_selection(&layout, kind, kind.default_model()).unwrap();
        assert_eq!(saved.vision_selection(kind.feature()), Some(&old));
        assert!(
            summary(kind, kind.default_model(), &saved).contains("overrides the new global one")
        );
    }
}
