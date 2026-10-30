use super::*;
use crate::app::shell::testkit::shell;

#[test]
fn finished_speech_submits_a_normal_turn_or_queues_after_active_work() {
    for busy in [false, true] {
        let (_home, _project, app, mut shell) = shell();
        shell.composer.set_busy(busy);
        // Even a one-word conversational reply is a turn.
        shell.voice_transcript(&app, "Yes");
        assert!(shell.composer.draft().is_empty());
        if busy {
            assert!(shell.deferred.is_empty());
            assert_eq!(shell.composer.queued_messages(), vec![("Yes".into(), true)]);
            shell.composer.set_busy(false);
            assert_eq!(shell.composer.take_follow_up().as_deref(), Some("Yes"));
        } else {
            assert_eq!(shell.deferred, ["Yes"]);
        }
        assert_eq!(shell.composer.history(), ["Yes"]);
    }
}

#[test]
fn manual_review_preserves_existing_drafts_and_command_like_speech() {
    for (draft, speech, trigger) in [
        ("", "hello", SubmitTrigger::Never),
        ("typed draft: ", "hello", SubmitTrigger::Release),
        ("", "/quit", SubmitTrigger::Release),
        ("", "!rm something", SubmitTrigger::Release),
        ("", "not finished", SubmitTrigger::ReleaseComplete),
        ("", "do not resubmit", SubmitTrigger::SaySubmit),
    ] {
        let (_home, _project, mut app, mut shell) = shell();
        app.config.voice.stt.submit_trigger = trigger;
        shell.composer.set_draft(draft);
        shell.voice_transcript(&app, speech);
        assert_eq!(shell.composer.expanded_draft(), format!("{draft}{speech}"));
        assert!(shell.deferred.is_empty());
        assert!(!shell.composer.has_queued_work());
    }
}

#[test]
fn empty_or_control_prefixed_speech_cannot_send_old_queue_items_or_commands() {
    for (speech, trigger, expected) in [
        ("\u{1b}[31m/quit", SubmitTrigger::Release, "/quit"),
        (
            "\u{1b}[0m!echo unsafe",
            SubmitTrigger::Release,
            "!echo unsafe",
        ),
        ("\u{1b}[0m", SubmitTrigger::Release, ""),
        ("submit", SubmitTrigger::SaySubmit, ""),
    ] {
        let (_home, _project, mut app, mut shell) = shell();
        shell.composer.set_busy(true);
        shell.composer.handle_paste("keep queued work");
        shell.composer.submit_voice();
        shell.composer.set_busy(false);
        app.config.voice.stt.submit_trigger = trigger;
        shell.voice_transcript(&app, speech);
        assert_eq!(shell.composer.draft(), expected);
        assert!(shell.deferred.is_empty());
        assert_eq!(
            shell.composer.queued_messages(),
            vec![("keep queued work".into(), true)]
        );
    }
}

#[test]
fn submission_rules_handle_sentence_endings_and_spoken_submit_word() {
    for (text, trigger, expected, send) in [
        (" Hello? ", SubmitTrigger::ReleaseComplete, "Hello?", true),
        ("Hello", SubmitTrigger::ReleaseComplete, "Hello", false),
        (
            "Please continue SUBMIT.",
            SubmitTrigger::SaySubmit,
            "Please continue",
            true,
        ),
        (
            "こんにちは\u{3000}submit",
            SubmitTrigger::SaySubmit,
            "こんにちは",
            true,
        ),
        ("resubmit", SubmitTrigger::SaySubmit, "resubmit", false),
        ("submit", SubmitTrigger::SaySubmit, "", true),
        ("", SubmitTrigger::Release, "", true),
    ] {
        assert_eq!(submission(text, trigger), (expected, send), "{text:?}");
    }
}

#[test]
fn default_tap_does_not_stop_on_release_even_with_release_capable_terminal() {
    let config = crate::config::voice::SttConfig::default();
    let mut gesture = crate::voice::gesture::Gesture::default();
    assert_eq!(config.input_mode, InputMode::Toggle);
    assert_eq!(config.submit_trigger, SubmitTrigger::Release);
    assert_eq!(
        gesture.press(config.input_mode == InputMode::Hold),
        crate::voice::gesture::Command::Start { hold: false }
    );
    assert_eq!(gesture.release(), None);
    assert!(gesture.recording());
    assert_eq!(gesture.press(false), crate::voice::gesture::Command::Stop);
}
