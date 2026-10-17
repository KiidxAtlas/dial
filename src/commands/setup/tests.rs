use super::*;

fn empty_env(root: &std::path::Path) -> Env {
    Env {
        path: Vec::new(),
        user_dir: root.join("user"),
    }
}

#[test]
fn choices_only_dispatch_review_and_cover_every_capability() {
    let temp = tempfile::tempdir().unwrap();
    let spec = setup_choices(&empty_env(temp.path()));
    assert_eq!(spec.command, "setup");
    assert_eq!(spec.verb, "choose");
    assert!(spec.remove.is_none());
    assert_eq!(spec.items.len(), deps::RECIPES.len() + 3);
    for id in ["providers", "memory", "embeddings"]
        .into_iter()
        .chain(deps::RECIPES.iter().map(|r| r.id))
    {
        assert_eq!(spec.items.iter().filter(|item| item.value == id).count(), 1);
    }
    assert!(
        spec.items
            .iter()
            .all(|item| !item.value.contains(char::is_whitespace))
    );
    assert!(!temp.path().join("user").exists());
}

#[test]
fn embedding_choices_use_catalog_and_never_install_directly() {
    let spec = embedding_choices();
    assert_eq!(spec.verb, "choose");
    assert!(spec.remove.is_none());
    assert_eq!(spec.items.len(), embeddings::CATALOG.len());
    for (item, model) in spec.items.iter().zip(embeddings::CATALOG) {
        assert_eq!(embeddings::model_from_id(&item.value), Some(model.name));
        assert!(item.detail.contains(&format!("{} MB", model.size_mb)));
        assert!(item.detail.contains(model.description));
    }
}

#[test]
fn review_rejects_unknown_malformed_and_manual_targets_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let env = empty_env(temp.path());
    assert!(
        review_plan("unknown-capability", &env)
            .unwrap_err()
            .contains("Unknown capability")
    );
    for id in [
        "embeddings:",
        "embeddings:-bad",
        "embeddings:bad;command",
        "embeddings-other",
    ] {
        assert!(
            review_plan(id, &env)
                .unwrap_err()
                .contains("Invalid embedding")
        );
    }
    assert_eq!(
        review_plan("hindsight", &env).unwrap_err(),
        run::preview_text("hindsight", &env)
    );
    assert!(review_plan("embeddings:nomic-embed-text", &env).is_err());
    assert!(!env.user_dir.exists());
}

#[test]
fn review_preserves_full_existing_plan_and_skips_installed_targets() {
    let temp = tempfile::tempdir().unwrap();
    let env = empty_env(temp.path());
    // A fake Homebrew entry is enough for pure planning; never execute it.
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("brew"), "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("brew"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let env = Env {
        path: vec![bin],
        ..env
    };
    assert_eq!(
        review_plan("ollama", &env).unwrap(),
        run::preview_text("ollama", &env)
    );
    let artifact = deps::install_dir(&env.user_dir, "bun").join("node_modules/.bin/bun");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(artifact, "").unwrap();
    assert!(
        review_plan("bun", &env)
            .unwrap_err()
            .contains("already set up")
    );
}

fn with_role(config: &mut Config, role: &str) {
    let selection: crate::llm::model_config::ModelSelection =
        toml::from_str("provider = \"ollama\"\nmodel = \"m\"").unwrap();
    config.model_roles.insert(role.into(), selection);
}

#[test]
fn memory_choices_list_every_backend_with_status_and_mark_the_current_one() {
    let config = Config::default();
    let spec = memory_choices(&config);
    assert_eq!(spec.command, "setup");
    assert_eq!(spec.verb, "choose");
    assert_eq!(spec.items.len(), BACKENDS.len());
    for (id, _, _) in BACKENDS {
        assert_eq!(
            spec.items
                .iter()
                .filter(|item| item.value == format!("memory:{}", id.as_str()))
                .count(),
            1
        );
    }
    let current: Vec<_> = spec.items.iter().filter(|item| item.current).collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].value, "memory:provenance");
    let local = spec
        .items
        .iter()
        .find(|item| item.value == "memory:local")
        .unwrap();
    assert!(local.detail.contains("Not ready"), "{}", local.detail);
    assert!(local.detail.contains("/roles (Alt+M)"), "{}", local.detail);
    let hindsight = spec
        .items
        .iter()
        .find(|item| item.value == "memory:hindsight")
        .unwrap();
    assert!(
        hindsight.detail.contains("Service URL"),
        "{}",
        hindsight.detail
    );
}

#[test]
fn backend_missing_separates_blocking_and_optional_steps() {
    let mut config = Config::default();
    let (blocking, optional) = backend_missing(&config, MemoryBackendId::Semantic);
    assert!(blocking.is_empty(), "{blocking:?}");
    assert!(optional.iter().any(|line| line.contains("embedding role")));
    assert!(
        optional
            .iter()
            .any(|line| line.contains("memory role (Alt+M)"))
    );
    with_role(&mut config, "embedding");
    with_role(&mut config, "memory");
    assert_eq!(
        backend_missing(&config, MemoryBackendId::Semantic),
        (Vec::new(), Vec::new())
    );
    assert!(
        backend_missing(&config, MemoryBackendId::Local)
            .0
            .is_empty()
    );
    config.memory.semantic.embedding_model = "half".into();
    assert!(
        backend_missing(&config, MemoryBackendId::Semantic).0[0].contains("set both or clear both")
    );
    let mut sharp = Config::default();
    assert!(
        !backend_missing(&sharp, MemoryBackendId::Sharpshooter)
            .0
            .is_empty()
    );
    sharp.memory.sharpshooter.model = "ollama/qwen3".into();
    assert!(
        backend_missing(&sharp, MemoryBackendId::Sharpshooter)
            .0
            .is_empty()
    );
    let report = switch_report(&config, MemoryBackendId::Local);
    assert!(
        report.contains("This project's settings still select Provenance facts"),
        "{report}"
    );
    let report = switch_report(&Config::default(), MemoryBackendId::Provenance);
    assert!(
        report.contains("Ready: nothing else to set up."),
        "{report}"
    );
}
