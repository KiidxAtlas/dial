use super::*;

fn translated(event: PointerEvent) -> PointerEvent {
    event.map_points(|x, y| Ok((x + 100.0, y - 50.0))).unwrap()
}

#[test]
fn pointer_mapping_preserves_gesture_options_and_scroll_deltas() {
    let modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: true,
        meta: false,
    };
    let click = translated(PointerEvent::Click {
        x: 2.0,
        y: 3.0,
        button: MouseButton::Right,
        count: 2,
        modifiers,
    });
    let PointerEvent::Click {
        x,
        y,
        button,
        count,
        modifiers: actual,
    } = click
    else {
        panic!("click must remain a click");
    };
    assert_eq!((x, y), (102.0, -47.0));
    assert_eq!(button, MouseButton::Right);
    assert_eq!(count, 2);
    assert_eq!(actual, modifiers);

    let PointerEvent::Move { x, y } = translated(PointerEvent::Move { x: 4.0, y: 5.0 }) else {
        panic!("move must remain a move");
    };
    assert_eq!((x, y), (104.0, -45.0));

    let drag = translated(PointerEvent::Drag {
        path: vec![(1.0, 2.0), (3.0, 4.0)],
        button: MouseButton::Middle,
        modifiers,
        keys: vec![KeyName::Alt, KeyName::Char('a')],
    });
    let PointerEvent::Drag {
        path,
        button,
        modifiers: actual,
        keys,
    } = drag
    else {
        panic!("drag must remain a drag");
    };
    assert_eq!(path, [(101.0, -48.0), (103.0, -46.0)]);
    assert_eq!(button, MouseButton::Middle);
    assert_eq!(actual, modifiers);
    assert_eq!(keys, [KeyName::Alt, KeyName::Char('a')]);

    let hold = translated(PointerEvent::Hold {
        x: 6.0,
        y: 7.0,
        button: MouseButton::Left,
        keys: vec![KeyName::Shift],
        duration: Duration::from_millis(25),
    });
    let PointerEvent::Hold {
        x,
        y,
        button,
        keys,
        duration,
    } = hold
    else {
        panic!("hold must remain a hold");
    };
    assert_eq!((x, y), (106.0, -43.0));
    assert_eq!(button, MouseButton::Left);
    assert_eq!(keys, [KeyName::Shift]);
    assert_eq!(duration, Duration::from_millis(25));

    let scroll = translated(PointerEvent::Scroll {
        x: 8.0,
        y: 9.0,
        dx: -3.0,
        dy: 4.0,
    });
    let PointerEvent::Scroll { x, y, dx, dy } = scroll else {
        panic!("scroll must remain a scroll");
    };
    assert_eq!((x, y), (108.0, -41.0));
    assert_eq!((dx, dy), (-3.0, 4.0));
}

#[test]
fn pointer_mapping_propagates_coordinate_errors_and_stops_at_invalid_drag_point() {
    let events = [
        PointerEvent::Click {
            x: 1.0,
            y: 2.0,
            button: MouseButton::Left,
            count: 1,
            modifiers: Modifiers::default(),
        },
        PointerEvent::Move { x: 1.0, y: 2.0 },
        PointerEvent::Hold {
            x: 1.0,
            y: 2.0,
            button: MouseButton::Left,
            keys: vec![],
            duration: Duration::ZERO,
        },
        PointerEvent::Scroll {
            x: 1.0,
            y: 2.0,
            dx: 0.0,
            dy: 1.0,
        },
    ];
    for event in events {
        let error = event
            .map_points(|_, _| Err(DesktopError::invalid_coordinate_frame("invalid point")))
            .unwrap_err();
        assert_eq!(
            error.code,
            super::super::error::ErrorCode::InvalidCoordinateFrame
        );
    }
    let drag = PointerEvent::Drag {
        path: vec![(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)],
        button: MouseButton::Left,
        modifiers: Modifiers::default(),
        keys: vec![],
    };
    let mut visited = Vec::new();
    let result = drag.map_points(|x, y| {
        visited.push((x, y));
        if x == 2.0 {
            Err(DesktopError::invalid_coordinate_frame("invalid drag point"))
        } else {
            Ok((x, y))
        }
    });
    assert!(result.is_err());
    assert_eq!(visited, [(1.0, 1.0), (2.0, 2.0)]);
}
