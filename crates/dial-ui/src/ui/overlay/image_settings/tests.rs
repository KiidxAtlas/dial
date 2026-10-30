use super::*;
use crate::ui::overlay::settings::testkit::{Rig, ch, key, render, screen, status_preview};

const MLX: &[&str] = &["size", "steps", "seed", "strength", "loras"];
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
