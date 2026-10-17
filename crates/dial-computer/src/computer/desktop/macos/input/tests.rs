use super::*;

fn shift_transition(event: &CGEvent) -> (i64, u32, bool) {
    (
        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
        event.get_type() as u32,
        event.get_flags().contains(CGEventFlags::CGEventFlagShift),
    )
}

const SHIFT_SPACE_TRANSITIONS: [(i64, u32, bool); 4] = [
    (56, CGEventType::FlagsChanged as u32, true),
    (49, CGEventType::KeyDown as u32, true),
    (49, CGEventType::KeyUp as u32, true),
    (56, CGEventType::FlagsChanged as u32, false),
];

#[test]
fn computer_cancelled_bounded_hold_releases_space_and_modifiers() {
    let source = source().expect("event source");
    let cancellation = control::CancellationSource::default();
    let token = cancellation.token();
    let mut events = Vec::new();
    let result = control::with_token_for_test(&token, || {
        with_held_keys(
            &source,
            &[KeyName::Shift, KeyName::Space],
            |event| {
                events.push(shift_transition(event));
                Ok(())
            },
            || {
                cancellation.cancel();
                control::wait(Duration::from_secs(100))
            },
        )
    });
    assert!(result.is_err());
    assert_eq!(events, SHIFT_SPACE_TRANSITIONS);
    assert!(token.check().is_err());
}

#[test]
fn computer_partial_held_key_delivery_still_releases_every_attempted_key() {
    let source = source().expect("event source");
    let mut events = Vec::new();
    let result = with_held_keys(
        &source,
        &[KeyName::Shift, KeyName::Space],
        |event| {
            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            events.push(shift_transition(event));
            if code == 49 && matches!(event.get_type(), CGEventType::KeyDown) {
                Err(DesktopError::input_failed("delivery may be partial"))
            } else {
                Ok(())
            }
        },
        || panic!("failed press must not run the hold"),
    );
    assert!(result.is_err());
    assert_eq!(events, SHIFT_SPACE_TRANSITIONS);
}

#[test]
fn computer_cancelling_text_releases_current_grapheme_and_stops() {
    let source = source().expect("event source");
    let cancellation = control::CancellationSource::default();
    let token = cancellation.token();
    let mut events = Vec::new();
    let result = control::with_token_for_test(&token, || {
        type_text(&source, "e\u{301}later", |event| {
            events.push(event.get_type());
            if matches!(event.get_type(), CGEventType::KeyDown) {
                cancellation.cancel();
            }
            Ok(())
        })
    });
    assert!(result.is_err());
    assert!(matches!(
        events.as_slice(),
        [CGEventType::KeyDown, CGEventType::KeyUp]
    ));
    assert!(cancellation.token().check().is_ok());
    assert!(token.check().is_err());
}

#[test]
fn computer_cancelling_chord_releases_modifier_without_pressing_next_key() {
    let source = source().expect("event source");
    let cancellation = control::CancellationSource::default();
    let token = cancellation.token();
    let mut events = Vec::new();
    let result = control::with_token_for_test(&token, || {
        key_chord(&source, &[KeyName::Ctrl, KeyName::Char('a')], |event| {
            events.push((
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
                event.get_flags().contains(CGEventFlags::CGEventFlagControl),
            ));
            if events.len() == 1 {
                cancellation.cancel();
            }
            Ok(())
        })
    });
    assert!(result.is_err());
    assert_eq!(events, [(59, true), (59, false)]);
}

#[test]
fn computer_event_source_never_suppresses_local_input() {
    let source = source().expect("Quartz event source");
    // SAFETY: `source` remains live for both CoreGraphics getter calls.
    unsafe {
        assert_eq!(get_local_events_suppression_interval(source.as_ptr()), 0.0);
        assert_eq!(
            get_local_events_filter_during_suppression_state(source.as_ptr(), SUPPRESSION_INTERVAL,),
            LOCAL_EVENT_FILTER,
        );
        assert_eq!(
            get_local_events_filter_during_suppression_state(source.as_ptr(), REMOTE_MOUSE_DRAG,),
            LOCAL_EVENT_FILTER,
        );
    }
}

fn record(id: u32, minimized: Option<bool>) -> ax::AxWindowRecord {
    ax::AxWindowRecord { id, minimized }
}

#[test]
fn computer_keyboard_destination_counts_only_windows_that_can_be_key() {
    assert_eq!(keyboard_conflict(10, &[record(10, Some(false))]), None);
    assert_eq!(
        keyboard_conflict(10, &[record(10, Some(false)), record(11, Some(true))]),
        None
    );
    assert_eq!(
        keyboard_conflict(
            10,
            &[
                record(10, Some(false)),
                record(11, None),
                record(12, Some(false))
            ]
        ),
        Some(KeyboardConflict::Siblings(2)),
    );
    assert_eq!(
        keyboard_conflict(10, &[record(11, Some(false))]),
        Some(KeyboardConflict::Unmapped),
    );
}

#[test]
fn computer_interrupted_chord_releases_every_attempted_key() {
    let source = source().expect("Quartz event source");
    let mut events = Vec::new();
    let result = key_chord(&source, &[KeyName::Ctrl, KeyName::Enter], |event| {
        let kind = event.get_type();
        let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        events.push((
            kind as u32,
            code,
            event.get_flags().contains(CGEventFlags::CGEventFlagControl),
        ));
        if matches!(kind, CGEventType::KeyDown) && code == 36 {
            Err(DesktopError::input_failed("focus changed"))
        } else {
            Ok(())
        }
    });
    assert!(result.is_err());
    // Quartz expresses modifier transitions as FlagsChanged; the final
    // cleared flag proves Ctrl is released even when Enter's press fails.
    assert_eq!(
        events,
        vec![
            (CGEventType::FlagsChanged as u32, 59, true),
            (CGEventType::KeyDown as u32, 36, true),
            (CGEventType::KeyUp as u32, 36, true),
            (CGEventType::FlagsChanged as u32, 59, false),
        ]
    );
}

#[test]
fn computer_failed_modifier_press_leaves_no_flag_on_cleanup_releases() {
    let source = source().expect("Quartz event source");
    let mut events = Vec::new();
    let result = key_chord(
        &source,
        &[KeyName::Ctrl, KeyName::Shift, KeyName::Enter],
        |event| {
            let kind = event.get_type();
            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            let flags = event.get_flags();
            let shift = flags.contains(CGEventFlags::CGEventFlagShift);
            events.push((
                kind as u32,
                code,
                flags.contains(CGEventFlags::CGEventFlagControl),
                shift,
            ));
            if code == 56 && shift {
                Err(DesktopError::input_failed("focus changed"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    // Shift's failed press is released first, so no later release still
    // carries its flag.
    assert_eq!(
        events,
        vec![
            (CGEventType::FlagsChanged as u32, 59, true, false),
            (CGEventType::FlagsChanged as u32, 56, true, true),
            (CGEventType::FlagsChanged as u32, 56, true, false),
            (CGEventType::FlagsChanged as u32, 59, false, false),
        ]
    );
}

#[test]
fn computer_takeover_requires_exact_hit_test_ownership() {
    let owner = |pid, window| ax::PointOwner { pid, window };
    assert!(!covers(&owner(7, Some(42)), 7, 42));
    assert!(covers(&owner(7, Some(43)), 7, 42));
    assert!(covers(&owner(7, None), 7, 42));
    assert!(covers(&owner(8, Some(42)), 7, 42));
    assert!(covers(&owner(8, None), 7, 42));
}

#[test]
fn computer_scroll_primer_starts_inside_the_target_window() {
    let window = DesktopWindow {
        id: "42".to_string(),
        title: String::new(),
        app: String::new(),
        pid: Some(7),
        x: 100,
        y: 50,
        width: 300,
        height: 200,
        focused: false,
    };
    let scroll = |x| PointerEvent::Scroll {
        x,
        y: 80.0,
        dx: 0.0,
        dy: -30.0,
    };
    for x in [100.0, 101.0, 249.0, 250.0, 398.0, 399.0] {
        let [Some((start, _)), Some((end, _))] = event_points(&window, &scroll(x)) else {
            panic!("a scroll at {x} must hit-test its primer start and its point");
        };
        assert_eq!(end, x);
        assert!(
            (100.0..400.0).contains(&start),
            "primer for {x} starts outside at {start}"
        );
        assert_eq!((start - x).abs(), PRIMER_OFFSETS[0]);
    }
}

#[test]
fn computer_wheel_steps_split_into_bounded_events_that_sum_to_the_request() {
    assert_eq!(wheel_steps(0, -300), vec![(0, -30); 10]);
    assert_eq!(wheel_steps(0, 30), vec![(0, 30)]);
    assert_eq!(wheel_steps(0, 0), Vec::<(i32, i32)>::new());
    // (dx, dy, event count, largest per-axis step allowed)
    for (dx, dy, count, largest) in [
        (0, -31, 2, 30),
        (7, 95, 4, 30),
        (-301, 44, 11, 30),
        (1, -1, 1, 30),
        (0, 1200, 40, 30),
        (0, -1201, 40, 31),
        (-100_003, 2, 40, 2501),
        (i32::MIN, i32::MAX, 40, 53_687_092),
    ] {
        let steps = wheel_steps(dx, dy);
        assert_eq!(steps.len(), count, "({dx}, {dy})");
        assert!(
            steps
                .iter()
                .all(|&(x, y)| x.unsigned_abs() <= largest && y.unsigned_abs() <= largest),
            "({dx}, {dy}) exceeded {largest} px per event: {steps:?}"
        );
        assert_eq!(steps.iter().map(|step| step.0).sum::<i32>(), dx);
        assert_eq!(steps.iter().map(|step| step.1).sum::<i32>(), dy);
    }
}

#[test]
fn computer_graphemes_join_marks_zwj_and_regional_pairs() {
    assert_eq!(
        graphemes_str("e\u{301}later"),
        vec!["e\u{301}", "l", "a", "t", "e", "r"]
    );
    assert_eq!(
        graphemes_str("\u{1F1FA}\u{1F1F8}\u{1F1EB}\u{1F1F7}").len(),
        2
    );
    assert_eq!(graphemes_str("a\u{1F468}\u{200D}\u{1F469}b").len(), 3);
    assert!(graphemes_str("").is_empty());
}
