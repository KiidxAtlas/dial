use super::*;

fn table(text: &str) -> toml::Table {
    text.parse().unwrap()
}

#[test]
fn apply_restores_pinned_keys_and_removes_ones_absent_at_capture() {
    let pin = SessionModels::capture(&table(
        "model = 'a'\nprovider = 'mlx'\ntheme = 'old'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n",
    ));
    let mut merged = table(
        "model = 'b'\nprovider = 'openai'\nmodel_profile = 'fast'\ntheme = 'new'\n[endpoint]\nbase_url = 'https://api.openai.com/v1'\n[model_roles.task]\nprovider = 'openai'\nmodel = 't'\n",
    );
    pin.apply(&mut merged);
    assert_eq!(
        merged,
        table(
            "model = 'a'\nprovider = 'mlx'\ntheme = 'new'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n"
        )
    );
}

#[test]
fn adopting_own_writes_takes_only_the_written_keys() {
    let mut pin = SessionModels::capture(&table(
        "model = 'a'\ncontext_window = 'auto'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n",
    ));
    let disk = table(
        "model = 'other-tab'\ncontext_window = 64000\n[model_roles.small]\nprovider = 'mlx'\nmodel = 'other-small'\n[model_roles.task]\nprovider = 'openai'\nmodel = 'mine'\n",
    );
    pin.adopt_paths(&disk, ["context_window", "model_roles.task.model", "theme"]);
    let mut merged = disk.clone();
    pin.apply(&mut merged);
    assert_eq!(
        merged,
        table(
            "model = 'a'\ncontext_window = 64000\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n[model_roles.task]\nprovider = 'openai'\nmodel = 'mine'\n"
        )
    );
}

#[test]
fn adopting_a_role_takes_its_complete_selection() {
    let mut pin = SessionModels::capture(&table(
        "model = 'a'\nmodel_profile = 'old'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n",
    ));
    let disk = table(
        "model = 'b'\nprovider = 'openai'\n[endpoint]\nbase_url = 'https://api.openai.com/v1'\n",
    );
    pin.adopt_role(&disk, "main");
    pin.adopt_role(&disk, "small");
    let mut merged = table("model = 'c'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 'x'\n");
    pin.apply(&mut merged);
    assert_eq!(merged, disk);
}

#[test]
fn moved_feature_models_stay_session_local_and_adopt_only_own_writes() {
    let initial = table(
        "[vision.grounding]\nprovider='mlx'\nmodel='own-grounding'\n\
         [vision.image_embedding]\nprovider='mlx'\nmodel='own-vectors'\n\
         [laya]\ncheckpoint='/own'\nenabled=false\n",
    );
    let disk = table(
        "[vision.grounding]\nprovider='mlx'\nmodel='other-grounding'\n\
         [vision.image_embedding]\nprovider='mlx'\nmodel='other-vectors'\n\
         [laya]\ncheckpoint='/other'\nenabled=true\n",
    );
    let mut pin = SessionModels::capture(&initial);
    let mut merged = disk.clone();
    pin.apply(&mut merged);
    assert_eq!(merged["vision"], initial["vision"]);
    assert_eq!(merged["laya"]["checkpoint"].as_str(), Some("/own"));
    assert_eq!(merged["laya"]["enabled"].as_bool(), Some(true));
    pin.adopt_paths(&disk, ["vision.grounding", "laya.checkpoint"]);
    pin.apply(&mut merged);
    assert_eq!(merged["vision"]["grounding"], disk["vision"]["grounding"]);
    assert_eq!(
        merged["vision"]["image_embedding"],
        initial["vision"]["image_embedding"]
    );
    assert_eq!(merged["laya"]["checkpoint"].as_str(), Some("/other"));
    let empty = table("[laya]\nenabled=false\n");
    pin.adopt_paths(&empty, ["vision", "laya.checkpoint"]);
    pin.apply(&mut merged);
    assert!(!merged.contains_key("vision"));
    assert!(merged["laya"].get("checkpoint").is_none());
    assert_eq!(merged["laya"]["enabled"].as_bool(), Some(true));
}
