use super::*;
use crate::llm::profiles::ReasoningEffort::{High, Low, Xhigh};
use serde_json::json;
use std::path::Path;

/// A saved variant profile whose stored levels (`low`, `high`, `xhigh`) may go stale.
const SAVED: &str = r#"
provider = "google-antigravity"
model = "gemini-3.5-flash"
model_profile = "saved"
[model_profiles.saved]
provider = "google-antigravity"
model = "gemini-3.5-flash"
reasoning_effort = "xhigh"
context_window = 65536
[model_profiles.saved.capabilities]
reasoning = "antigravity-model-variant"
reasoning_efforts = ["low", "high", "xhigh"]
"#;

fn config(home: &Path) -> Config {
    let project = tempfile::tempdir().unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(project.path(), home.to_owned()).unwrap();
    Config::from_table(&layout, toml::from_str(SAVED).unwrap()).unwrap()
}

/// Saves a live listing whose one variant row declares `efforts` (bit set: low=1, medium=2,
/// high=4, xhigh=8), scoped to the built-in Antigravity endpoint.
fn write_listing(home: &Path, efforts: u8) {
    let path = crate::config::paths::model_catalog_file(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let row = json!({
        "selection": {
            "provider": "google-antigravity",
            "model": "gemini-3.5-flash",
            "base_url": "https://daily-cloudcode-pa.googleapis.com",
            "protocol": "antigravity"
        },
        "label": "gemini-3.5-flash",
        "source": "test listing",
        "available": true,
        "metadata": {"declared_reasoning": {"mode": "model-variant", "efforts": efforts}}
    });
    std::fs::write(&path, json!({"any": [row]}).to_string()).unwrap();
}

#[test]
fn saved_variant_profile_takes_its_levels_from_the_current_listing() {
    let home = tempfile::tempdir().unwrap();
    // The account now offers low and high only: the saved xhigh is stale.
    write_listing(home.path(), 0b0101);
    let profile = reasoning_profile(&config(home.path())).unwrap().unwrap();
    assert_eq!(profile.capabilities.reasoning_efforts, vec![Low, High]);
    assert_eq!(profile.reasoning_effort, Some(Xhigh));
    assert_eq!(profile.context_window, Some(65536));
    assert!(
        profile.validate().is_err(),
        "a stale effort must fail request validation"
    );
    // /effort can still choose a level the listing offers.
    let mut chosen = profile;
    chosen.reasoning_effort = Some(High);
    chosen.validate().unwrap();
}

#[test]
fn saved_variant_profile_without_a_current_listing_asks_for_a_refresh() {
    let home = tempfile::tempdir().unwrap();
    let error = reasoning_profile(&config(home.path())).unwrap_err();
    assert!(error.to_string().contains("refresh /models"), "{error}");
}
