use super::*;
use crate::llm::profiles::{ImageGenerationApi, ModelProfile};

/// A configuration on an uncatalogued provider, so only configured selections are listed.
fn config() -> Config {
    let mut config = Config {
        provider: Some("acme".into()),
        model: "main".into(),
        ..Config::default()
    };
    config.providers.insert(
        "acme".into(),
        crate::llm::model_config::ProviderSpec {
            base_url: "https://acme.example/v1".into(),
            ..Default::default()
        },
    );
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
fn chat_picker_excludes_known_non_chat_even_when_configured() {
    let home = tempfile::tempdir().unwrap();
    save_listing(home.path(), &[("fav", catalog::ModelKind::Embedding, None)]);
    let cfg = config();
    let rows = list(&cfg, &cfg.selection(), home.path(), Some("acme"));
    assert!(!rows.iter().any(|row| row["model_id"] == "fav"));
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

#[test]
fn image_listing_includes_cached_chatgpt_candidates_and_reflects_verified_and_refused_states() {
    use crate::llm::catalog::image_capabilities;
    let home = tempfile::tempdir().unwrap();
    let auth_dir = crate::config::paths::subscription_auth_dir(home.path()).unwrap();
    crate::llm::chatgpt::test_account(&auth_dir, false);
    crate::llm::chatgpt::test_catalog(
        &auth_dir,
        &json!({"models": [{"slug": "gpt-6.2-astra", "display_name": "GPT-6.2 Astra"}]}),
    );

    let config = Config {
        user_dir: home.path().to_path_buf(),
        ..Config::default()
    };
    // Cached ChatGPT model appears as an image candidate offline without guessing from names.
    let images = list_for(&config, home.path(), Some("chatgpt"), Purpose::Image);
    let astra = images
        .iter()
        .find(|row| row["model_id"] == "gpt-6.2-astra")
        .expect("cached ChatGPT model listed");
    assert_eq!(astra["image_capability"], "candidate");
    assert_eq!(astra["image_capability_source"], "candidate");
    assert!(astra["image_api"].is_null());

    // Once verified by a live generation, the cached capability upgrades to capable/verified.
    let selection = ModelSelection {
        provider: "chatgpt".into(),
        model: "gpt-6.2-astra".into(),
        base_url: Some(crate::llm::chatgpt::BASE_URL.into()),
        account_id: Some("oaiapp_test".into()),
        ..Default::default()
    };
    image_capabilities::record_verified(home.path(), &selection, ImageGenerationApi::Responses)
        .unwrap();
    let upgraded = list_for(&config, home.path(), Some("chatgpt"), Purpose::Image);
    let astra = upgraded
        .iter()
        .find(|row| row["model_id"] == "gpt-6.2-astra")
        .expect("verified ChatGPT model listed");
    assert_eq!(astra["image_capability"], "capable");
    assert_eq!(astra["image_capability_source"], "verified");
    assert_eq!(astra["image_api"], "responses");
    assert!(astra["image_checked_at"].as_u64().unwrap_or(0) > 0);

    // Persisted account refusal withdraws ChatGPT models from Purpose::Image until cleared.
    image_capabilities::record_account_refused(home.path(), "chatgpt", "oaiapp_test", "free plan")
        .unwrap();
    assert!(list_for(&config, home.path(), Some("chatgpt"), Purpose::Image).is_empty());

    image_capabilities::clear_refusals(home.path()).unwrap();
    assert!(
        list_for(&config, home.path(), Some("chatgpt"), Purpose::Image)
            .iter()
            .any(|row| row["model_id"] == "gpt-6.2-astra")
    );
}

#[test]
fn list_skips_disabled_providers_stale_catalogs_and_account_scoped_bundled_ids() {
    use crate::llm::model_config::ProviderSpec;
    let home = tempfile::tempdir().unwrap();
    let mut config = config();
    // Disabled provider must not flood the chat listing with bundled models.
    config.providers.insert(
        "copilot".into(),
        ProviderSpec {
            enabled: false,
            ..ProviderSpec::default()
        },
    );
    // Account-scoped provider with no live listing must not dump static bundled IDs.
    config.providers.insert(
        "google-antigravity".into(),
        ProviderSpec {
            enabled: true,
            base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
            ..ProviderSpec::default()
        },
    );
    // Enabled provider with a saved live listing must not pile stale bundled models on top.
    config.providers.insert(
        "anthropic".into(),
        ProviderSpec {
            enabled: true,
            base_url: "https://api.anthropic.com/v1".into(),
            ..ProviderSpec::default()
        },
    );
    let live = catalog::ModelEntry {
        selection: selection("anthropic", "claude-live-only"),
        label: "Claude Live Only".into(),
        source: "listing".into(),
        available: true,
        metadata: catalog::ModelMetadata::default(),
    };
    let path = crate::config::paths::model_catalog_file(home.path());
    let bytes = serde_json::to_vec(&BTreeMap::from([("anthropic", vec![live])])).unwrap();
    crate::platform::atomic::write_private(&path, &bytes).unwrap();

    let rows = list(&config, &config.selection(), home.path(), None);
    assert!(
        rows.iter().all(|row| row["provider"] != "copilot"),
        "disabled provider must be omitted"
    );
    assert!(
        rows.iter()
            .all(|row| row["provider"] != "google-antigravity"),
        "account-scoped provider without live listing must not dump bundled IDs"
    );
    let anthropic_ids: Vec<&str> = rows
        .iter()
        .filter(|row| row["provider"] == "anthropic")
        .map(|row| row["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        anthropic_ids,
        ["claude-live-only"],
        "live listing suppresses stale bundled catalog rows"
    );
}

#[test]
fn refresh_lists_available_subscription_models_without_a_config_entry_or_saved_listing() {
    let home = tempfile::tempdir().unwrap();
    let mut config = config();
    config.providers.insert(
        "removed".into(),
        crate::llm::model_config::ProviderSpec {
            enabled: false,
            ..Default::default()
        },
    );
    let entry = |provider, model, kind, available| catalog::ModelEntry {
        selection: selection(provider, model),
        label: format!("{model} label"),
        source: "subscription catalog".into(),
        available,
        metadata: catalog::ModelMetadata {
            kind,
            ..Default::default()
        },
    };
    use catalog::ModelKind::{Chat, Embedding};
    let discovered = vec![
        entry("anthropic", "claude-sonnet-5", Chat, true),
        entry("anthropic", "stale", Chat, false),
        entry("anthropic", "vectors", Embedding, true),
        entry("removed", "disabled", Chat, true),
    ];
    let rows = chat_list(
        &config,
        &config.selection(),
        home.path(),
        None,
        Some(&discovered),
    );
    let offered: Vec<_> = rows
        .iter()
        .filter(|r| r["sources"] == json!(["listing"]))
        .collect();
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0]["provider"], "anthropic");
    assert_eq!(offered[0]["model_id"], "claude-sonnet-5");
    assert_eq!(offered[0]["label"], "claude-sonnet-5 label");
    assert!(rows.iter().any(|row| row["current"] == true));
    assert!(
        select_chat(
            &config,
            &config.selection(),
            home.path(),
            "anthropic",
            "claude-sonnet-5",
            Some(&discovered)
        )
        .is_ok()
    );
}
#[test]
fn set_model_accepts_only_known_models() {
    let mut config = Config::default();
    config.model_favorites.push(ModelSelection {
        provider: "lab".into(),
        model: "pinned-1".into(),
        ..Default::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let pick = |id| select_chat(&config, &config.selection(), dir.path(), "lab", id, None);
    assert_eq!(pick("pinned-1").unwrap(), config.model_favorites[0]);
    assert!(pick("zz-unlisted-9000").is_err());
}

#[test]
fn antigravity_picker_model_is_known_only_to_the_grant_that_listed_it() {
    let dir = tempfile::tempdir().unwrap();
    let provider = "google-antigravity";
    let model = "gemini-4-hydrogen-tiered-high";
    let config = Config::default();
    let grant = crate::llm::credential_store::oauth_path(dir.path(), provider);
    let install =
        |access: &str, refresh: &str, project: &str| {
            crate::config::settings_store::write_private(&grant, &serde_json::to_vec(&json!({
            "access":access, "refresh":refresh, "project_id":project, "expires_at_ms":u64::MAX
        })).unwrap()).unwrap();
        };
    install("opaque-a", "grant-a", "project");
    let rows =
        crate::llm::catalog::test_antigravity_listing(dir.path(), &json!({"models":{(model):{}}}))
            .unwrap();
    assert_eq!(rows[0].selection.model, model);
    let known = || {
        let pick = |scan| {
            select_chat(
                &config,
                &config.selection(),
                dir.path(),
                provider,
                model,
                scan,
            )
            .is_ok()
        };
        let cached = pick(None);
        assert_eq!(
            cached,
            pick(Some(rows.as_slice())),
            "an old scan cannot bypass the account cache"
        );
        cached
    };
    assert!(
        known(),
        "the picker must be able to switch to the ID it just offered"
    );
    install("renewed-a", "grant-a", "project");
    assert!(known());
    install("opaque-b", "grant-b", "project");
    assert!(!known());
    install("renewed-a", "grant-a", "different-project");
    assert!(!known());
}

#[test]
fn scan_replacement_withdraws_models_and_old_work_cannot_repopulate_it() {
    let scan = ModelScan::default();
    let older = scan.begin();
    let newer = scan.begin();
    assert!(scan.publish(newer, vec![]).is_ok());
    assert!(scan.publish(older, vec![]).is_err());
    let in_flight = scan.begin();
    scan.reset();
    assert!(scan.publish(in_flight, vec![]).is_err());
    assert!(scan.snapshot().is_none());
}

#[test]
fn offline_picker_does_not_restore_a_route_from_another_project() {
    let home = tempfile::tempdir().unwrap();
    save_listing(
        home.path(),
        &[("endpoint-only", catalog::ModelKind::Chat, None)],
    );
    let mut cfg = config();
    let pick = |cfg: &Config| {
        select_chat(
            cfg,
            &cfg.selection(),
            home.path(),
            "acme",
            "endpoint-only",
            None,
        )
    };
    assert_eq!(
        pick(&cfg).unwrap().base_url,
        Some(cfg.providers["acme"].base_url.clone())
    );
    cfg.providers.get_mut("acme").unwrap().base_url = "https://other.example/v1".into();
    assert!(pick(&cfg).is_err());
}

fn list_for(
    config: &Config,
    user_dir: &Path,
    provider: Option<&str>,
    purpose: Purpose,
) -> Vec<Value> {
    role_list(config, user_dir, provider, purpose, None)
}
