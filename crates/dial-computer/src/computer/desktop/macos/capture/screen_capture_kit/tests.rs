use super::{frame_bytes, pixel_dimensions, unpremultiply};

#[test]
fn computer_rgba_preserves_opacity_and_unpremultiplies_transparent_edges() {
    let mut pixels = [12, 34, 56, 255, 64, 32, 0, 128, 9, 8, 7, 0, 255, 0, 0, 1];
    unpremultiply(&mut pixels);
    assert_eq!(
        pixels,
        [12, 34, 56, 255, 128, 64, 0, 128, 0, 0, 0, 0, 255, 0, 0, 1]
    );
}

#[test]
fn computer_hidpi_dimensions_round_outward_and_reject_invalid_or_oversized_frames() {
    assert_eq!(pixel_dimensions(500.25, 336.0, 2.0).unwrap(), (1001, 672));
    for (width, height, scale) in [
        (0.0, 1.0, 1.0),
        (1.0, 1.0, f64::NAN),
        (1.0, 1.0, 0.0),
        (1e9, 1e9, 2.0),
    ] {
        assert!(pixel_dimensions(width, height, scale).is_err());
    }
    assert!(frame_bytes(0, 10).is_err());
    assert!(frame_bytes(u32::MAX, u32::MAX).is_err());
}
