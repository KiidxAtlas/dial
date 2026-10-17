//! Regression tests for lossless escaping of unrenderable glyphs and for honest frame capacity.
//! Filter: `cargo test agent::compact::snapcompact::tests`.
use crate::config::snapcompact::SnapcompactConfig;

use super::*;
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
