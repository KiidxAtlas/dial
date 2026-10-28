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
fn every_curated_model_is_a_valid_repo_and_roles_exist() {
    for kind in Kind::ALL {
        assert!(
            crate::llm::model_config::ROLES
                .iter()
                .any(|(name, _)| *name == kind.role()),
            "{}",
            kind.role()
        );
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
        assert!(text.contains("model_roles.grounding"));
    } else {
        assert!(text.contains("OpenAI-compatible endpoint"));
    }
}
