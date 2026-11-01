use super::*;

fn parse(text: &str) -> Result<SnapcompactConfig, toml::de::Error> {
    toml::from_str(text)
}

fn resolve(
    config: &SnapcompactConfig,
    model: &str,
    family: SnapcompactFamily,
) -> SnapcompactConfig {
    config.resolved_for_model(model, family).into_owned()
}

#[test]
fn overlay_is_off_by_default_and_absent_from_old_configs() {
    assert!(!SnapcompactConfig::default().overlay);
    assert!(!parse("").unwrap().overlay);
    assert!(!parse("shape = \"8x13-bw\"").unwrap().overlay);
}

#[test]
fn overlay_parses_both_values_and_rejects_non_boolean_values() {
    assert!(parse("overlay = true").unwrap().overlay);
    assert!(!parse("overlay = false").unwrap().overlay);
    assert!(parse("overlay = \"yes\"").is_err());
    assert!(parse("overlay = 1").is_err());
}

#[test]
fn overlay_survives_serde_roundtrip() {
    let config = parse("overlay = true").unwrap();
    let text = toml::to_string(&config).unwrap();
    assert_eq!(parse(&text).unwrap(), config);
}

#[test]
fn resolved_for_provider_preserves_overlay_for_auto_and_explicit_shapes() {
    for shape in ["auto", "8x13-bw"] {
        for overlay in [false, true] {
            let config = SnapcompactConfig {
                shape: shape.into(),
                overlay,
                ..SnapcompactConfig::default()
            };
            for anthropic in [false, true] {
                let resolved = config.resolved_for_provider(anthropic);
                assert_eq!(resolved.overlay, overlay, "shape {shape}");
            }
        }
    }
}

#[test]
fn default_leaves_frame_cap_and_detail_on_auto() {
    let config = SnapcompactConfig::default();
    assert_eq!(config.frame_size, None);
    assert_eq!(config.max_edge, None);
    assert_eq!(config.image_detail, ImageDetail::Auto);
}

#[test]
fn frame_size_and_max_edge_accept_only_the_inclusive_pixel_range() {
    for key in ["frame_size", "max_edge"] {
        for value in ["-1", "0", "1", "63", "2577", "99999"] {
            assert!(
                parse(&format!("{key} = {value}")).is_err(),
                "{key} = {value}"
            );
        }
        for value in ["64", "2576"] {
            assert!(
                parse(&format!("{key} = {value}")).is_ok(),
                "{key} = {value}"
            );
        }
    }
}

#[test]
fn an_out_of_range_frame_size_is_rejected_even_when_max_edge_is_valid() {
    assert!(parse("frame_size = 1\nmax_edge = 64").is_err());
}

#[test]
fn auto_markers_parse_to_none_and_malformed_or_unknown_settings_are_rejected() {
    let config =
        parse("frame_size = \"auto\"\nmax_edge = \"auto\"\nimage_detail = \"original\"").unwrap();
    assert_eq!(config.frame_size, None);
    assert_eq!(config.max_edge, None);
    assert_eq!(config.image_detail, ImageDetail::Original);
    assert!(parse("frame_size = \"large\"").is_err());
    assert!(parse("image_detail = \"medium\"").is_err());
    assert!(parse("unknown_knob = true").is_err());
}

#[test]
fn frame_detail_and_cap_survive_serde_roundtrip_including_auto() {
    for text in [
        "",
        "frame_size = 900\nmax_edge = 1200\nimage_detail = \"high\"",
        "frame_size = \"auto\"\nmax_edge = 2000",
    ] {
        let config = parse(text).unwrap();
        let rendered = toml::to_string(&config).unwrap();
        assert_eq!(parse(&rendered).unwrap(), config, "{text}");
    }
}

#[test]
fn known_model_presets_pick_layout_and_ideal_frame_per_family() {
    let auto = SnapcompactConfig::default();
    let cases = [
        ("claude-opus-4-7", "11on16-bw", 1932),
        ("openrouter/anthropic/claude-opus-4-8", "11on16-bw", 1932),
        ("claude-fable-latest", "11on16-bw", 1932),
        ("claude-mythos-preview", "11on16-bw", 1932),
        ("claude-opus-4-6", "11on16-bw", 1568),
        ("claude-sonnet-4-7", "11on16-bw", 1568),
        ("google/gemini-2.5-pro", "8on22-bw", 2048),
        ("gpt-5.4", "8on22-bw", 1568),
        ("kimi-k2.5", "8on22-bw", 1568),
        ("glm-4.6", "8on16-bw", 1568),
    ];
    for (model, shape, frame) in cases {
        let resolved = resolve(&auto, model, SnapcompactFamily::Unknown);
        assert_eq!(resolved.shape, shape, "{model} shape");
        assert_eq!(resolved.frame_size, Some(frame), "{model} frame");
    }
}

#[test]
fn bedrock_regional_claude_ids_get_the_claude_layout_and_large_frame() {
    let auto = SnapcompactConfig::default();
    for model in [
        "us.anthropic.claude-opus-4-7-v1:0",
        "eu.anthropic.claude-opus-4-7-v1:0",
    ] {
        let resolved = resolve(&auto, model, SnapcompactFamily::Unknown);
        assert_eq!(resolved.shape, "11on16-bw", "{model}");
        assert_eq!(resolved.frame_size, Some(1932), "{model}");
    }
}

#[test]
fn codename_lookalikes_do_not_get_the_fable_or_mythos_frame() {
    let auto = SnapcompactConfig::default();
    for model in ["claude-fableproxy", "claude-mythosproxy"] {
        let resolved = resolve(&auto, model, SnapcompactFamily::Unknown);
        assert_eq!(resolved.frame_size, Some(1568), "{model}");
    }
}

#[test]
fn unknown_models_fall_back_by_wire_family_while_known_names_win() {
    let auto = SnapcompactConfig::default();
    let anthropic = resolve(&auto, "private-model", SnapcompactFamily::Anthropic);
    assert_eq!(anthropic.shape, "11on16-bw");
    assert_eq!(anthropic.frame_size, Some(1568));
    for family in [
        SnapcompactFamily::Google,
        SnapcompactFamily::OpenAi,
        SnapcompactFamily::Unknown,
    ] {
        let resolved = resolve(&auto, "private-model", family);
        assert_eq!(resolved.shape, "8on22-bw", "{family:?}");
        assert_eq!(resolved.frame_size, Some(1568), "{family:?}");
    }
    let known = resolve(&auto, "gemini-2.5-pro", SnapcompactFamily::Anthropic);
    assert_eq!(known.shape, "8on22-bw");
    assert_eq!(known.frame_size, Some(2048));
}

#[test]
fn explicit_shape_keeps_its_native_baseline_for_every_model() {
    for (shape, frame) in [("5x8-bw", 2576), ("5x8-sent", 2576), ("8x13-bw", 1568)] {
        let config = SnapcompactConfig {
            shape: shape.into(),
            ..SnapcompactConfig::default()
        };
        for model in ["claude-opus-4-7", "gemini-2.5-pro", "private-model"] {
            let resolved = resolve(&config, model, SnapcompactFamily::Anthropic);
            assert_eq!(resolved.shape, shape, "{model}");
            assert_eq!(resolved.frame_size, Some(frame), "{shape} on {model}");
        }
    }
}

#[test]
fn explicit_frame_size_overrides_the_model_default_and_shape_baseline() {
    let config = SnapcompactConfig {
        frame_size: Some(800),
        ..SnapcompactConfig::default()
    };
    for shape in ["auto", "5x8-bw"] {
        let config = SnapcompactConfig {
            shape: shape.into(),
            ..config.clone()
        };
        for model in ["claude-opus-4-7", "gemini-2.5-pro", "private-model"] {
            let resolved = resolve(&config, model, SnapcompactFamily::Anthropic);
            assert_eq!(resolved.frame_size, Some(800), "{shape} on {model}");
        }
    }
}

#[test]
fn max_edge_caps_defaults_and_explicit_frames_but_never_raises_them() {
    let capped = SnapcompactConfig {
        max_edge: Some(1000),
        ..SnapcompactConfig::default()
    };
    assert_eq!(
        resolve(&capped, "claude-opus-4-7", SnapcompactFamily::Unknown).frame_size,
        Some(1000),
        "1932 default is capped"
    );
    let native_5x8 = SnapcompactConfig {
        shape: "5x8-bw".into(),
        ..capped.clone()
    };
    assert_eq!(
        resolve(&native_5x8, "private-model", SnapcompactFamily::Unknown).frame_size,
        Some(1000),
        "5x8 native 2576 is capped"
    );

    let loose = SnapcompactConfig {
        max_edge: Some(2576),
        ..SnapcompactConfig::default()
    };
    let gemini = resolve(&loose, "gemini-2.5-pro", SnapcompactFamily::Unknown);
    assert_eq!(
        gemini.frame_size,
        Some(2048),
        "a looser cap keeps the default"
    );
    assert_eq!(gemini.max_edge, Some(2576), "the cap is kept as configured");

    let explicit = SnapcompactConfig {
        frame_size: Some(2000),
        max_edge: Some(1500),
        ..SnapcompactConfig::default()
    };
    assert_eq!(
        resolve(&explicit, "private-model", SnapcompactFamily::Unknown).frame_size,
        Some(1500),
        "the cap also clamps an explicit frame"
    );
}

#[test]
fn explicit_image_detail_wins_over_the_wire_and_model_policy() {
    for (detail, expected) in [
        (ImageDetail::High, ImageDetail::High),
        (ImageDetail::Original, ImageDetail::Original),
    ] {
        let config = SnapcompactConfig {
            image_detail: detail,
            ..SnapcompactConfig::default()
        };
        for family in [
            SnapcompactFamily::OpenAi,
            SnapcompactFamily::Anthropic,
            SnapcompactFamily::Unknown,
        ] {
            let resolved = resolve(&config, "gpt-5.4", family);
            assert_eq!(resolved.image_detail, expected, "{detail:?} on {family:?}");
        }
    }
}

#[test]
fn auto_detail_is_original_only_for_gpt_5_4_on_the_openai_wire() {
    let auto = SnapcompactConfig::default();
    let cases = [
        ("gpt-5.4", SnapcompactFamily::OpenAi, ImageDetail::Original),
        (
            "codex-5.4",
            SnapcompactFamily::OpenAi,
            ImageDetail::Original,
        ),
        ("gpt-5.3", SnapcompactFamily::OpenAi, ImageDetail::High),
        ("gpt-5.4", SnapcompactFamily::Unknown, ImageDetail::High),
        ("gpt-5.4", SnapcompactFamily::Anthropic, ImageDetail::High),
        (
            "claude-opus-4-7",
            SnapcompactFamily::OpenAi,
            ImageDetail::High,
        ),
        (
            "private-model",
            SnapcompactFamily::OpenAi,
            ImageDetail::High,
        ),
    ];
    for (model, family, expected) in cases {
        let resolved = resolve(&auto, model, family);
        assert_eq!(resolved.image_detail, expected, "{model} on {family:?}");
    }
}

#[test]
fn resolved_config_re_resolves_to_itself_so_later_requests_cannot_change_it() {
    let configs = [
        SnapcompactConfig::default(),
        SnapcompactConfig {
            max_edge: Some(1200),
            ..SnapcompactConfig::default()
        },
        SnapcompactConfig {
            shape: "5x8-bw".into(),
            ..SnapcompactConfig::default()
        },
    ];
    for config in configs {
        let first = resolve(&config, "claude-opus-4-7", SnapcompactFamily::Anthropic);
        let again = first.resolved_for_model("claude-opus-4-7", SnapcompactFamily::Anthropic);
        assert!(matches!(again, Cow::Borrowed(_)), "{first:?}");
        assert_eq!(*again, first);
        let later = first.resolved_for_model("gemini-2.5-pro", SnapcompactFamily::Google);
        assert_eq!(
            *later, first,
            "a later model or wire changed a resolved config"
        );
    }
}

#[test]
fn standalone_provider_resolution_is_idempotent() {
    let first = SnapcompactConfig::default()
        .resolved_for_provider(false)
        .into_owned();
    let again = first.resolved_for_provider(false);
    assert!(matches!(again, Cow::Borrowed(_)));
    assert_eq!(*again, first);
}
