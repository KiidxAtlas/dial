use super::*;
use crate::ui::overlay::settings::testkit::{
    Rig, ch, ctrl, key, released, remapped_keys, render, screen, status_preview,
};

const MLX: &[&str] = &["size", "steps", "seed", "strength", "loras", "cond_weights"];
const OPENAI: &[&str] = &["size", "quality", "background", "moderation"];

fn overlay(supported: &[&str]) -> ImageSettingsOverlay {
    ImageSettingsOverlay::new(
        "image_test".into(),
        "MLX-Serve",
        ImageModelSettings {
            model: "mlx-community/flux2-klein-9b-4bit".into(),
            ..Default::default()
        },
        supported,
        &["detail.safetensors".into(), "style.safetensors".into()],
    )
}

fn go(overlay: &mut ImageSettingsOverlay, path: &str) {
    overlay.cursor = overlay.fields.iter().position(|f| f.path == path).unwrap();
}

/// Sends `event` and applies the resulting save as if the controller accepted it.
fn saved(overlay: &mut ImageSettingsOverlay, event: Event) -> toml::Value {
    let rig = Rig::new();
    let OverlayEvent::Set { path, value, .. } = overlay.event(&event, &rig.keys) else {
        panic!("expected a save");
    };
    assert_eq!(path, "image.models.image_test");
    overlay.settled(Ok(()));
    value
}

#[test]
fn rows_follow_the_settings_the_image_api_accepts() {
    let paths = |o: &ImageSettingsOverlay| o.fields.iter().map(|f| f.path).collect::<Vec<_>>();
    assert_eq!(
        paths(&overlay(MLX)),
        [
            "size",
            "steps",
            "seed",
            "strength",
            "cond_weights",
            "lora",
            "lora_scale",
            "loras"
        ]
    );
    assert_eq!(
        paths(&overlay(OPENAI)),
        ["size", "quality", "background", "moderation"]
    );
    assert!(overlay(&[]).fields.is_empty());
}

#[test]
fn arrows_step_numbers_cycle_loras_and_save_the_whole_entry() {
    let rig = Rig::new();
    let mut overlay = overlay(MLX);
    go(&mut overlay, "steps");
    let value = saved(&mut overlay, key(KeyCode::Right));
    assert_eq!(value["steps"].as_integer(), Some(8));
    assert_eq!(
        value["model"].as_str(),
        Some("mlx-community/flux2-klein-9b-4bit")
    );
    let value = saved(&mut overlay, key(KeyCode::Right));
    assert_eq!(value["steps"].as_integer(), Some(9));

    // The scale needs a LoRA; choosing one starts at full strength.
    go(&mut overlay, LORA_SCALE);
    assert!(matches!(
        overlay.event(&key(KeyCode::Right), &rig.keys),
        OverlayEvent::None
    ));
    assert!(
        overlay
            .message
            .as_ref()
            .is_some_and(|(m, error)| *error && m.contains("LoRA"))
    );
    go(&mut overlay, LORA);
    saved(&mut overlay, key(KeyCode::Right));
    assert_eq!(
        overlay.settings.loras,
        [ImageLora {
            path: "detail.safetensors".into(),
            scale: 1.0
        }]
    );
    go(&mut overlay, LORA_SCALE);
    saved(&mut overlay, key(KeyCode::Left));
    assert_eq!(overlay.settings.loras[0].scale, 0.95);

    // Reset removes the LoRA and leaves the other values alone.
    go(&mut overlay, LORA);
    saved(&mut overlay, ch('r'));
    assert!(overlay.settings.loras.is_empty());
    assert_eq!(overlay.settings.steps, Some(9));
}

#[test]
fn typed_values_are_validated_and_failed_saves_keep_the_old_value() {
    let rig = Rig::new();
    let mut overlay = overlay(MLX);
    assert!(overlay.candidate("steps", "0").is_err());

    // Layer weights take any count (it depends on the model), comma or space separated.
    let weights = overlay.candidate("cond_weights", "1, 0.5  2").unwrap();
    assert_eq!(weights.cond_weights, [1.0, 0.5, 2.0]);
    assert!(overlay.candidate("cond_weights", "1 x").is_err());
    assert!(overlay.candidate("cond_weights", "1 inf").is_err());
    let twelve = ["1"; 12].join(" ");
    assert_eq!(
        overlay
            .candidate("cond_weights", &twelve)
            .unwrap()
            .cond_weights
            .len(),
        12
    );
    assert!(
        overlay
            .candidate("cond_weights", "")
            .unwrap()
            .cond_weights
            .is_empty()
    );
    assert!(overlay.candidate("size", "\"512x512\"").is_err());
    assert!(
        overlay
            .candidate("loras", "[{ path = \"a.bin\", scale = 1.0 }]")
            .is_err()
    );
    let stacked = overlay
        .candidate(
            "loras",
            "[{ path = \"a.safetensors\" }, { path = \"/x/b.safetensors\", scale = 0.5 }]",
        )
        .unwrap();
    assert_eq!(stacked.loras.len(), 2);
    assert_eq!(stacked.loras[0].scale, 1.0);

    // A seed is typed exactly; a refused save does not change what the panel shows.
    go(&mut overlay, "seed");
    overlay.event(&key(KeyCode::Enter), &rig.keys);
    for c in "1234".chars() {
        overlay.event(&ch(c), &rig.keys);
    }
    let event = overlay.event(&key(KeyCode::Enter), &rig.keys);
    assert!(matches!(event, OverlayEvent::Set { .. }));
    overlay.settled(Err("disk full".into()));
    assert_eq!(overlay.settings.seed, None);
    assert!(overlay.message.as_ref().is_some_and(|(_, error)| *error));
}

#[test]
fn page_names_the_model_api_and_values_and_survives_tiny_terminals() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut overlay = overlay(MLX);
    overlay.settings.steps = Some(4);
    overlay.settings.loras = vec![ImageLora {
        path: "style.safetensors".into(),
        scale: 1.25,
    }];
    go(&mut overlay, "steps");
    let text = screen(&render(100, 30, |frame| overlay.draw(frame, &ctx)));
    for needle in [
        " Image settings",
        "flux2-klein-9b-4bit",
        "MLX-Serve",
        "DIFFUSION",
        "Steps",
        "‹ 4 ›",
        "style.safetensors × 1.25",
        "2 custom values",
        "Turbo",
        "Accepts 1 to 200",
        "reset",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
    let _ = std::fs::create_dir_all("../../target/ui-previews");
    let _ = std::fs::write("../../target/ui-previews/image-settings.txt", &text);

    for (w, h) in [(20, 6), (4, 3), (100, 8)] {
        render(w, h, |frame| overlay.draw(frame, &ctx));
    }
    let mut empty = super::ImageSettingsOverlay::new(
        "k".into(),
        "xAI Images",
        ImageModelSettings {
            model: "grok".into(),
            ..Default::default()
        },
        &[],
        &[],
    );
    let text = screen(&render(80, 10, |frame| empty.draw(frame, &ctx)));
    assert!(text.contains("accepts no adjustable settings"), "{text}");
    assert!(matches!(
        empty.event(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::Close
    ));
}

#[test]
fn native_provider_settings_are_editable_and_validated() {
    let mut queued = overlay(&["size", "seed", "input"]);
    assert_eq!(
        queued.fields.iter().map(|f| f.path).collect::<Vec<_>>(),
        ["size", "seed", "input"]
    );
    go(&mut queued, "input");
    let OverlayEvent::Set { value, .. } = queued.apply("{ guidance_scale = 3.5, custom = true }")
    else {
        panic!("valid model input must be saved");
    };
    let saved: ImageModelSettings = value.try_into().unwrap();
    assert_eq!(saved.input["guidance_scale"], serde_json::json!(3.5));
    queued.settled(Ok(()));
    assert_eq!(queued.settings, saved);
    assert!(matches!(queued.apply("42"), OverlayEvent::None));

    let mut comfy = overlay(&["comfyui"]);
    assert_eq!(comfy.fields.len(), 1);
    assert_eq!(comfy.fields[0].path, "comfyui");
    assert!(matches!(
        comfy.apply("{ workflow = {}, prompt_node = \"6\", output_node = \"9\" }"),
        OverlayEvent::None
    ));
    let event = comfy.apply(r#"{ workflow = { "6" = { class_type = "CLIPTextEncode", inputs = { text = "old" } }, "9" = { class_type = "SaveImage", inputs = {} } }, prompt_node = "6", output_node = "9" }"#);
    assert!(matches!(event, OverlayEvent::Set { .. }));
    comfy.settled(Ok(()));
    assert!(comfy.settings.comfyui.is_some());
}

#[test]
fn remapped_keys_drive_paging_editing_and_cancel() {
    let rig = Rig::new();
    let keys = remapped_keys();
    let all = [
        "size",
        "quality",
        "background",
        "moderation",
        "steps",
        "seed",
        "strength",
        "cond_weights",
        "loras",
        "input",
        "comfyui",
    ];
    let mut overlay = overlay(&all);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let _ = render(70, 12, |frame| overlay.draw(frame, &ctx));
    assert!(
        overlay.page > 0 && overlay.page < overlay.fields.len(),
        "a short panel pages by fewer rows than it has"
    );
    let page = overlay.page;
    overlay.event(&ctrl('n'), &keys);
    assert_eq!(overlay.cursor, page, "the remapped page down moves a page");
    overlay.event(&ctrl('p'), &keys);
    assert_eq!(overlay.cursor, 0, "the remapped page up moves back");
    overlay.event(&key(KeyCode::PageDown), &keys);
    assert_eq!(overlay.cursor, 0, "PageDown is no longer bound to paging");

    go(&mut overlay, "steps");
    overlay.event(&ctrl('y'), &keys);
    assert!(
        overlay.editor.is_some(),
        "the remapped confirm opens the editor"
    );
    overlay.event(&key(KeyCode::Enter), &keys);
    assert!(overlay.editor.is_some(), "Enter no longer confirms");
    overlay.event(&ch('1'), &keys);
    overlay.event(&ch('2'), &keys);
    let OverlayEvent::Set { value, .. } = overlay.event(&ctrl('y'), &keys) else {
        panic!("the remapped confirm saves the typed value");
    };
    assert_eq!(
        value.get("steps").and_then(toml::Value::as_integer),
        Some(12)
    );
    overlay.settled(Ok(()));

    overlay.event(&ctrl('y'), &keys);
    overlay.event(&ctrl('g'), &keys);
    assert!(
        overlay.editor.is_none(),
        "the remapped cancel discards the editor"
    );
    assert!(
        matches!(overlay.event(&ctrl('g'), &keys), OverlayEvent::Close),
        "the remapped cancel closes the form"
    );
    assert!(
        matches!(overlay.event(&key(KeyCode::Esc), &keys), OverlayEvent::None),
        "Esc is no longer cancel"
    );
}

/// Press then release of each key: the press acts once and the release changes nothing, whether or
/// not an editor is open. Remapped confirm and cancel open, commit and discard only on their press.
#[test]
fn released_keys_never_act_and_remapped_keys_drive_the_editor() {
    let rig = Rig::new();
    let state =
        |o: &ImageSettingsOverlay| (o.cursor, o.scope, o.editor.is_some(), o.message.is_some());
    for code in [
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Tab,
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Backspace,
        KeyCode::Char('r'),
        KeyCode::Char('s'),
    ] {
        let mut overlay = overlay(MLX);
        go(&mut overlay, "steps");
        let before = state(&overlay);
        overlay.event(&key(code), &rig.keys);
        let pressed = state(&overlay);
        if code == KeyCode::Char('s') {
            assert!(pressed.1 != before.1, "the press switches the scope once");
        }
        assert!(
            matches!(
                overlay.event(&released(key(code)), &rig.keys),
                OverlayEvent::None
            ),
            "{code:?} released acted on the form"
        );
        assert!(
            state(&overlay) == pressed,
            "{code:?} released changed the form"
        );
    }

    let keys = remapped_keys();
    let mut overlay = overlay(MLX);
    go(&mut overlay, "steps");
    assert!(matches!(
        overlay.event(&released(ctrl('y')), &keys),
        OverlayEvent::None
    ));
    assert!(
        overlay.editor.is_none(),
        "a released confirm opened the editor"
    );
    overlay.event(&ctrl('y'), &keys);
    assert!(matches!(
        overlay.event(&released(ctrl('y')), &keys),
        OverlayEvent::None
    ));
    overlay.event(&ch('7'), &keys);
    assert!(matches!(
        overlay.event(&released(ctrl('g')), &keys),
        OverlayEvent::None
    ));
    assert_eq!(
        overlay.editor.as_ref().map(|e| e.text.as_str()),
        Some("7"),
        "a released key changed the typed value"
    );
    let rendered = screen(&render(100, 30, |frame| {
        overlay.draw(
            frame,
            &DrawCtx {
                theme: &rig.theme,
                keys: &keys,
                status: &rig.status,
                preview_status: &status_preview,
            },
        )
    }));
    assert!(rendered.contains("Ctrl+Y save"), "{rendered}");
    overlay.event(&ctrl('g'), &keys);
    assert!(
        overlay.editor.is_none(),
        "the remapped cancel press discards the editor"
    );
}
