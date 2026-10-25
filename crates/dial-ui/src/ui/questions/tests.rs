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

fn request(count: usize, header: Option<&str>) -> QuestionRequest {
    QuestionRequest {
        question_id: "q".into(),
        generation: 1,
        session_id: "s".into(),
        owner_task_id: None,
        questions: (0..count)
            .map(|index| Question {
                id: format!("q{index}"),
                question: format!("Pick {index}?"),
                header: header.map(str::to_owned),
                options: vec![QuestionOption {
                    label: "yes".into(),
                    description: None,
                    preview: None,
                }],
                multi: false,
                recommended: None,
            })
            .collect(),
        timeout_seconds: 0,
        notify: false,
    }
}

#[test]
fn a_full_answer_stays_editable_and_oversized_pastes_are_refused() {
    let single = request(1, None);
    let mut dialog = Dialog::new(&single, Path::new("/tmp"));
    let keys = Keymap::defaults();
    dialog.start_text(Field::Custom);
    dialog.paste(&"a".repeat(questions::MAX_TEXT));
    let Mode::Text(_, editor) = &dialog.mode else {
        panic!("text mode")
    };
    assert_eq!(editor.text.len(), questions::MAX_TEXT);
    // Growth is refused at the limit; deletion still works.
    dialog.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &keys);
    dialog.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &keys);
    let Mode::Text(_, editor) = &dialog.mode else {
        panic!("text mode")
    };
    assert_eq!(editor.text.len(), questions::MAX_TEXT - 1);
    dialog.paste("too long");
    assert!(dialog.feedback.contains("too long"));

    // The attachment path field uses its own, smaller limit for paste too.
    dialog.key(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &keys,
    );
    dialog.paste(&"p".repeat(MAX_PATH + 1));
    let Mode::Attach(_, _, path) = &dialog.mode else {
        panic!("attach mode")
    };
    assert!(path.text.is_empty());
}

#[test]
fn a_single_question_has_no_counter_and_hints_only_working_keys() {
    let single = request(1, None);
    let rows = Dialog::new(&single, Path::new("/tmp")).rows(80, 30);
    assert_eq!(rows[0], "Question");
    assert!(!rows.last().unwrap().contains("← back"), "{rows:?}");
    let titled = request(1, Some("Scope"));
    assert_eq!(
        Dialog::new(&titled, Path::new("/tmp")).rows(80, 30)[0],
        "Scope"
    );

    let multi = request(2, Some("Scope"));
    let keys = Keymap::defaults();
    let mut dialog = Dialog::new(&multi, Path::new("/tmp"));
    let rows = dialog.rows(80, 30);
    assert_eq!(rows[0], "Question 1/2 · Scope");
    assert!(rows.iter().any(|row| row.contains("→ next")), "{rows:?}");
    assert!(!rows.iter().any(|row| row.contains("← back")), "{rows:?}");
    dialog.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys);
    dialog.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &keys);
    let rows = dialog.rows(80, 30);
    assert_eq!(rows[0], "Question 2/2 · Scope");
    assert!(
        rows.iter()
            .any(|row| row.contains("← back") && row.contains("→ done")),
        "{rows:?}"
    );
}
