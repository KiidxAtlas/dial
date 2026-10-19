use super::*;
use crate::ui::overlay::settings::testkit::{Rig, ch, key, render, screen, status_preview};
use crossterm::event::KeyCode;

fn mlx() -> ModelProfile {
    ModelProfile {
        provider: "mlx".into(),
        model: "local-model".into(),
        capabilities: crate::llm::profiles::ModelCapabilities {
            temperature: true,
            top_p: true,
            seed: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn go(overlay: &mut ModelSettingsOverlay, rig: &Rig, path: &str) {
    overlay.cursor = overlay.fields.iter().position(|f| f.path == path).unwrap();
    let _ = rig;
}

#[test]
fn editor_round_trips_local_values_and_rejects_invalid_ones_with_plain_messages() {
    let overlay = ModelSettingsOverlay::new("local".into(), mlx());
    assert_eq!(
        overlay
            .candidate("local_sampling.top_k", "40")
            .unwrap()
            .local_sampling
            .unwrap()
            .top_k,
        Some(40)
    );
    assert!(overlay.candidate("local_sampling.min_p", "1.1").is_err());
    // 0 means "no operator output cap"; values above the documented maximum are refused.
    assert!(overlay.candidate("max_tokens", "0").is_ok());
    assert!(overlay.candidate("max_tokens", "1048577").is_err());
    assert!(
        overlay
            .candidate("tools", "[]\nmodel = 'different'")
            .is_err()
    );
    let message = overlay
        .candidate("temperature", "warm")
        .unwrap_err()
        .to_string();
    assert!(message.contains("quotes"), "{message}");
}

#[test]
fn context_defaults_shortcut_navigates_without_modifying_the_profile() {
    let rig = Rig::new();
    let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
    assert!(matches!(overlay.event(&ch('d'), &rig.keys),
        OverlayEvent::Action(action) if action == "open:context-defaults"));
    assert!(overlay.profile.context_window.is_none());
    assert!(overlay.pending.is_none());
}

#[test]
fn declared_capabilities_read_as_plain_words_and_keep_the_raw_toml() {
    let overlay = ModelSettingsOverlay::new("local".into(), mlx());
    let table = overlay.table();
    let caps = table.get("capabilities").expect("capabilities table");
    let summary = ModelSettingsOverlay::capability_summary(caps);
    assert!(summary.contains("Temperature: yes"), "{summary}");
    assert!(summary.contains("Seed: yes"), "{summary}");
    assert!(summary.contains("Image input: not declared"), "{summary}");
    assert!(!summary.contains(" = "), "raw TOML leaked: {summary}");
}

#[test]
fn arrows_start_at_a_sensible_value_then_step_and_clamp() {
    let rig = Rig::new();
    let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
    go(&mut overlay, &rig, "temperature");
    for (event, expected) in [
        (KeyCode::Right, 0.2),
        (KeyCode::Right, 0.25),
        (KeyCode::Left, 0.2),
    ] {
        let OverlayEvent::Set { value, .. } = overlay.event(&key(event), &rig.keys) else {
            panic!("expected a save")
        };
        assert_eq!(value["temperature"].as_float(), Some(expected));
        overlay.settled(Ok(()));
    }
    assert!(overlay.is_override(overlay.fields[overlay.cursor]));
    assert!(
        matches!(overlay.event(&ch('r'), &rig.keys), OverlayEvent::Set { value, .. } if value.get("temperature").is_none())
    );
    overlay.settled(Ok(()));
    assert!(!overlay.is_override(overlay.fields[overlay.cursor]));
}

#[test]
fn context_size_saves_validates_and_resets_for_local_and_cloud_models() {
    let rig = Rig::new();
    for provider in ["mlx", "openai", "lm-studio"] {
        let mut profile = mlx();
        profile.provider = provider.into();
        let mut overlay = ModelSettingsOverlay::new("model".into(), profile);
        go(&mut overlay, &rig, "context_window");
        assert!(matches!(
            overlay.apply("65536"),
            OverlayEvent::Set { value, .. } if value["context_window"].as_integer() == Some(65536)
        ));
        overlay.settled(Ok(()));
        assert_eq!(overlay.profile.context_window, Some(65536));
        let encoded = toml::to_string(&overlay.profile).unwrap();
        let decoded: ModelProfile = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.context_window, Some(65536));
        for invalid in ["0", "2047", "2097153", "32768.5", "-1"] {
            assert!(overlay.candidate("context_window", invalid).is_err());
        }
        assert!(matches!(
            overlay.event(&ch('r'), &rig.keys),
            OverlayEvent::Set { value, .. } if value.get("context_window").is_none()
        ));
        overlay.settled(Ok(()));
        assert_eq!(overlay.profile.context_window, None);
        assert!(matches!(
            overlay.event(&key(KeyCode::Right), &rig.keys),
            OverlayEvent::Set { value, .. } if value["context_window"].as_integer() == Some(32768)
        ));
    }
}

#[test]
fn choices_toggles_and_typed_numbers_save_valid_profiles() {
    let rig = Rig::new();
    let mut profile = mlx();
    profile.provider = "mlx".into();
    let mut overlay = ModelSettingsOverlay::new("local".into(), profile);
    go(&mut overlay, &rig, "local_sampling.top_k");
    overlay.event(&key(KeyCode::Enter), &rig.keys);
    assert!(overlay.editor.is_some());
    for c in "25".chars() {
        overlay.event(&ch(c), &rig.keys);
    }
    assert!(
        matches!(overlay.event(&key(KeyCode::Enter), &rig.keys), OverlayEvent::Set { value, .. } if value["local_sampling"]["top_k"].as_integer() == Some(25))
    );
    overlay.settled(Ok(()));
    assert!(overlay.editor.is_none());
    go(&mut overlay, &rig, "local_sampling.min_p");
    overlay.event(&key(KeyCode::Enter), &rig.keys);
    for c in "7".chars() {
        overlay.event(&ch(c), &rig.keys);
    }
    assert!(matches!(
        overlay.event(&key(KeyCode::Enter), &rig.keys),
        OverlayEvent::None
    ));
    assert!(
        overlay.editor.is_some(),
        "an invalid value keeps the editor open"
    );
    assert!(overlay.message.as_ref().is_some_and(|(_, error)| *error));
}

#[test]
fn page_shows_sections_values_and_help_and_survives_tiny_terminals() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut profile = mlx();
    profile.temperature = Some(0.3);
    let mut overlay = ModelSettingsOverlay::new("local".into(), profile);
    let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
    assert!(
        text.contains("Context size") && text.contains("2048 to 2097152"),
        "{text}"
    );
    go(&mut overlay, &rig, "temperature");
    let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
    for needle in [
        " Model settings",
        "SAMPLING",
        "Temperature",
        "0.3",
        "default",
        "custom value",
        "Randomness",
        "reset",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
    go(&mut overlay, &rig, "local_sampling.chat_template_kwargs");
    let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
    assert!(
        text.contains("Template arguments") && text.to_lowercase().contains("chat template"),
        "{text}"
    );
    if std::env::var_os("DIAL_RENDER_MODEL_SETTINGS").is_some() {
        std::fs::create_dir_all("target/ui-previews").unwrap();
        std::fs::write("target/ui-previews/model-settings.txt", &text).unwrap();
    }
    for (width, height) in [(1, 1), (10, 3), (24, 8), (50, 15), (80, 24), (160, 50)] {
        let buffer = render(width, height, |frame| overlay.draw(frame, &ctx));
        assert_eq!(buffer.area.width, width);
    }
}

#[test]
fn detail_box_keeps_the_range_line_when_help_wraps() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
    go(&mut overlay, &rig, "context_window");
    for (width, height) in [(100, 32), (60, 32)] {
        let text = screen(&render(width, height, |frame| overlay.draw(frame, &ctx)));
        assert!(text.contains("Accepts 2048 to"), "{width}x{height}: {text}");
    }
    let ascii = crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let ascii_ctx = DrawCtx {
        theme: &ascii,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ascii_ctx)));
    assert!(text.contains("<-/->") && !text.contains('←'), "{text}");
}

#[test]
fn unsupported_fields_explain_how_to_enable_them_and_cloud_pages_hide_local_ones() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut cloud = ModelSettingsOverlay::new(
        "cloud".into(),
        ModelProfile {
            provider: "anthropic".into(),
            model: "cloud-model".into(),
            ..Default::default()
        },
    );
    assert!(
        !cloud
            .fields
            .iter()
            .any(|f| f.path.starts_with("local_sampling"))
    );
    assert!(cloud.fields.iter().any(|f| f.path == "prompt_cache"));
    go(&mut cloud, &rig, "temperature");
    let text = screen(&render(100, 32, |frame| cloud.draw(frame, &ctx)));
    assert!(text.contains("Not declared as supported"), "{text}");
    let local = ModelSettingsOverlay::new("local".into(), mlx());
    assert!(
        !local
            .fields
            .iter()
            .any(|f| f.path == "prompt_cache" || f.path == "thinking")
    );
}
#[test]
fn lmstudio_settings_expose_supported_local_controls() {
    let rig = Rig::new();
    let mut profile = mlx();
    profile.provider = "lm-studio".into();
    let mut overlay = ModelSettingsOverlay::new("LM Studio".into(), profile);
    for path in [
        "local_sampling.top_k",
        "local_sampling.min_p",
        "local_sampling.seed",
        "local_sampling.repetition_penalty",
        "local_sampling.presence_penalty",
        "local_sampling.frequency_penalty",
    ] {
        assert!(
            overlay.fields.iter().any(|f| f.path == path),
            "missing {path}"
        );
    }
    for path in [
        "local_sampling.xtc_probability",
        "local_sampling.repetition_context_size",
        "local_sampling.chat_template_kwargs",
        "prompt_cache",
    ] {
        assert!(
            !overlay.fields.iter().any(|f| f.path == path),
            "unsupported {path}"
        );
    }
    go(&mut overlay, &rig, "local_sampling.top_k");
    let candidate = overlay.candidate("local_sampling.top_k", "40").unwrap();
    assert_eq!(candidate.local_sampling.unwrap().top_k, Some(40));
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
    assert!(text.contains("Top-k"), "{text}");
}

#[test]
fn a_failed_save_keeps_the_editor_and_shows_the_reason() {
    let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
    let mut editor = Editor::default();
    editor.set("1000");
    overlay.editor = Some(editor);
    overlay.pending = Some((overlay.profile.clone(), "saved".into()));
    overlay.settled(Err("cannot write settings\nmore detail".into()));
    assert!(overlay.editor.is_some() && overlay.pending.is_none());
    assert_eq!(
        overlay.message,
        Some(("cannot write settings".into(), true))
    );
}

#[test]
fn suggestions_are_listed_and_enter_applies_only_the_selected_one() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
    overlay.suggestions = vec![
        Suggestion {
            path: "edit_mode".into(),
            global: false,
            value: serde_json::json!("apply_patch"),
            title: "Use the apply_patch edit grammar".into(),
            evidence: "edits failed 9 of 20 times under replace, 1 of 15 under apply_patch".into(),
        },
        Suggestion {
            path: "assistance.local.side_models".into(),
            global: true,
            value: serde_json::json!("reuse_main"),
            title: "Reuse the main model for side requests".into(),
            evidence: "side requests swapped the loaded model 4 times".into(),
        },
    ];
    overlay.cursor = overlay.fields.len();
    let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
    assert!(
        // Long titles are cut to the label column; the detail box shows the whole title.
        text.contains("SUGGESTIONS") && text.contains("Use the apply_patch"),
        "{text}"
    );
    assert!(text.contains("edits failed 9 of 20"), "{text}");

    // Only the selected row is saved, and only once Enter is pressed.
    overlay.cursor = overlay.fields.len();
    assert!(matches!(
        overlay.event(&key(KeyCode::Right), &rig.keys),
        OverlayEvent::None
    ));
    let OverlayEvent::Set { path, value, .. } = overlay.event(&key(KeyCode::Enter), &rig.keys)
    else {
        panic!("expected a profile save")
    };
    assert_eq!(path, "model_profiles.local");
    assert_eq!(value["edit_mode"].as_str(), Some("apply_patch"));
    assert_eq!(overlay.suggestions.len(), 2, "still listed until saved");
    overlay.settled(Ok(()));
    assert_eq!(overlay.suggestions.len(), 1);
    assert_eq!(
        overlay.profile.edit_mode,
        Some(crate::config::types::EditMode::ApplyPatch)
    );

    // A global key goes through the config path, and a failed save keeps the row.
    overlay.cursor = overlay.fields.len();
    let OverlayEvent::Set { path, value, .. } = overlay.event(&key(KeyCode::Enter), &rig.keys)
    else {
        panic!("expected a config save")
    };
    assert_eq!(path, "assistance.local.side_models");
    assert_eq!(value.as_str(), Some("reuse_main"));
    overlay.settled(Err("cannot write settings".into()));
    assert_eq!(overlay.suggestions.len(), 1);
    assert!(matches!(
        overlay.event(&key(KeyCode::Enter), &rig.keys),
        OverlayEvent::Set { .. }
    ));
    overlay.settled(Ok(()));
    assert!(overlay.suggestions.is_empty());
    assert_eq!(overlay.cursor, overlay.fields.len() - 1);
}

#[test]
fn evidence_file_offers_suggestions_for_local_models_only() {
    let dir = tempfile::tempdir().unwrap();
    let mlx_key = local_evidence::model_key("mlx", "local-model");
    let hosted_key = local_evidence::model_key("anthropic", "local-model");
    let file = serde_json::json!({
        mlx_key: {
            "stage": 2,
            "edits": {
                "replace": {"ok": 1, "failed": 9},
                "apply_patch": {"ok": 14, "failed": 1}
            }
        },
        hosted_key: {
            "stage": 2,
            "edits": {"replace": {"ok": 1, "failed": 9}}
        }
    });
    std::fs::write(dir.path().join(local_evidence::FILE), file.to_string()).unwrap();
    let base = Current {
        edit_mode: "replace".into(),
        side_models: "warn".into(),
        context_window: 32768,
        ..Current::default()
    };
    let overlay =
        ModelSettingsOverlay::new("local".into(), mlx()).with_suggestions(dir.path(), base.clone());
    assert!(
        overlay
            .suggestions
            .iter()
            .any(|s| s.path == "edit_mode" && s.value == serde_json::json!("apply_patch")),
        "{:?}",
        overlay.suggestions
    );
    let mut hosted = mlx();
    hosted.provider = "anthropic".into();
    let hosted =
        ModelSettingsOverlay::new("cloud".into(), hosted).with_suggestions(dir.path(), base);
    assert!(hosted.suggestions.is_empty());
}
