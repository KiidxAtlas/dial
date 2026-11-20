use super::*;
use crate::llm::profiles::{ImageGenerationApi, ModelProfile};

/// A configuration on an uncatalogued provider, so only configured selections are listed.
fn config() -> Config {
    let mut config = Config {
        provider: Some("acme".into()),
        model: "main".into(),
        ..Config::default()
    };
    config.model_favorites.push(ModelSelection {
        provider: "acme".into(),
        model: "fav".into(),
        ..Default::default()
    });
    let profile: ModelProfile = serde_json::from_value(json!({
        "provider":"acme","model":"main","reasoning_effort":"high",
        "capabilities":{"reasoning_efforts":["low","high"]}
    }))
    .unwrap();
    config.model_profiles.insert("deep".into(), profile);
    config
}

fn selection(provider: &str, model: &str) -> ModelSelection {
    ModelSelection {
        provider: provider.into(),
        model: model.into(),
        ..Default::default()
    }
}

#[test]
fn list_merges_configured_sources_and_marks_the_current_model() {
    let home = tempfile::tempdir().unwrap();
    let config = config();
    let rows = list(
        &config,
        &selection("acme", "fav"),
        home.path(),
        Some("acme"),
    );
    let keys: Vec<String> = rows
        .iter()
        .map(|row| {
            format!(
                "{}/{}",
                row["provider"].as_str().unwrap(),
                row["model_id"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(keys, ["acme/fav", "acme/main"]);
    assert_eq!(rows[0]["current"], true);
    assert_eq!(rows[0]["sources"], json!(["current", "favorite"]));
    assert_eq!(rows[1]["current"], false);
    assert_eq!(rows[1]["sources"], json!(["main", "profile:deep"]));
    assert_eq!(rows[1]["thinking_levels"], json!(["low", "high"]));
    assert!(rows[1].get("context_window").is_none());

    // A session-level switch to an unconfigured model is still listed as current.
    let switched = list(
        &config,
        &selection("other", "x"),
        home.path(),
        Some("other"),
    );
    assert!(
        switched
            .iter()
            .any(|row| row["model_id"] == "x" && row["current"] == true)
    );
    let filtered = list(&config, &selection("other", "x"), home.path(), Some("acme"));
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|row| row["provider"] == "acme"));
}

/// A saved `/models` listing in `home`, as a refresh writes it.
fn save_listing(home: &Path, rows: &[(&str, catalog::ModelKind, Option<ImageGenerationApi>)]) {
    let entries: Vec<catalog::ModelEntry> = rows
        .iter()
        .map(|(model, kind, image_generation)| catalog::ModelEntry {
            selection: ModelSelection {
                base_url: Some("https://acme.example/v1".into()),
                ..selection("acme", model)
            },
            label: format!("{model} label"),
            source: "listing".into(),
            available: true,
            metadata: catalog::ModelMetadata {
                kind: *kind,
                image_generation: *image_generation,
                ..Default::default()
            },
        })
        .collect();
    let path = crate::config::paths::model_catalog_file(home);
    let bytes = serde_json::to_vec(&BTreeMap::from([("acme", entries)])).unwrap();
    crate::platform::atomic::write_private(&path, &bytes).unwrap();
}

#[test]
fn image_listing_offers_models_whose_metadata_declares_image_output() {
    use catalog::ModelKind::{Chat, Image};
    let home = tempfile::tempdir().unwrap();
    save_listing(
        home.path(),
        &[
            ("painter", Image, None),
            ("both", Chat, Some(ImageGenerationApi::ChatCompletions)),
            ("talker", Chat, None),
            ("refused", Image, Some(ImageGenerationApi::Disabled)),
        ],
    );
    let mut config = config();
    config.image.model = "both".into();
    // Scoped to the fixture's provider: downloaded local models on this machine are listed too.
    let images = list_for(&config, home.path(), Some("acme"), Purpose::Image);
    let ids: Vec<&str> = images
        .iter()
        .map(|row| row["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["both", "painter"],
        "declared output only; Disabled wins"
    );
    assert_eq!(images[0]["current"], true);
    assert_eq!(images[0]["image_api"], "chat-completions");
    assert_eq!(images[0]["base_url"], "https://acme.example/v1");
    assert_eq!(images[1]["current"], false);
    assert!(list_for(&config, home.path(), Some("other"), Purpose::Image).is_empty());

    // The chat listing keeps chat models only and says which of them also make images.
    let chat = list(&config, &config.selection(), home.path(), Some("acme"));
    let row = |id: &str| chat.iter().find(|row| row["model_id"] == id);
    assert!(
        row("painter").is_none(),
        "an image-only model is not a main model"
    );
    assert_eq!(row("both").unwrap()["image_output"], true);
    assert_eq!(row("talker").unwrap()["image_output"], false);
    assert!(
        row("main").unwrap().get("image_output").is_none(),
        "unknown is omitted"
    );
}

#[test]
fn embedding_listing_offers_declared_embedding_models_with_the_id_a_role_saves() {
    use catalog::ModelKind::{Chat, Embedding, Unknown};
    let home = tempfile::tempdir().unwrap();
    save_listing(
        home.path(),
        &[
            ("vectors", Embedding, None),
            ("other-vectors", Embedding, None),
            ("talker", Chat, None),
            ("mystery", Unknown, None),
        ],
    );
    let mut config = config();
    config
        .model_roles
        .insert("embedding".into(), selection("acme", "vectors"));
    let rows = list_for(&config, home.path(), Some("acme"), Purpose::Embedding);
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["other-vectors", "vectors"],
        "declared embedding kind only"
    );
    assert_eq!(rows[1]["current"], true, "the assigned embedding role");
    assert_eq!(rows[0]["current"], false);
    assert_eq!(rows[1]["provider"], "acme");
    assert_eq!(rows[1]["base_url"], "https://acme.example/v1");
    assert!(rows[1].get("image_api").is_none());
}

#[test]
fn current_reports_the_profile_level_and_whether_it_can_be_set() {
    let config = config();
    let current = current(&config);
    assert_eq!(current["provider"], "acme");
    assert_eq!(current["model_id"], "main");
    assert_eq!(current["thinking"]["level"], "high");
    assert_eq!(current["thinking"]["levels"], json!(["low", "high"]));
    assert_eq!(current["thinking"]["settable"], true);

    let mut bare = config.clone();
    bare.model_profiles.clear();
    let current = super::current(&bare);
    assert_eq!(current["thinking"]["level"], Value::Null);
    assert_eq!(current["thinking"]["settable"], false);

    // A bundled model with declared levels needs no hand-written profile.
    let bundled = ["openai", "anthropic", "gemini"]
        .into_iter()
        .find_map(|provider| {
            catalog::bundled_models(provider)
                .into_iter()
                .find(|model| model.thinking.is_some() && model.thinking_efforts.len() > 1)
                .map(|model| (provider, model))
        })
        .expect("a bundled reasoning model");
    let catalogued = Config {
        provider: Some(bundled.0.into()),
        model: bundled.1.id.clone(),
        ..Config::default()
    };
    let current = super::current(&catalogued);
    assert_eq!(current["thinking"]["settable"], true);
    assert_eq!(current["thinking"]["level"], Value::Null);
    assert!(!current["thinking"]["levels"].as_array().unwrap().is_empty());
}

#[test]
fn acp_options_omit_the_list_unless_asked() {
    let home = tempfile::tempdir().unwrap();
    let config = config();
    let full = acp_options(&config, home.path(), true);
    assert_eq!(full["model"], "acme/main");
    // Downloaded local models follow; the configured ones sort first.
    assert_eq!(
        full["models"].as_array().unwrap()[0..2],
        [
            json!({"value":"acme/fav","name":"fav"}),
            json!({"value":"acme/main","name":"main"})
        ]
    );
    assert_eq!(full["thought_level"], "high");
    assert_eq!(full["settable"], true);
    assert!(
        acp_options(&config, home.path(), false)
            .get("models")
            .is_none()
    );
}

fn delete_code(params: Value) -> dial_protocol::ErrorCode {
    delete_local(&params, &[])
        .unwrap_err()
        .downcast_ref::<super::super::RequestError>()
        .expect("coded refusal")
        .code
}

#[test]
fn delete_local_model_refuses_malformed_requests_and_unknown_models_without_deleting() {
    use dial_protocol::ErrorCode;
    assert_eq!(delete_code(json!(null)), ErrorCode::InvalidParams);
    assert_eq!(delete_code(json!({})), ErrorCode::InvalidParams);
    assert_eq!(
        delete_code(json!({"model": "  "})),
        ErrorCode::InvalidParams
    );
    assert_eq!(
        delete_code(json!({"model": "x", "dry_run": "yes"})),
        ErrorCode::InvalidParams
    );
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("model");
    std::fs::create_dir_all(&outside).unwrap();
    assert_eq!(
        delete_code(json!({"model": outside.to_string_lossy(), "dry_run": true})),
        ErrorCode::NotFound
    );
    assert!(outside.is_dir());
}
