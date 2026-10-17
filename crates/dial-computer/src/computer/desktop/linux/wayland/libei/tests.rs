use super::*;

#[test]
fn drag_motions_and_release_are_spaced_for_event_consumers() {
    use std::time::Instant;

    let started = Instant::now();
    let mut previous = started;
    let mut received = Vec::new();
    pace_drag(&[(450.0, 400.0), (600.0, 411.0), (750.0, 422.0)], |x, y| {
        let now = Instant::now();
        assert!(now.duration_since(previous) >= DRAG_STEP_DELAY);
        previous = now;
        received.push((x, y));
        Ok(())
    })
    .unwrap();
    assert_eq!(received, [(600.0, 411.0), (750.0, 422.0)]);
    assert!(
        previous.elapsed() >= DRAG_STEP_DELAY,
        "release must not overtake the final motion"
    );
}

#[test]
fn cancellation_drops_an_in_flight_request_without_restarting_it() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let source = control::CancellationSource::default();
    let token = source.token();
    let mut starts = 0;
    let result = control::with_token_for_test(&token, || {
        runtime.block_on(cancellable(async {
            starts += 1;
            source.cancel();
            std::future::pending::<()>().await;
        }))
    });
    assert!(result.is_err());
    assert_eq!(starts, 1);
}

#[test]
fn cancellation_stops_drag_before_the_next_motion() {
    let source = control::CancellationSource::default();
    let token = source.token();
    let mut received = Vec::new();
    let result = control::with_token_for_test(&token, || {
        pace_drag(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)], |x, y| {
            received.push((x, y));
            source.cancel();
            Ok(())
        })
    });
    assert!(result.is_err());
    assert_eq!(received, [(1.0, 1.0)]);
    let later = control::CancellationSource::default().token();
    control::with_token_for_test(&later, || {
        pace_drag(&[(0.0, 0.0), (3.0, 3.0)], |x, y| {
            received.push((x, y));
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(received, [(1.0, 1.0), (3.0, 3.0)]);
}

#[test]
fn printable_text_without_a_keymap_never_assumes_us_layout() {
    assert!(char_stroke(None, 'a').is_err());
    assert!(char_stroke(None, '@').is_err());
    assert_eq!(char_stroke(None, '\n').unwrap().keycode, 28);
}

#[test]
fn newline_presses_enter_even_when_the_keymap_has_a_linefeed_key() {
    let mut layout = KeyboardLayout::compile(include_str!("../testdata/fr.xkb"))
        .expect("French fixture must compile");
    assert_eq!(char_stroke(Some(&mut layout), '\n').unwrap().keycode, 28);
}

#[test]
fn scroll_detents_preserve_direction_and_reject_overflow() {
    assert_eq!(discrete_detents(2.0).unwrap(), 240);
    assert_eq!(discrete_detents(-0.5).unwrap(), -60);
    assert!(discrete_detents(f64::INFINITY).is_err());
    assert!(discrete_detents(f64::from(i32::MAX)).is_err());
}

#[test]
fn discovery_waits_for_every_granted_device() {
    let targets = DiscoveryTargets {
        pointer: true,
        keyboard: true,
    };

    assert!(!targets.is_complete(false, true));
    assert!(targets.is_complete(true, true));
}
