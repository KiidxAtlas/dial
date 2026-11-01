//! Regression tests for lossless escaping of unrenderable glyphs and for honest frame capacity.
//! Filter: `cargo test agent::compact::snapcompact::tests`.
use crate::config::snapcompact::SnapcompactConfig;

use super::views::{self, ViewContext};
use super::*;
use crate::config::compaction::{CompactionConfig, CompactionIdentity};
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use std::sync::atomic::AtomicBool;

const ARABIC_TEXT: &str = "مرحبا ه";
const ARABIC_HEH: char = '\u{0647}';

/// Decodes the JSON string escapes the archive emits, as a reader of the archive would.
/// Raw characters pass through; `\uXXXX` pairs are combined as UTF-16 before UTF-8 decoding.
fn decode_json_escapes(text: &str) -> String {
    let mut units: Vec<u16> = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0u16; 2];
            units.extend_from_slice(ch.encode_utf16(&mut buf));
            continue;
        }
        match chars.next().expect("dangling escape") {
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                assert_eq!(hex.len(), 4, "truncated \\u escape");
                units.push(u16::from_str_radix(&hex, 16).expect("four hex digits"));
            }
            '\\' => units.push(u16::from(b'\\')),
            '"' => units.push(u16::from(b'"')),
            'n' => units.push(u16::from(b'\n')),
            other => panic!("unexpected escape \\{other}"),
        }
    }
    String::from_utf16(&units).expect("valid UTF-16")
}

fn escape(text: &str, font: &str) -> String {
    let cancel = AtomicBool::new(false);
    renderer::escape_unrenderable(text, font, &cancel).expect("escape succeeds")
}

fn validate(text: &str, font: &str) -> Result<()> {
    let cancel = AtomicBool::new(false);
    renderer::validate_text(text, font, &cancel)
}

fn archive_text(text: &str, shape: &str, max_frames: usize) -> Result<Archive> {
    let cancel = AtomicBool::new(false);
    let config = SnapcompactConfig {
        shape: shape.to_owned(),
        ..SnapcompactConfig::default()
    };
    let messages = [json!({"role": "user", "content": text})];
    archive_with_options(&messages, &config, max_frames, &cancel)
}

#[test]
fn arabic_heh_is_escaped_exactly_and_round_trips() {
    assert!(ARABIC_TEXT.contains(ARABIC_HEH));
    for font in ["5x8", "6x12", "8x13", "silver"] {
        let escaped = escape(ARABIC_TEXT, font);
        assert!(!escaped.contains(ARABIC_HEH), "{font} kept a raw heh");
        assert!(escaped.is_ascii(), "{font}: {escaped}");
        assert!(escaped.contains("\\u0647"), "{font}: {escaped}");
        assert_eq!(decode_json_escapes(&escaped), ARABIC_TEXT, "{font}");
        validate(&escaped, font).expect("escaped output is renderable");
    }
}

#[test]
fn raw_unsupported_glyphs_remain_refused_by_validation() {
    // Tofu must never pass validation, so an escaping regression cannot hide as silent glyphs.
    for font in ["5x8", "8x13", "silver"] {
        let err = validate("\u{647}", font).expect_err("raw Arabic heh has no glyph");
        assert!(err.to_string().contains("U+0647"), "{font}: {err}");
    }
}

#[test]
fn controls_layout_codepoints_and_astral_characters_are_escaped() {
    let original = "a\u{0}b\u{7f}c\u{85}d\u{e}e\u{f}f\u{2588}g\u{200f}h\u{1f600}i\u{feff}";
    let escaped = escape(original, "8x13");
    for raw in [
        '\u{0}',
        '\u{7f}',
        '\u{85}',
        '\u{e}',
        '\u{f}',
        '\u{2588}',
        '\u{200f}',
        '\u{1f600}',
        '\u{feff}',
    ] {
        assert!(!escaped.contains(raw), "raw U+{:04X} survived", raw as u32);
    }
    assert!(escaped.contains("\\u2588"), "{escaped}");
    // Astral code points use a JSON surrogate pair, not a raw or invented escape.
    assert!(escaped.contains("\\ud83d\\ude00"), "{escaped}");
    assert_eq!(decode_json_escapes(&escaped), original);
    validate(&escaped, "8x13").expect("escaped output is renderable");
}

#[test]
fn combining_marks_keep_their_own_cells_and_unsupported_marks_are_escaped() {
    // U+0301 has a Silver glyph: it stays its own code point so cell accounting matches the source.
    let supported = "e\u{301}";
    assert_eq!(escape(supported, "silver"), supported);
    // U+0300 has no Silver glyph and must not be drawn as tofu or merged into the base letter.
    assert_eq!(escape("e\u{300}", "silver"), "e\\u0300");
    assert_eq!(
        decode_json_escapes(&escape("e\u{300}", "silver")),
        "e\u{300}"
    );
}

#[test]
fn newlines_are_kept_for_boundary_mapping() {
    assert_eq!(escape("a\nb\u{647}", "8x13"), "a\nb\\u0647");
}

#[test]
fn literal_backslashes_and_unicode_escapes_are_not_reinterpreted() {
    let original = "literal \\u0647, C:\\new\\test, \\\\, and ه";
    let escaped = escape(original, "8x13");
    assert_eq!(decode_json_escapes(&escaped), original);
    validate(&escaped, "8x13").unwrap();
}

#[test]
fn archive_keeps_original_source_and_renders_without_tofu() {
    let original = format!("{ARABIC_TEXT}\u{200f}\u{0}\u{2588}\n{ARABIC_HEH}");
    for shape in ["8x13-bw", "silver16-bw"] {
        let archive = archive_text(&original, shape, 64).expect(shape);
        let meta = archive.messages[0]
            .get("dial_snapcompact")
            .expect("snapcompact metadata");
        let source = meta["source_text"].as_str().expect("source_text");
        // The durable source keeps the exact characters; only the rendered copy is escaped.
        assert!(source.contains(ARABIC_TEXT), "{shape}");
        assert!(source.contains('\u{2588}'), "{shape}");
        assert!(!archive.frames.is_empty(), "{shape}");
        assert_eq!(archive.source_bytes, source.len(), "{shape}");
    }
}

#[test]
fn escaped_arabic_consumes_frame_capacity_and_overflow_is_refused() {
    let shape = "8x13-bw";
    let latin = "h".repeat(6000);
    let arabic = ARABIC_HEH.to_string().repeat(6000);
    let ascii = archive_text(&latin, shape, 64).expect("ASCII fits one frame");
    let escaped = archive_text(&arabic, shape, 64).expect("escaped Arabic archives");
    assert_eq!(ascii.frames.len(), 1);
    assert!(escaped.frames.len() > ascii.frames.len());
    let needed = escaped.frames.len();
    let err = archive_text(&arabic, shape, needed - 1).expect_err("overflow must not truncate");
    assert!(
        err.to_string().contains("refused without truncating"),
        "{err}"
    );
    let exact = archive_text(&arabic, shape, needed).expect("exact budget fits");
    assert_eq!(exact.frames.len(), needed);
}

#[test]
fn default_config_keeps_the_version_one_archive_without_layers() {
    let archive = archive_text("hello", "8x13-bw", 64).expect("archive");
    let meta = archive.messages[0]
        .get("dial_snapcompact")
        .expect("snapcompact metadata");
    assert_eq!(meta["version"], json!(1));
    assert!(
        meta.get("layers").is_none(),
        "default archives must not carry layered metadata"
    );
}

#[test]
fn cancelled_archive_fails_closed_without_output() {
    let cancel = AtomicBool::new(true);
    let config = SnapcompactConfig::default();
    let messages = [json!({"role": "user", "content": "hello"})];
    let result = archive_with_options(&messages, &config, 64, &cancel);
    assert!(result.is_err(), "cancelled archive must not return output");
}

/// Writes a layered overlay preview for visual inspection. Opt-in only: it needs an explicit
/// `DIAL_SNAPCOMPACT_PREVIEW_DIR` and uses only synthetic conversation text. Each generation
/// compacts the previous archive plus one new message, saving the composite frames and the clean
/// software-rendered view of the newest real layer.
/// Run: `DIAL_SNAPCOMPACT_PREVIEW_DIR=/tmp/preview cargo test -p dial-agent writes_layered_overlay_preview_when_requested -- --ignored`
#[test]
#[ignore = "writes visual fixtures; set DIAL_SNAPCOMPACT_PREVIEW_DIR to opt in"]
fn writes_layered_overlay_preview_when_requested() {
    let Some(dir) = std::env::var_os("DIAL_SNAPCOMPACT_PREVIEW_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("preview directory is creatable");
    let cancel = AtomicBool::new(false);
    let config = SnapcompactConfig {
        overlay: true,
        ..SnapcompactConfig::default()
    };
    let policy = CompactionConfig::default();
    let texts = [
        "First synthetic generation: alpha 0123",
        "Second synthetic generation: beta 4567",
        "Third synthetic generation: gamma 89AB",
    ];
    let mut messages: Vec<Value> = Vec::new();
    for (generation, text) in texts.iter().enumerate() {
        messages.push(json!({"role": "user", "content": text}));
        let archive =
            archive_with_options(&messages, &config, 64, &cancel).expect("overlay archive renders");
        messages = archive.messages;
        let content = messages[0]["content"]
            .as_array()
            .expect("archive content is an array");
        let mut frames = 0;
        for part in content {
            let Some(url) = part["image_url"]["url"].as_str() else {
                continue;
            };
            let encoded = url
                .strip_prefix("data:image/png;base64,")
                .expect("generated frames are PNG data URLs");
            let png = STANDARD.decode(encoded).expect("frame is valid base64");
            frames += 1;
            let path = dir.join(format!("composite-{generation}-frame-{frames}.png"));
            std::fs::write(&path, png).expect("composite frame is writable");
        }
        assert!(frames > 0, "generation {generation} produced no frames");
        let newest = crate::storage::snapcompact::layers(&messages)
            .expect("layers parse")
            .pop()
            .expect("archive has a layer");
        assert!(
            newest.source_text.contains(text),
            "newest layer must hold generation {generation}'s source"
        );
        let clean = views::call(
            &messages,
            &json!({"view": "layers", "layer": newest.id, "query": "", "format": "image"}),
            ViewContext {
                policy: &policy,
                identity: CompactionIdentity {
                    provider: "synthetic",
                    model: "synthetic",
                    agent: None,
                },
                window: 1_000_000,
                vision: true,
                family: crate::config::snapcompact::SnapcompactFamily::Unknown,
            },
            &cancel,
        );
        assert!(!clean.is_error, "clean view failed: {}", clean.text);
        assert!(
            clean.text.contains(&newest.id),
            "clean view must print the recovered layer id"
        );
        let encoded = clean
            .data
            .as_ref()
            .and_then(|data| data["artifact"]["data"].as_str())
            .expect("clean view carries a PNG artifact");
        let png = STANDARD
            .decode(encoded)
            .expect("clean view is valid base64");
        std::fs::write(dir.join(format!("clean-{generation}.png")), png)
            .expect("clean view is writable");
        messages.push(json!({"role": "user", "content": "following synthetic message"}));
    }
}

fn overlay_archive(messages: &[Value], max_frames: usize) -> Result<Archive> {
    let cancel = AtomicBool::new(false);
    let config = SnapcompactConfig {
        overlay: true,
        ..SnapcompactConfig::default()
    };
    archive_with_options(messages, &config, max_frames, &cancel)
}

fn layered_generations(count: usize) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    for generation in 0..count {
        messages.push(json!({
            "role": "user",
            "content": format!("synthetic generation {generation} token {generation:04}"),
        }));
        let archive = overlay_archive(&messages, 64).expect("overlay generation renders");
        messages = archive.messages;
        messages.push(json!({"role": "user", "content": format!("following {generation}")}));
    }
    messages
}

#[test]
fn overlay_archive_is_version_two_with_distinct_colors_per_generation() {
    let messages = layered_generations(3);
    assert_eq!(messages[0]["dial_snapcompact"]["version"], json!(2));
    let layers = crate::storage::snapcompact::layers(&messages).expect("layers parse");
    assert_eq!(layers.len(), 3, "one layer per compaction generation");
    let colors: std::collections::BTreeSet<[u8; 3]> =
        layers.iter().map(|layer| layer.color).collect();
    assert_eq!(colors.len(), layers.len(), "generation colors must differ");
}

#[test]
fn overlay_layers_keep_exact_source_text_messages_and_frames_per_generation() {
    let messages = layered_generations(2);
    let layers = crate::storage::snapcompact::layers(&messages).expect("layers parse");
    assert_eq!(layers.len(), 2);
    assert_ne!(
        layers[0].id, layers[1].id,
        "distinct generations, distinct ids"
    );
    for (generation, layer) in layers.iter().enumerate() {
        let token = format!("token {generation:04}");
        assert!(
            layer.source_text.contains(&token),
            "generation {generation} lost its text"
        );
        assert!(
            !layer.messages.is_empty(),
            "generation {generation} keeps exact messages"
        );
        assert!(
            layer
                .messages
                .iter()
                .all(|m| m.get("dial_snapcompact").is_none()),
            "layers never store archive containers"
        );
        assert!(
            !layer.frames.is_empty(),
            "generation {generation} keeps its frames"
        );
    }
}

#[test]
fn overlay_composite_page_count_is_the_larger_generation_not_the_sum() {
    let messages = layered_generations(2);
    let layers = crate::storage::snapcompact::layers(&messages).expect("layers parse");
    let counts: Vec<usize> = layers.iter().map(|layer| layer.frames.len()).collect();
    let widest = counts.iter().copied().max().unwrap_or(0);
    let sum: usize = counts.iter().sum();
    assert!(
        widest > 0 && sum > widest,
        "fixture must distinguish max from sum"
    );
    let composite = overlay_archive(&messages, 64).expect("composite renders");
    assert_eq!(
        composite.frames.len(),
        widest,
        "pages are max(old, new), not old + new"
    );
}

#[test]
fn overlay_frames_are_priced_and_point_at_image_parts() {
    let messages = layered_generations(2);
    let archive = overlay_archive(&messages, 64).expect("overlay renders");
    assert!(archive.source_bytes > 0);
    for frame in &archive.frames {
        assert!(frame.token_upper_bound > 0, "every frame is priced");
        let part = &archive.messages[0]["content"][frame.content_index];
        assert_eq!(part["type"], "image_url", "frame index names an image part");
    }
}

#[test]
fn overlay_over_the_frame_budget_is_refused_without_output() {
    let messages = layered_generations(2);
    let needed = overlay_archive(&messages, 64)
        .expect("renders")
        .frames
        .len();
    assert!(
        overlay_archive(&messages, needed - 1).is_err(),
        "a short frame budget must refuse, not truncate"
    );
}

#[test]
fn render_view_produces_one_page_and_refuses_overflow_without_truncation() {
    let cancel = AtomicBool::new(false);
    let config = SnapcompactConfig::default();
    let (png, width, height) = super::render_view("short synthetic line 0123", &config, &cancel)
        .expect("single page renders");
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    assert!(width > 0 && height > 0);
    let huge = "x".repeat(2_000_000);
    assert!(
        super::render_view(&huge, &config, &cancel).is_err(),
        "overflow must be refused, never silently truncated"
    );
}

#[test]
fn render_view_observes_cancellation_before_output() {
    let cancel = AtomicBool::new(true);
    let config = SnapcompactConfig::default();
    assert!(super::render_view("text", &config, &cancel).is_err());
}

fn decode_png_gray_ink(bytes: &[u8]) -> (u32, u32, f64) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .expect("frame decodes as PNG");
    let mut buffer = vec![0; reader.output_buffer_size().expect("bounded PNG dimensions")];
    let info = reader.next_frame(&mut buffer).expect("frame pixels decode");
    let channels = info.color_type.samples();
    let pixels = &buffer[..info.buffer_size()];
    let total = (info.width * info.height) as usize;
    let ink = pixels
        .chunks(channels)
        .filter(|px| px[..channels.min(3)].iter().any(|&c| c < 128))
        .count();
    (info.width, info.height, ink as f64 / total as f64)
}

#[test]
fn overlay_frames_decode_to_shared_coordinates_with_real_ink() {
    let messages = layered_generations(2);
    let layers = crate::storage::snapcompact::layers(&messages).expect("layers parse");
    let first = layers[0].frames[0]["image_url"]["url"]
        .as_str()
        .expect("data url")
        .strip_prefix("data:image/png;base64,")
        .expect("png data url");
    let second = layers[1].frames[0]["image_url"]["url"]
        .as_str()
        .expect("data url")
        .strip_prefix("data:image/png;base64,")
        .expect("png data url");
    let first = decode_png_gray_ink(&STANDARD.decode(first).expect("base64"));
    let second = decode_png_gray_ink(&STANDARD.decode(second).expect("base64"));
    assert_eq!(
        (first.0, first.1),
        (second.0, second.1),
        "generations share frame coordinates"
    );
    for (label, ink) in [("first", first.2), ("second", second.2)] {
        assert!(
            ink > 0.0 && ink < 1.0,
            "{label} frame must contain visible but not blank ink: {ink}"
        );
    }
}
