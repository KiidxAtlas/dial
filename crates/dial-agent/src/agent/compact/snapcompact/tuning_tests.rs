//! Regression tests for model-aware Snapcompact geometry, image detail and billing.
//! Filter: `cargo test --locked -p dial-agent agent::compact::snapcompact::tuning_tests`.
//! Deterministic synthetic conversations only: no network, model, timing or benchmark assertions.
use super::*;
use crate::config::snapcompact::{
    ImageDetail, ImageTokenization, SHAPE_VARIANTS, SnapcompactFamily,
};
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use std::io::Cursor;
use std::sync::atomic::AtomicBool;

const SENTINEL: &str = "tuning-sentinel-7f3a";
/// Renderer's hard frame-size ceiling (`renderer::MAX_FRAME_SIZE`).
const RENDERER_MAX_FRAME: u32 = 2576;

struct Decoded {
    width: u32,
    height: u32,
    /// Bit depth and colour type of the PNG as stored, before EXPAND.
    depth: png::BitDepth,
    stored_color: png::ColorType,
    /// Samples after `Transformations::EXPAND`, row-major, `samples` per pixel.
    samples: usize,
    pixels: Vec<u8>,
}

/// Decodes with EXPAND so palette and 1-bit frames read back as ordinary pixels.
fn decode(bytes: &[u8]) -> Decoded {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().expect("PNG header decodes");
    let (width, height) = (reader.info().width, reader.info().height);
    let (depth, stored_color) = (reader.info().bit_depth, reader.info().color_type);
    let mut pixels = vec![0; reader.output_buffer_size().expect("bounded PNG buffer")];
    let frame = reader.next_frame(&mut pixels).expect("PNG pixels decode");
    pixels.truncate(frame.buffer_size());
    Decoded {
        width,
        height,
        depth,
        stored_color,
        samples: frame.color_type.samples(),
        pixels,
    }
}

impl Decoded {
    /// Number of distinct RGB(A)/grey pixel values after expansion.
    fn distinct_colors(&self) -> usize {
        self.pixels
            .chunks(self.samples)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// Several hundred short lines: fills multiple rows so height is not clamped to the 64px floor.
fn long_text() -> String {
    (0..400)
        .map(|index| format!("line {index} alpha beta gamma. "))
        .collect()
}

fn shape_config(shape: &str) -> SnapcompactConfig {
    SnapcompactConfig {
        shape: shape.to_owned(),
        ..SnapcompactConfig::default()
    }
}

fn image_parts(archive: &Archive) -> Vec<&Value> {
    archive.messages[0]["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter(|part| part["type"] == "image_url")
        .collect()
}

fn part_png(part: &Value) -> Vec<u8> {
    let url = part["image_url"]["url"].as_str().expect("image url");
    let data = url
        .strip_prefix("data:image/png;base64,")
        .expect("embedded PNG data URL");
    STANDARD.decode(data).expect("valid base64 PNG")
}

/// Every emitted image must decode to the dimensions and detail recorded in its metadata.
fn assert_frames_match_pngs(archive: &Archive) {
    let parts = image_parts(archive);
    assert_eq!(
        parts.len(),
        archive.frames.len(),
        "one image part per frame"
    );
    for (part, frame) in parts.iter().zip(&archive.frames) {
        let decoded = decode(&part_png(part));
        assert_eq!(
            (decoded.width, decoded.height),
            (frame.width, frame.height),
            "metadata must describe the decoded PNG"
        );
        assert_eq!(part["image_url"]["detail"], frame.detail.as_str());
    }
}

#[test]
fn tuned_tracking_and_leading_keep_the_natural_8x13_glyph_pixels() {
    fn pixel(image: &Decoded, x: usize, y: usize) -> &[u8] {
        let offset = (y * image.width as usize + x) * image.samples;
        &image.pixels[offset..offset + image.samples]
    }
    let cancel = AtomicBool::new(false);
    let baseline = SnapcompactConfig {
        frame_size: Some(64),
        ..shape_config("8x13-bw")
    };
    let baseline = shape(&baseline).unwrap();
    let reference = decode(&renderer::render("A", &baseline, &cancel).unwrap().png);
    for (name, advance, pitch) in [("11on16-bw", 11, 16), ("8on22-bw", 8, 22)] {
        let config = SnapcompactConfig {
            frame_size: Some(64),
            ..shape_config(name)
        };
        let shape = shape(&config).unwrap();
        let text = "A".repeat(64 / advance + 1);
        let image = decode(&renderer::render(&text, &shape, &cancel).unwrap().png);
        assert_eq!(image.depth, png::BitDepth::One, "{name} is not stretched");
        for y in 0..13 {
            for x in 0..8 {
                let expected = pixel(&reference, x, y);
                assert_eq!(pixel(&image, x, y), expected, "{name}: first glyph");
                assert_eq!(pixel(&image, x + advance, y), expected, "{name}: tracking");
                assert_eq!(pixel(&image, x, y + pitch), expected, "{name}: leading");
            }
        }
        for y in 0..pitch {
            for x in 8..advance {
                assert!(
                    pixel(&image, x, y).iter().all(|&c| c == 255),
                    "{name}: tracking gap"
                );
            }
        }
        for y in 13..pitch {
            for x in 0..8 {
                assert!(
                    pixel(&image, x, y).iter().all(|&c| c == 255),
                    "{name}: leading gap"
                );
            }
        }
    }
}

#[test]
fn natural_8x13_is_one_bit_black_and_white() {
    let archive = archive_with_options(
        &[user(&long_text())],
        &shape_config("8x13-bw"),
        64,
        &AtomicBool::new(false),
    )
    .expect("8x13-bw archives");
    let decoded = decode(&part_png(image_parts(&archive)[0]));
    assert_eq!(decoded.depth, png::BitDepth::One, "bw frames stay 1-bit");
    assert_eq!(decoded.stored_color, png::ColorType::Indexed);
    assert_eq!(
        decoded.distinct_colors(),
        2,
        "black ink and white paper, no blended greys"
    );
}

#[test]
fn pagination_boundary_is_exact_at_declared_frame_sizes() {
    // (frame size, columns, rows) for 8x13 glyphs: floor(size / 8) and floor(size / 13).
    for (size, cols, rows) in [(1932u32, 241usize, 148usize), (2048, 256, 157)] {
        let config = SnapcompactConfig {
            frame_size: Some(size),
            ..shape_config("8x13-bw")
        };
        let shape = shape(&config).expect("shape resolves");
        let cancel = AtomicBool::new(false);
        let fill = "a".repeat(cols * rows);
        let one = paginate(&fill, &shape, 1, &cancel).expect("exact page fits");
        assert_eq!(one.len(), 1, "{size}: exactly one full page");

        let overflow = format!("{fill}a");
        assert!(
            paginate(&overflow, &shape, 1, &cancel).is_err(),
            "{size}: one more character must fail closed instead of truncating"
        );
        let two = paginate(&overflow, &shape, 2, &cancel).expect("second frame allowed");
        assert_eq!(two.len(), 2, "{size}: overflow starts a second page");
        assert_eq!(two.concat(), overflow, "{size}: no character is dropped");
    }
}

#[test]
fn oversize_frames_never_exceed_renderer_limit() {
    let at_limit = SnapcompactConfig {
        frame_size: Some(RENDERER_MAX_FRAME),
        ..shape_config("8x13-bw")
    };
    let archive = archive_with_options(
        &[user(&long_text())],
        &at_limit,
        64,
        &AtomicBool::new(false),
    )
    .expect("frame size at the renderer limit archives");
    assert_frames_match_pngs(&archive);
    assert!(
        archive
            .frames
            .iter()
            .all(|frame| frame.width == RENDERER_MAX_FRAME)
    );

    for size in [RENDERER_MAX_FRAME + 1, 4096] {
        let config = SnapcompactConfig {
            frame_size: Some(size),
            ..shape_config("8x13-bw")
        };
        // Rejection is allowed; a returned frame must never exceed the host limit.
        if let Ok(archive) =
            archive_with_options(&[user(&long_text())], &config, 64, &AtomicBool::new(false))
        {
            assert!(
                archive
                    .frames
                    .iter()
                    .all(|frame| frame.width <= RENDERER_MAX_FRAME),
                "{size}: frame wider than renderer limit"
            );
        }
    }
}

#[test]
fn frame_size_drives_decoded_width_and_metadata() {
    let config = SnapcompactConfig {
        frame_size: Some(1932),
        ..shape_config("8x13-bw")
    };
    let archive = archive_with_options(&[user(&long_text())], &config, 64, &AtomicBool::new(false))
        .expect("1932px archives");
    assert_frames_match_pngs(&archive);
    assert!(archive.frames.iter().all(|frame| frame.width == 1932));
}

#[test]
fn detail_matches_metadata_and_every_emitted_image_part() {
    for detail in [ImageDetail::High, ImageDetail::Original] {
        let config = SnapcompactConfig {
            image_detail: detail,
            ..shape_config("8x13-bw")
        };
        let archive =
            archive_with_options(&[user(&long_text())], &config, 64, &AtomicBool::new(false))
                .expect("archives");
        assert!(!archive.frames.is_empty());
        for frame in &archive.frames {
            assert_eq!(frame.detail, detail.as_str(), "{detail:?} metadata");
        }
        assert_frames_match_pngs(&archive);
    }
}

#[test]
fn billing_uses_actual_decoded_dimensions_conservatively() {
    let cancel = AtomicBool::new(false);
    let patches = ImageTokenization::Patches {
        patch_size: 14,
        tokens_per_patch: 1,
        base_tokens: 0,
    };
    let cases = [(ImageTokenization::PixelCeiling, false), (patches, true)];
    for (billing, patch_model) in cases {
        let config = SnapcompactConfig {
            frame_size: Some(1932),
            ..shape_config("8x13-bw")
        };
        let archive = archive_with_billing(&[user(&long_text())], &config, 64, &cancel, &billing)
            .expect("archives");
        for frame in &archive.frames {
            let (w, h) = (u64::from(frame.width), u64::from(frame.height));
            let expected = if patch_model {
                w.div_ceil(14) * h.div_ceil(14)
            } else {
                w * h
            };
            assert_eq!(frame.token_upper_bound, expected, "{billing:?} bound");
        }
        assert_frames_match_pngs(&archive);
    }
}

#[test]
fn source_text_is_retained_exactly_and_counted() {
    let text = format!("{SENTINEL} verbatim source");
    let archive = archive_with_options(
        &[user(&text)],
        &shape_config("8x13-bw"),
        64,
        &AtomicBool::new(false),
    )
    .expect("archives");
    let meta = &archive.messages[0]["dial_snapcompact"];
    let source = meta["source_text"].as_str().expect("source text");
    assert!(source.contains(SENTINEL), "source keeps the exact input");
    assert_eq!(archive.source_bytes, source.len());
}

#[test]
fn overlay_and_clean_view_respect_frame_size() {
    let config = SnapcompactConfig {
        frame_size: Some(1932),
        overlay: true,
        ..shape_config("8x13-bw")
    };
    let cancel = AtomicBool::new(false);
    let archive = archive_with_options(&[user(&long_text())], &config, 64, &cancel)
        .expect("overlay archives");
    assert_frames_match_pngs(&archive);
    assert!(archive.frames.iter().all(|frame| frame.width == 1932));

    let (png, width, height) = render_view(&long_text(), &config, &cancel).expect("view renders");
    let decoded = decode(&png);
    assert_eq!((decoded.width, decoded.height), (width, height));
    assert_eq!(width, 1932, "clean view uses the declared frame size");
}

#[test]
fn every_explicit_shape_variant_archives_with_decoded_dimensions() {
    let cancel = AtomicBool::new(false);
    for shape in SHAPE_VARIANTS.iter().copied().filter(|s| *s != "auto") {
        let archive = archive_with_options(&[user(SENTINEL)], &shape_config(shape), 64, &cancel)
            .unwrap_or_else(|error| panic!("{shape} archives: {error:#}"));
        assert_frames_match_pngs(&archive);
    }
}

#[test]
fn resolved_routes_are_explicit_renderable_shapes() {
    let cancel = AtomicBool::new(false);
    let routes = [
        ("claude-3-5-sonnet-20241022", SnapcompactFamily::Anthropic),
        ("claude-opus-4-7", SnapcompactFamily::Anthropic),
        ("gemini-3.5-flash", SnapcompactFamily::Google),
        ("gpt-5.5", SnapcompactFamily::OpenAi),
        ("unlisted-model", SnapcompactFamily::Unknown),
    ];
    for (model, family) in routes {
        let config = SnapcompactConfig::default();
        let resolved = config.resolved_for_model(model, family);
        assert_ne!(resolved.shape, "auto", "{model} must resolve auto");
        assert!(
            SHAPE_VARIANTS.contains(&resolved.shape.as_str()),
            "{model} resolved to unsupported {}",
            resolved.shape
        );
        let archive = archive_with_options(&[user(SENTINEL)], &resolved, 64, &cancel)
            .unwrap_or_else(|error| panic!("{model} archives: {error:#}"));
        assert_frames_match_pngs(&archive);
    }
}
