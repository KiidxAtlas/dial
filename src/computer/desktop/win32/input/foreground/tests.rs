use super::*;

#[test]
fn cancellation_stops_packets_but_allows_cleanup_and_a_later_operation() {
    let source = control::CancellationSource::default();
    let token = source.token();
    source.cancel();
    control::with_token_for_test(&token, || {
        let cancelled = token.check().unwrap_err().message;
        assert_eq!(send(&[]).unwrap_err().message, cancelled);
        assert_eq!(
            send_to(std::ptr::null_mut(), &[]).unwrap_err().message,
            cancelled
        );
        control::cleanup(|| assert!(send(&[]).is_ok()));
        assert_eq!(send(&[]).unwrap_err().message, cancelled);
    });
    control::with_token_for_test(&source.token(), || assert!(send(&[]).is_ok()));
}

#[test]
fn text_packets_keep_surrogates_and_return_transitions_together() {
    let mut storage = [unicode_event(0, false); 4];
    let emoji = text_events(TextUnit::Char('\u{1f600}'), &mut storage);
    assert_eq!(emoji.len(), 4);
    let pending = pending_releases(&emoji[..3]);
    assert_eq!(pending.len(), 1);
    assert!(same_release(&pending[0], &unicode_event(0xde00, true)));
    assert!(pending_releases(emoji).is_empty());

    let enter = text_events(TextUnit::Enter, &mut storage);
    assert_eq!(enter.len(), 2);
    let pending = pending_releases(&enter[..1]);
    assert_eq!(pending.len(), 1);
    assert!(same_release(&pending[0], &key_event(VK_RETURN, true)));
    assert!(pending_releases(enter).is_empty());
}

#[test]
fn short_click_releases_only_an_inserted_button_down() {
    let click = [
        mouse_event(MOUSEEVENTF_MOVE, 0),
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0),
        mouse_event(MOUSEEVENTF_LEFTUP, 0),
    ];
    assert!(pending_releases(&click[..1]).is_empty());
    let pending = pending_releases(&click[..2]);
    assert_eq!(pending.len(), 1);
    assert!(same_release(
        &pending[0],
        &mouse_event(MOUSEEVENTF_LEFTUP, 0)
    ));
    assert!(pending_releases(&click).is_empty());
}

#[test]
fn short_unicode_batch_does_not_replay_completed_characters() {
    let text = [
        unicode_event(0x41, false),
        unicode_event(0x41, true),
        unicode_event(0xd83d, false),
        unicode_event(0xd83d, true),
        unicode_event(0xde00, false),
        unicode_event(0xde00, true),
    ];
    let pending = pending_releases(&text[..5]);
    assert_eq!(pending.len(), 1);
    assert!(same_release(&pending[0], &unicode_event(0xde00, true)));
    assert!(pending_releases(&text).is_empty());
}

#[test]
fn short_chord_tracks_repeats_and_completed_releases() {
    let ctrl_down = keyboard_input(0x11, 0x1d, 0);
    let ctrl_up = keyboard_input(0x11, 0x1d, KEYEVENTF_KEYUP);
    let letter_down = keyboard_input(0x41, 0x1e, 0);
    let letter_up = keyboard_input(0x41, 0x1e, KEYEVENTF_KEYUP);
    let chord = [ctrl_down, letter_down, letter_down, letter_up, ctrl_up];
    let pending = pending_releases(&chord[..3]);
    assert_eq!(pending.len(), 2);
    assert!(same_release(&pending[1], &letter_up));
    assert!(same_release(&pending[0], &ctrl_up));
    let pending = pending_releases(&chord[..4]);
    assert_eq!(pending.len(), 1);
    assert!(same_release(&pending[0], &ctrl_up));
    assert!(pending_releases(&chord).is_empty());
}
