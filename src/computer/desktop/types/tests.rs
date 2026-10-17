use super::*;

fn display(id: &str, x: i32, primary: bool) -> DesktopDisplay {
    DesktopDisplay {
        id: id.to_string(),
        name: id.to_string(),
        x,
        y: 0,
        width: 100,
        height: 100,
        scale: 1.0,
        pixel_x: 0,
        pixel_y: 0,
        pixel_width: 100,
        pixel_height: 100,
        is_primary: primary,
    }
}

#[test]
fn computer_display_keys_never_alias_windows_or_the_root_desktop() {
    assert!(matches!(
        Target::parse("display:active"),
        Target::Display(_)
    ));
    assert_eq!(Target::parse("display:ACTIVE").key(), "display:active");
    assert_eq!(Target::parse("display:all").key(), "display:all");
    assert_eq!(Target::parse("display:screen-1").key(), "display:screen-1");
    assert_eq!(Target::parse("screen-1").key(), "screen-1");
    assert!(matches!(Target::parse("screen-1"), Target::Window(_)));
    assert_ne!(Target::parse("display:active"), Target::parse("desktop"));
}

#[test]
fn computer_active_default_selects_largest_focused_window_intersection_with_primary_fallback() {
    assert!(matches!(
        DisplaySelector::parse(None),
        DisplaySelector::Active
    ));
    assert!(matches!(
        DisplaySelector::parse(Some("active".into())),
        DisplaySelector::Active
    ));
    let displays = vec![display("left", -100, true), display("right", 0, false)];
    let window = DesktopWindow {
        id: "window".into(),
        title: String::new(),
        app: String::new(),
        pid: None,
        x: -10,
        y: 10,
        width: 70,
        height: 50,
        focused: true,
    };
    let selected = DisplaySelector::Active
        .select(displays.clone(), Some(&window))
        .unwrap();
    assert_eq!(selected[0].id, "right");
    let selected = DisplaySelector::Active
        .select(displays.clone(), None)
        .unwrap();
    assert_eq!(selected[0].id, "left");
    let offscreen = DesktopWindow { x: 1000, ..window };
    assert_eq!(
        DisplaySelector::Active
            .select(displays.clone(), Some(&offscreen))
            .unwrap()[0]
            .id,
        "left"
    );
    assert_eq!(
        DisplaySelector::parse(Some("all".into()))
            .select(displays.clone(), None)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        DisplaySelector::parse(Some("right".into()))
            .select(displays.clone(), None)
            .unwrap()[0]
            .id,
        "right"
    );
    assert!(
        DisplaySelector::Id("missing".into())
            .select(displays, None)
            .is_err()
    );
}
