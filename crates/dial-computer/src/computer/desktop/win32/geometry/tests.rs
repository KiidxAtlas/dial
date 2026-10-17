use super::*;
use crate::computer::desktop::frame::FrameGeometry;

fn display(x: i32, y: i32, width: u32, height: u32, scale: f64) -> DesktopDisplay {
    DesktopDisplay {
        id: format!("{x},{y}"),
        name: String::new(),
        x,
        y,
        width,
        height,
        scale,
        pixel_x: 0,
        pixel_y: 0,
        pixel_width: 0,
        pixel_height: 0,
        is_primary: x == 0 && y == 0,
    }
}

#[test]
fn adjacent_mixed_dpi_monitors_do_not_overlap_or_rescale() {
    let mut displays = [
        display(0, 0, 1920, 1080, 1.0),
        display(1920, 0, 3840, 2160, 2.0),
    ];
    let layout = PhysicalLayout::new(displays.iter()).unwrap();
    for display in &mut displays {
        layout.place(display);
    }
    assert_eq!((layout.width, layout.height), (5760, 2160));
    assert_eq!(
        displays[0].pixel_x + displays[0].pixel_width,
        displays[1].pixel_x
    );
    let frame = FrameGeometry::for_displays(&displays);
    assert_eq!(
        frame.map_point(1919.0, 500.0, None).unwrap(),
        (1919.0, 500.0)
    );
    assert_eq!(
        frame.map_point(1920.0, 500.0, None).unwrap(),
        (1920.0, 500.0)
    );
    assert_eq!(
        frame.map_point(5000.0, 1800.0, None).unwrap(),
        (5000.0, 1800.0)
    );
}

#[test]
fn negative_monitor_origins_round_trip_from_normalized_capture_pixels() {
    let mut displays = [
        display(-2560, -200, 2560, 1440, 2.0),
        display(0, 0, 1920, 1080, 1.0),
    ];
    let layout = PhysicalLayout::new(displays.iter()).unwrap();
    for display in &mut displays {
        layout.place(display);
    }
    assert_eq!((layout.width, layout.height), (4480, 1440));
    let frame = FrameGeometry::for_displays(&displays);
    assert_eq!(
        frame.map_point(100.0, 100.0, None).unwrap(),
        (-2460.0, -100.0)
    );
    assert_eq!(
        frame.map_point(2660.0, 400.0, None).unwrap(),
        (100.0, 200.0)
    );
    assert!(frame.map_point(3000.0, 100.0, None).is_err());
}
