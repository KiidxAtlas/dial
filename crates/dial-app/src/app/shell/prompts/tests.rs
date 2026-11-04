use super::*;
use crate::app::shell::testkit::shell;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn accept_all_drafts_skips_pending_and_future_reviews_only_for_this_shell() {
    let (_home, _project, app, mut shell) = shell();
    let cancel = AtomicBool::new(false);
    let approval = shell.pty_approval;
    let config = toml::to_string(&app.config).unwrap();
    assert!(shell.review_turn_draft(&cancel, |_| Some(1)));
    for _ in 0..4 {
        assert!(shell.review_turn_draft(&cancel, |_| panic!("draft review must be skipped")));
    }
    assert_eq!(shell.pty_approval, approval);
    assert_eq!(toml::to_string(&app.config).unwrap(), config);

    let mut fresh = Shell::with_terminal(
        &app,
        String::new(),
        crate::ui::term::with_writer(Box::new(std::io::sink()), (100, 30)),
    )
    .unwrap();
    let mut asked = false;
    assert!(!fresh.review_turn_draft(&cancel, |_| {
        asked = true;
        None
    }));
    assert!(asked, "a new shell must ask again");
}

#[test]
fn single_accept_and_cancellation_do_not_enable_accept_all() {
    let (_home, _project, _app, mut shell) = shell();
    let cancel = AtomicBool::new(false);
    assert!(shell.review_turn_draft(&cancel, |_| Some(0)));
    for choice in [Some(2), None, Some(99)] {
        let mut asked = false;
        assert!(!shell.review_turn_draft(&cancel, |_| {
            asked = true;
            choice
        }));
        assert!(asked);
        assert!(!shell.accept_all_turn_drafts);
    }
}

#[test]
fn cancellation_overrides_accept_all_and_does_not_record_racing_consent() {
    let (_home, _project, _app, mut shell) = shell();
    let cancel = AtomicBool::new(true);
    assert!(!shell.review_turn_draft(&cancel, |_| panic!("cancelled before review")));
    cancel.store(false, Ordering::Release);
    assert!(!shell.review_turn_draft(&cancel, |_| {
        cancel.store(true, Ordering::Release);
        Some(1)
    }));
    assert!(!shell.accept_all_turn_drafts);
    cancel.store(false, Ordering::Release);
    assert!(shell.review_turn_draft(&cancel, |_| Some(1)));
    cancel.store(true, Ordering::Release);
    assert!(!shell.review_turn_draft(&cancel, |_| panic!(
        "consent must not override cancellation"
    )));
}

#[test]
fn draft_focus_cannot_submit_or_approve_and_handoff_preserves_the_text() {
    let (_home, _project, _app, mut shell) = shell();
    shell.composer.set_draft("keep");
    assert!(shell.initial_focus() == Focus::Draft);
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert!(shell.draft_step(key(KeyCode::Char('y'))) == DraftStep::Stay);
    assert!(shell.draft_step(key(KeyCode::Enter)) == DraftStep::Stay);
    assert_eq!(shell.composer.draft(), "keepy\n");
    assert!(shell.draft_step(key(KeyCode::Tab)) == DraftStep::Choices);
    assert!(shell.draft_step(key(KeyCode::Esc)) == DraftStep::Choices);
    assert_eq!(shell.composer.draft(), "keepy\n");
}

#[test]
fn attention_inspector_consumes_answers_paste_and_navigation_until_explicit_return() {
    let keys = crate::ui::keys::Keymap::defaults();
    let mut view = AttentionView::default();
    for event in [
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        Event::Paste("yes /approval auto".into()),
        Event::Resize(1, 1),
    ] {
        assert_eq!(view.event(&event, &keys, 100, 10), None);
    }
    assert_eq!(
        view.event(
            &Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            &keys,
            100,
            10
        ),
        None
    );
    assert!(view.offset > 0);
    for code in [KeyCode::F(6), KeyCode::Esc] {
        assert_eq!(
            view.event(
                &Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                &keys,
                100,
                10
            ),
            Some(AttentionExit::Returned)
        );
    }
    assert_eq!(
        view.event(
            &Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &keys,
            100,
            10
        ),
        Some(AttentionExit::Cancelled)
    );
    let mut release = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(view.event(&Event::Key(release), &keys, 100, 10), None);
}

#[test]
fn modal_attention_projects_pending_prompt_without_dispatch_or_draft_changes() {
    let (_home, _project, _app, mut shell) = shell();
    shell.composer.set_draft("keep my draft");
    shell.awaiting_user = true;
    let before = shell.composer.draft();
    let text = shell.modal_attention_text("Allow bash?", "Pending approval for a command");
    assert!(text.contains("Main prompt · waiting for you"));
    assert!(text.contains("Allow bash?"));
    assert!(text.contains("Pending approval for a command"));
    assert!(text.contains("Return to original prompt"));
    assert!(!text.contains("command:/"));
    assert_eq!(shell.composer.draft(), before);
    assert!(shell.awaiting_user);
    assert!(!shell.accept_all_turn_drafts);
    assert!(attention_key(&KeyEvent::new(
        KeyCode::F(6),
        KeyModifiers::NONE
    )));
    assert!(!attention_key(&KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE
    )));
}
