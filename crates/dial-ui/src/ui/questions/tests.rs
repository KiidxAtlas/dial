use super::*;
use crate::agent::workflow::questions::{Question, QuestionOption};
use crossterm::event::KeyEventKind;

fn release(code: KeyCode) -> KeyEvent {
    let mut event = KeyEvent::new(code, KeyModifiers::NONE);
    event.kind = KeyEventKind::Release;
    event
}

#[test]
fn key_releases_are_no_ops_in_the_question_dialog() {
    let request = QuestionRequest {
        question_id: "q".into(),
        generation: 1,
        session_id: "s".into(),
        owner_task_id: None,
        questions: vec![Question {
            id: "a".into(),
            question: "Pick one?".into(),
            header: None,
            options: vec![QuestionOption {
                label: "yes".into(),
                description: None,
                preview: None,
            }],
            multi: false,
            recommended: None,
        }],
        timeout_seconds: 60,
        notify: false,
    };
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    let keys = Keymap::defaults();
    assert!(dialog.key(release(KeyCode::Enter), &keys).is_none());
    assert!(dialog.key(release(KeyCode::Down), &keys).is_none());
    assert_eq!(dialog.cursor, 0, "a release must not move the cursor");
    assert!(matches!(dialog.mode, Mode::Choices));
    assert!(dialog.answers[0].selected_options.is_empty());
}

#[test]
fn the_framed_dialog_fits_its_width_and_marks_the_choice() {
    let rows = vec![
        "Question 1/1".to_owned(),
        "Pick one?".to_owned(),
        "> [ ] yes".to_owned(),
        "  [x] no".to_owned(),
        "↑↓ choose · Enter select · Esc cancel".to_owned(),
    ];
    let theme = crate::ui::theme::Theme::dark();
    for width in [8usize, 20, 60, 120] {
        let lines = framed(rows.clone(), width, 40, &theme);
        for line in &lines {
            assert_eq!(text::line_width(line), width, "width {width}");
        }
    }
    let lines = framed(rows, 60, 40, &theme);
    let plain: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(plain[0].starts_with(" Question 1/1"), "{plain:?}");
    assert!(plain.iter().all(|l| !l.contains('│') && !l.contains('╭')));
    assert!(
        plain.iter().any(|l| l.contains("\u{203a} \u{25cb} yes")),
        "{plain:?}"
    );
    assert!(plain.iter().any(|l| l.contains("\u{25c9} no")), "{plain:?}");
    assert_eq!(
        plain.last().unwrap().trim(),
        "",
        "the panel ends in a row of fill"
    );
    assert_eq!(
        lines[0].spans[0].style.bg,
        theme.bg(crate::ui::theme::Tok::PanelBg).bg
    );
}

#[test]
fn short_question_frames_keep_the_current_choice_visible_and_respect_zero_size() {
    let theme = crate::ui::theme::Theme::dark();
    let mut rows = vec!["Question 1/1".into(), "Choose one".into()];
    for i in 0..20 {
        rows.push(format!(
            "{} [ ] option-{i}",
            if i == 18 { ">" } else { " " }
        ));
    }
    rows.push("Enter select · Esc cancel".into());
    for width in [0, 1, 7, 8, 24, 80] {
        for height in 0..=30 {
            assert!(framed(Vec::new(), width, height, &theme).is_empty());
            let lines = framed(rows.clone(), width, height, &theme);
            assert!(lines.len() <= height, "{width}x{height}");
            assert!(lines.iter().all(|row| text::line_width(row) <= width));
            if width >= 24 && height >= 3 {
                assert!(
                    lines
                        .iter()
                        .any(|line| line.to_string().contains("option-18")),
                    "{width}x{height}: {lines:?}"
                );
            }
        }
    }
}
