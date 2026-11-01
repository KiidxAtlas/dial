//! Colored-overlay regression tests: exact 50% ink compositing, tool-result omission from visual
//! layers with exact recovery, generation parent headers, flag-off preservation and v1 migration.
//! Filter: `cargo test agent::compact::snapcompact::overlay`.
use super::*;
use crate::agent::compact::snapcompact::archive_with_options;
use crate::storage::snapcompact::{self as stored, Layer};
use serde_json::json;
use std::sync::atomic::AtomicBool;

const BLACK: [u8; 3] = [0, 0, 0];
const WHITE: [u8; 3] = [255, 255, 255];
const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
/// Distinctive, lowercase token that must reach software recovery but never the visual layer.
const TOKEN: &str = "zqorphanvalue4417";
/// Prefix of the marker the visual layer writes in place of omitted tool results.
const OMITTED: &str = "[Tool result omitted from this visual layer by tool_results=false";
const TOKEN_TEXT: &str = "zqorphanvalue4417 raw tool output";

/// A single-page mask frame: black pixels are full ink, white pixels are transparent.
fn mask_frame(pixels: &[[u8; 3]], width: u32, height: u32) -> Value {
    let bytes: Vec<u8> = pixels.iter().flatten().copied().collect();
    image_part(
        &encode_rgb(&bytes, width, height).unwrap(),
        &SnapcompactConfig::default(),
    )
}

fn ink_layer(color: [u8; 3], frame: Value) -> Layer {
    Layer {
        id: "overlay-test".into(),
        color,
        source_text: String::new(),
        messages: vec![],
        frames: vec![frame],
    }
}

/// Composites page 0 and returns the decoded RGB canvas.
fn composite_rgb(layers: &[Layer]) -> (Vec<u8>, u32, u32) {
    let cancel = AtomicBool::new(false);
    let (png, _, _) = composite(layers, 0, &cancel).unwrap();
    decode_rgb(&image_part(&png, &SnapcompactConfig::default()), &cancel).unwrap()
}

fn config(tool_results: bool) -> SnapcompactConfig {
    SnapcompactConfig {
        overlay: true,
        tool_results,
        ..SnapcompactConfig::default()
    }
}

fn archive_of(messages: &[Value], config: &SnapcompactConfig) -> Archive {
    let cancel = AtomicBool::new(false);
    archive_with_options(messages, config, 16, &cancel).unwrap()
}

/// The metadata layer list carried by an archive message.
fn layers_of(archive: &Archive) -> Vec<Value> {
    archive.messages[0]["dial_snapcompact"]["layers"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn one_pixel_ink_halves_coverage_over_the_previous_canvas() {
    // Canvas starts white. Full black mask at 50% opacity: red gives [255,128,128].
    let red = ink_layer(RED, mask_frame(&[BLACK], 1, 1));
    assert_eq!(composite_rgb(&[red]), (vec![255, 128, 128], 1, 1));

    // Blue over that canvas: G (128*255+255)/510 = 64, B (128*255+255*255+255)/510 = 192.
    let red = ink_layer(RED, mask_frame(&[BLACK], 1, 1));
    let blue = ink_layer(BLUE, mask_frame(&[BLACK], 1, 1));
    assert_eq!(composite_rgb(&[red, blue]), (vec![128, 64, 192], 1, 1));
}

#[test]
fn white_mask_is_transparent_and_layers_share_coordinates() {
    // Two 2x1 layers with no spatial offset. Pixel 0: red ink then a white (transparent) mask.
    // Pixel 1: white (transparent) then blue ink over the untouched white canvas.
    let red = ink_layer(RED, mask_frame(&[BLACK, WHITE], 2, 1));
    let blue = ink_layer(BLUE, mask_frame(&[WHITE, BLACK], 2, 1));
    let (rgb, width, height) = composite_rgb(&[red, blue]);
    assert_eq!((width, height), (2, 1));
    assert_eq!(
        &rgb[0..3],
        &[255, 128, 128],
        "white mask must not alter prior ink"
    );
    assert_eq!(
        &rgb[3..6],
        &[128, 128, 255],
        "blue ink must land on the same pixel"
    );
}

#[test]
fn tool_results_are_hidden_from_the_visual_layer_but_recoverable_exactly() {
    let cases = [
        (
            "tool role",
            json!({"role": "tool", "tool_call_id": "call-1", "content": TOKEN_TEXT}),
        ),
        (
            "toolResult role",
            json!({"role": "toolResult", "tool_call_id": "call-1", "content": TOKEN_TEXT}),
        ),
        (
            "nested anthropic tool_result",
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call-1", "content": TOKEN_TEXT}
            ]}),
        ),
    ];
    for (label, message) in cases {
        let out = archive_of(std::slice::from_ref(&message), &config(false));
        let layers = layers_of(&out);
        let last = layers.last().expect("generation layer");
        let source = last["source_text"].as_str().unwrap();
        assert!(
            !source.contains(TOKEN),
            "{label}: visual source leaked the result"
        );
        assert!(
            source.contains(OMITTED),
            "{label}: omission marker missing: {source}"
        );
        assert_eq!(
            last["messages"][0], message,
            "{label}: exact original not retained"
        );

        let view = stored::view(&out.messages, &json!({"view": "layers", "query": TOKEN})).unwrap();
        assert!(!view.is_error, "{label}: {}", view.text);
        assert!(
            view.text.contains(TOKEN),
            "{label}: software view lost the record"
        );
    }
}

#[test]
fn tool_results_remain_visible_when_the_flag_is_enabled() {
    // Control case: the omission above is caused by the flag, not by the message shape.
    let message = json!({"role": "tool", "tool_call_id": "call-1", "content": TOKEN_TEXT});
    let out = archive_of(std::slice::from_ref(&message), &config(true));
    let source = layers_of(&out).last().unwrap()["source_text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        source.contains(TOKEN),
        "flag on must keep the result visible"
    );
    assert!(!source.contains(OMITTED));
}

#[test]
fn identical_fresh_message_in_consecutive_generations_gets_distinct_ids() {
    let message = json!({"role": "user", "content": "zqgenerationprobe"});
    let first = archive_of(std::slice::from_ref(&message), &config(true));
    let first_layers = layers_of(&first);
    let first_id = first_layers[0]["id"].as_str().unwrap().to_owned();

    let second = archive_of(&[first.messages[0].clone(), message], &config(true));
    let second_layers = layers_of(&second);
    assert_eq!(second_layers.len(), 2);
    let second_id = second_layers[1]["id"].as_str().unwrap();
    assert_ne!(
        first_id, second_id,
        "same messages must not collide across generations"
    );
    assert_eq!(second_layers[0]["id"].as_str().unwrap(), first_id);
    let header = second_layers[1]["source_text"].as_str().unwrap();
    assert!(
        header.contains(&format!("parent={first_id}")),
        "generation header must name its parent: {header}"
    );
}

#[test]
fn prior_layers_are_unchanged_when_the_overlay_flag_is_off() {
    let message = json!({"role": "user", "content": "first generation"});
    let first = archive_of(std::slice::from_ref(&message), &config(true));
    let prior = layers_of(&first)[0].clone();

    let flag_off = SnapcompactConfig {
        overlay: false,
        ..SnapcompactConfig::default()
    };
    let fresh = json!({"role": "user", "content": "second generation"});
    let second = archive_of(&[first.messages[0].clone(), fresh], &flag_off);
    let layers = layers_of(&second);
    assert_eq!(layers.len(), 2);
    assert_eq!(
        layers[0], prior,
        "prior layer must be preserved byte-for-byte as JSON"
    );
}

#[test]
fn v1_archive_source_is_migrated_into_a_layer_without_mutation() {
    let legacy = json!({
        "role": "user",
        "content": [],
        "dial_snapcompact": {
            "version": 1,
            "source_text": "LEGACY SOURCE TEXT",
            "source_image_indices": []
        }
    });
    let fresh = json!({"role": "user", "content": "after legacy"});
    let out = archive_of(&[legacy, fresh], &config(true));
    let layers = layers_of(&out);
    assert_eq!(
        layers.len(),
        2,
        "migrated v1 generation plus the new generation"
    );
    let migrated = &layers[0];
    assert_eq!(
        migrated["source_text"],
        json!("Imported archive generation 0\nLEGACY SOURCE TEXT"),
        "v1 source must be kept exactly, not re-read as colored ink"
    );
    assert_eq!(migrated["messages"], json!([]));
    assert_eq!(
        migrated["id"].as_str().unwrap(),
        stored::layer_id(&[], migrated["source_text"].as_str().unwrap()).unwrap()
    );
}
