use super::{ColorMasks, Format, zpixmap_to_rgba};

const MASKS: ColorMasks = ColorMasks {
    red: 0xff0000,
    green: 0x00ff00,
    blue: 0x0000ff,
};

#[test]
fn pixmap_format_preserves_byte_order_and_scanline_padding() {
    let format = Format {
        depth: 24,
        bits_per_pixel: 24,
        scanline_pad: 32,
    };
    for (data, lsb_first) in [
        ([3, 2, 1, 0, 6, 5, 4, 0], true),
        ([1, 2, 3, 0, 4, 5, 6, 0], false),
    ] {
        let image = zpixmap_to_rgba(&data, 1, 2, &format, lsb_first, MASKS).unwrap();
        assert_eq!(image.into_raw(), [1, 2, 3, 255, 4, 5, 6, 255]);
    }
}

#[test]
fn pixmap_format_supports_32_bit_pixels_and_rejects_invalid_layouts() {
    let format = Format {
        depth: 24,
        bits_per_pixel: 32,
        scanline_pad: 32,
    };
    let image = zpixmap_to_rgba(&[3, 2, 1, 0], 1, 1, &format, true, MASKS).unwrap();
    assert_eq!(image.into_raw(), [1, 2, 3, 255]);
    assert!(zpixmap_to_rgba(&[3, 2, 1], 1, 1, &format, true, MASKS).is_err());
    for invalid in [
        Format {
            depth: 16,
            ..format
        },
        Format {
            bits_per_pixel: 16,
            ..format
        },
    ] {
        assert!(zpixmap_to_rgba(&[0; 4], 1, 1, &invalid, true, MASKS).is_err());
    }
}
