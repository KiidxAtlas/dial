use image::Rgba;

use super::*;

fn display(scale: f64) -> DesktopDisplay {
    DesktopDisplay {
        id: "1".into(),
        name: "test".into(),
        x: 100,
        y: 50,
        width: 400,
        height: 300,
        scale,
        pixel_x: 0,
        pixel_y: 0,
        pixel_width: (400.0 * scale) as u32,
        pixel_height: (300.0 * scale) as u32,
        is_primary: true,
    }
}
fn window(x: i32, y: i32) -> DesktopWindow {
    DesktopWindow {
        id: "7".into(),
        title: "T".into(),
        app: "A".into(),
        pid: None,
        x,
        y,
        width: 400,
        height: 300,
        focused: false,
    }
}

#[test]
fn computer_pixel_to_logical_at_one_and_two_x() {
    for scale in [1.0, 2.0] {
        let f = FrameGeometry::for_displays(&[display(scale)]);
        assert_eq!(
            f.map_point(200.0 * scale, 100.0 * scale, None).unwrap(),
            (300.0, 150.0)
        );
    }
}

#[test]
fn computer_moved_window_is_reanchored() {
    let f = FrameGeometry::for_window(&window(10, 20), 800, 600);
    assert_eq!(
        f.map_point(400.0, 300.0, Some(&window(110, 220))).unwrap(),
        (310.0, 370.0)
    );
}

#[test]
fn computer_zoom_maps_capped_coordinates_to_fresh_native_pixels_without_mutating_base() {
    let fresh = FrameGeometry::for_window(&window(10, 20), 800, 600);
    let mut base = fresh.clone();
    apply_capture_caps(
        RgbaImage::new(800, 600),
        &mut base,
        &CaptureCaps {
            max_width: Some(400),
            max_height: None,
        },
    )
    .unwrap();
    let original = base.clone();
    let image = RgbaImage::from_fn(800, 600, |x, y| {
        Rgba([(x / 4) as u8, (y / 4) as u8, 7, 255])
    });
    let (crop, source_width, source_height) = base
        .crop_region(
            image,
            &fresh,
            &CaptureRegion {
                x: 50.0,
                y: 30.0,
                width: 100.0,
                height: 75.0,
            },
            &CaptureCaps::default(),
        )
        .unwrap();
    assert_eq!((source_width, source_height), (200, 150));
    assert_eq!(crop.dimensions(), (200, 150));
    assert_eq!(crop.get_pixel(0, 0), &Rgba([25, 15, 7, 255]));
    assert_eq!(crop.get_pixel(199, 149), &Rgba([74, 52, 7, 255]));
    let output = apply_image_caps(
        crop,
        &CaptureCaps {
            max_width: Some(100),
            max_height: None,
        },
    )
    .unwrap();
    assert_eq!(output.dimensions(), (100, 75));
    assert_eq!(base, original);
    assert_eq!(
        base.map_point(200.0, 150.0, Some(&window(10, 20))).unwrap(),
        (210.0, 170.0)
    );
}

#[test]
fn computer_zoom_rejects_non_finite_overflow_empty_and_out_of_frame_rectangles() {
    let frame = FrameGeometry::for_window(&window(10, 20), 800, 600);
    let valid = CaptureRegion {
        x: 0.0,
        y: 0.0,
        width: 800.0,
        height: 600.0,
    };
    assert!(frame.validate_region(&valid).is_ok());
    for region in [
        CaptureRegion {
            x: f64::NAN,
            ..valid
        },
        CaptureRegion {
            y: f64::INFINITY,
            ..valid
        },
        CaptureRegion {
            width: f64::NAN,
            ..valid
        },
        CaptureRegion {
            height: f64::INFINITY,
            ..valid
        },
        CaptureRegion {
            x: f64::MAX,
            width: f64::MAX,
            ..valid
        },
        CaptureRegion {
            width: 0.0,
            ..valid
        },
        CaptureRegion {
            height: -1.0,
            ..valid
        },
        CaptureRegion { x: -1.0, ..valid },
        CaptureRegion { x: 1.0, ..valid },
        CaptureRegion { y: 1.0, ..valid },
    ] {
        assert_eq!(
            frame.validate_region(&region).unwrap_err().code,
            super::super::error::ErrorCode::InvalidCoordinateFrame
        );
    }
}

#[test]
fn computer_resized_window_rejects_pointer_and_zoom_but_moved_window_remains_valid() {
    let original = window(10, 20);
    let base = FrameGeometry::for_window(&original, 800, 600);
    let resized = DesktopWindow {
        width: 500,
        ..original
    };
    assert!(base.map_point(100.0, 100.0, Some(&resized)).is_err());
    let fresh = FrameGeometry::for_window(&resized, 1000, 600);
    assert!(
        base.crop_region(
            RgbaImage::new(1000, 600),
            &fresh,
            &CaptureRegion {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0
            },
            &CaptureCaps::default()
        )
        .is_err()
    );
    assert!(base.validate_window(Some(&window(300, 400))).is_ok());
}

#[test]
fn computer_layout_fingerprint_checks_all_displays_not_active_selection_or_enumeration_order() {
    let first = display(2.0);
    let second = DesktopDisplay {
        id: "2".to_string(),
        x: 500,
        is_primary: false,
        ..display(1.0)
    };
    let mut frame = FrameGeometry::for_displays(std::slice::from_ref(&first));
    frame
        .record_layout(&[first.clone(), second.clone()])
        .unwrap();
    assert!(
        frame
            .validate_layout(&[second.clone(), first.clone()])
            .is_ok()
    );
    assert_eq!(
        frame.display_metadata(&[second.clone(), first.clone()])[0].id,
        first.id
    );
    for changed in [
        DesktopDisplay {
            id: "replacement".to_string(),
            ..second.clone()
        },
        DesktopDisplay {
            x: 600,
            ..second.clone()
        },
        DesktopDisplay {
            width: 500,
            ..second.clone()
        },
        DesktopDisplay {
            scale: 2.0,
            ..second.clone()
        },
        DesktopDisplay {
            pixel_height: 600,
            ..second
        },
    ] {
        assert!(frame.validate_layout(&[first.clone(), changed]).is_err());
    }
    assert!(frame.validate_layout(std::slice::from_ref(&first)).is_err());
}

#[test]
fn computer_cap_scaling_adjusts_geometry() {
    let mut f = FrameGeometry::for_displays(&[display(2.0)]);
    let image = RgbaImage::from_pixel(800, 600, Rgba([0, 0, 0, 255]));
    let image = apply_capture_caps(
        image,
        &mut f,
        &CaptureCaps {
            max_width: Some(400),
            max_height: Some(400),
        },
    )
    .unwrap();
    assert_eq!((image.width(), image.height()), (400, 300));
    assert_eq!(f.map_point(200.0, 100.0, None).unwrap(), (300.0, 150.0));
}

#[test]
fn computer_compose_places_and_clips_tiles() {
    let tile = |w, h, v| RgbaImage::from_pixel(w, h, Rgba([v, v, v, 255]));
    let sole = tile(3, 2, 7);
    let pointer = sole.as_raw().as_ptr();
    let same = compose(3, 2, Rgba([0; 4]), [Ok((sole, 0, 0))]).unwrap();
    assert_eq!(same.as_raw().as_ptr(), pointer);

    let canvas = compose(
        4,
        2,
        Rgba([0, 0, 0, 255]),
        [Ok((tile(2, 2, 1), 0, 0)), Ok((tile(3, 3, 2), 3, 1))],
    )
    .unwrap();
    let mut reference = RgbaImage::from_pixel(4, 2, Rgba([0, 0, 0, 255]));
    image::imageops::replace(&mut reference, &tile(2, 2, 1), 0, 0);
    image::imageops::replace(&mut reference, &tile(3, 3, 2), 3, 1);
    assert_eq!(canvas, reference);
}
