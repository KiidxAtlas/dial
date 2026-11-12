use super::*;
use crate::llm::profiles::ModelProfile;

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
