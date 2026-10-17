use super::*;

const A: u8 = 38;
const ONE: u8 = 10;
const RETURN: u8 = 36;
const SHIFT: u8 = 50;
const CTRL: u8 = 37;

/// US-like two-level map: a/A, 1/!, Return, `Shift_L`, `Control_L`.
fn keymap() -> Keymap {
    let mut keysyms = vec![0u32; (255 - 8 + 1) * 2];
    let mut set = |keycode: u8, syms: [u32; 2]| {
        let index = usize::from(keycode - 8) * 2;
        keysyms[index..index + 2].copy_from_slice(&syms);
    };
    set(A, [0x61, 0x41]);
    set(ONE, [0x31, 0x21]);
    set(RETURN, [0xff0d, 0xff0d]);
    set(SHIFT, [0xffe1, 0xffe1]);
    set(CTRL, [0xffe3, 0xffe3]);
    Keymap::new(8, 2, keysyms, vec![(SHIFT, 1), (CTRL, 4)])
}

fn press(keycode: u8) -> KeyStep {
    KeyStep {
        keycode,
        press: true,
    }
}

fn release(keycode: u8) -> KeyStep {
    KeyStep {
        keycode,
        press: false,
    }
}

#[test]
fn computer_cancellation_before_first_release_never_waits_out_the_hold_in_cleanup() {
    let source = control::CancellationSource::default();
    let token = source.token();
    let mut events = Vec::new();
    let result = control::with_token_for_test(&token, || {
        run_steps_with_hold(
            &[press(CTRL), press(A), release(A), release(CTRL)],
            std::time::Duration::from_secs(100),
            |step| {
                events.push(step);
                if step == press(A) {
                    source.cancel();
                }
                Ok(())
            },
        )
    });
    assert!(result.is_err());
    assert_eq!(
        events,
        vec![press(CTRL), press(A), release(A), release(CTRL)]
    );
}

#[test]
fn computer_cancelled_hold_releases_all_pressed_keys_in_reverse() {
    let source = control::CancellationSource::default();
    let token = source.token();
    let mut events = Vec::new();
    let result = control::with_token_for_test(&token, || {
        with_held_codes(
            &[CTRL, A],
            |step| {
                events.push(step);
                Ok(())
            },
            || {
                source.cancel();
                control::wait(std::time::Duration::from_secs(100))
            },
        )
    });
    assert!(result.is_err());
    assert_eq!(
        events,
        vec![press(CTRL), press(A), release(A), release(CTRL)]
    );
    assert!(token.check().is_err());
}

#[test]
fn computer_partially_delivered_press_and_failed_release_do_not_strand_earlier_keys() {
    let mut events = Vec::new();
    let result = with_held_codes(
        &[CTRL, A, ONE],
        |step| {
            events.push(step);
            if step.keycode == A {
                Err(DesktopError::input_failed("partial transport failure"))
            } else {
                Ok(())
            }
        },
        || panic!("failed press cannot execute the hold body"),
    );
    assert!(
        result
            .unwrap_err()
            .message
            .contains("key release also failed")
    );
    assert_eq!(
        events,
        vec![press(CTRL), press(A), release(A), release(CTRL)]
    );
}

#[test]
fn computer_successful_hold_releases_before_returning() {
    let mut events = Vec::new();
    with_held_codes(
        &[CTRL, A],
        |step| {
            events.push(step);
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        events,
        vec![press(CTRL), press(A), release(A), release(CTRL)]
    );
}

#[test]
fn computer_text_wraps_shifted_glyphs_in_shift() {
    let steps = keymap().plan_text("aA!\n").unwrap();
    assert_eq!(
        steps,
        vec![
            press(A),
            release(A),
            press(SHIFT),
            press(A),
            release(A),
            release(SHIFT),
            press(SHIFT),
            press(ONE),
            release(ONE),
            release(SHIFT),
            press(RETURN),
            release(RETURN),
        ]
    );
}

#[test]
fn computer_chord_adds_shift_once_unless_requested() {
    let map = keymap();
    assert_eq!(
        map.plan_chord(&[KeyName::Ctrl, KeyName::Char('A')])
            .unwrap(),
        vec![
            press(CTRL),
            press(SHIFT),
            press(A),
            release(A),
            release(SHIFT),
            release(CTRL),
        ]
    );
    assert_eq!(
        map.plan_chord(&[KeyName::Ctrl, KeyName::Shift, KeyName::Char('A')])
            .unwrap(),
        vec![
            press(CTRL),
            press(SHIFT),
            press(A),
            release(A),
            release(SHIFT),
            release(CTRL)
        ]
    );
}

#[test]
fn computer_unshifted_level_wins_over_an_earlier_shifted_match() {
    let mut map = keymap();
    let index = usize::from(ONE - 8) * 2;
    map.keysyms[index + 1] = 0x61;
    assert_eq!(map.lookup(0x61), Some((A, false)));
}

#[test]
fn computer_partially_failed_press_is_released_before_earlier_modifiers() {
    let mut emitted = Vec::new();
    let result = run_steps(&[press(CTRL), press(A)], |step| {
        emitted.push(step);
        if step == press(A) {
            Err(DesktopError::input_failed("failed after enqueue"))
        } else {
            Ok(())
        }
    });
    assert!(result.is_err());
    assert_eq!(
        emitted,
        vec![press(CTRL), press(A), release(A), release(CTRL)]
    );
}

#[test]
fn computer_cancellation_releases_held_keys_and_does_not_poison_later_input() {
    let source = control::CancellationSource::default();
    let token = source.token();
    let mut emitted = Vec::new();
    let result = control::with_token_for_test(&token, || {
        run_steps(
            &[press(CTRL), press(A), release(A), release(CTRL)],
            |step| {
                control::check()?;
                emitted.push(step);
                if step == press(A) {
                    source.cancel();
                }
                Ok(())
            },
        )
    });
    assert!(result.is_err());
    assert_eq!(
        emitted,
        vec![press(CTRL), press(A), release(A), release(CTRL)]
    );
    let later = control::CancellationSource::default().token();
    control::with_token_for_test(&later, || {
        run_steps(&[press(A), release(A)], |step| {
            control::check()?;
            emitted.push(step);
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(&emitted[4..], &[press(A), release(A)]);
}

#[test]
fn computer_already_cancelled_delivery_does_not_release_unpressed_keys() {
    let source = control::CancellationSource::default();
    let token = source.token();
    source.cancel();
    let mut emitted = Vec::new();
    let result = control::with_token_for_test(&token, || {
        run_steps(&[press(CTRL)], |step| {
            emitted.push(step);
            Ok(())
        })
    });
    assert!(result.is_err());
    assert!(emitted.is_empty());
}

#[test]
fn computer_text_with_an_unmapped_glyph_is_refused_whole() {
    assert!(keymap().plan_text("a€").is_err());
}
