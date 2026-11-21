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
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
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
        rows.iter().any(|row| row.contains("← back"))
            && rows.iter().any(|row| row.contains("→ done")),
        "{rows:?}"
    );
}

#[test]
fn custom_enter_submits_and_preserves_only_compatible_selections() {
    let keys = Keymap::defaults();
    for multi in [false, true] {
        let mut request = request(1, None);
        request.questions[0].multi = multi;
        let mut dialog = Dialog::new(&request, Path::new("/tmp"));
        // Select an option first, then choose Other through the choices list.
        dialog.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys);
        if multi {
            dialog.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &keys);
        } else {
            for _ in 0..3 {
                dialog.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &keys);
            }
        }
        dialog.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys);
        dialog.paste("custom");
        assert!(
            dialog
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &keys)
                .is_none()
        );
        dialog.paste("answer");
        let Some(QuestionOutcome::Answered(answers)) =
            dialog.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys)
        else {
            panic!("custom Enter must submit, multi={multi}");
        };
        assert_eq!(answers[0].custom_input.as_deref(), Some("custom\nanswer"));
        let expected = if multi { vec!["yes"] } else { Vec::new() };
        assert_eq!(answers[0].selected_options, expected, "multi={multi}");
    }
}

#[test]
fn custom_enter_advances_questions_and_submits_the_last_answer() {
    let request = request(2, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    let keys = Keymap::defaults();
    dialog.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &keys);
    dialog.paste("first");
    assert!(
        dialog
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys)
            .is_none()
    );
    assert_eq!(dialog.page, 1);
    assert!(matches!(dialog.mode, Mode::Choices));
    dialog.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &keys);
    dialog.paste("second");
    let Some(QuestionOutcome::Answered(answers)) =
        dialog.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys)
    else {
        panic!("the last custom answer must submit all answers");
    };
    assert_eq!(answers[0].custom_input.as_deref(), Some("first"));
    assert_eq!(answers[1].custom_input.as_deref(), Some("second"));
}

#[test]
fn note_enter_saves_without_advancing_or_submitting() {
    let request = request(2, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    let keys = Keymap::defaults();
    dialog.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &keys);
    dialog.paste("context");
    assert!(
        dialog
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keys)
            .is_none()
    );
    assert_eq!(dialog.page, 0);
    assert!(matches!(dialog.mode, Mode::Choices));
    assert_eq!(dialog.answers[0].note.as_deref(), Some("context"));
}

#[test]
fn inspection_pauses_and_resumes_exact_remaining_question_time_without_sleeping() {
    let request = request(1, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    let start = Instant::now();
    let remaining = Duration::new(52, 123_456_789);
    let paused = start + Duration::from_secs(7);
    dialog.deadline = Some(paused + remaining);
    let before = serde_json::to_value(&dialog.answers).unwrap();
    dialog.pause_timeout(paused);
    dialog.pause_timeout(paused + Duration::from_secs(1)); // idempotent, not reset
    assert_eq!(dialog.paused_timeout, Some(remaining));
    assert!(
        dialog
            .expired_at(start + Duration::from_secs(10_000))
            .is_none()
    );
    assert_eq!(serde_json::to_value(&dialog.answers).unwrap(), before);
    let returned = start + Duration::from_secs(10_000);
    dialog.resume_timeout(returned).unwrap();
    assert_eq!(dialog.deadline, Some(returned + remaining));
    dialog
        .resume_timeout(returned + Duration::from_secs(1))
        .unwrap();
    assert_eq!(dialog.deadline, Some(returned + remaining));
    assert!(
        dialog
            .expired_at(returned + remaining - Duration::from_nanos(1))
            .is_none()
    );
    assert!(dialog.expired_at(returned + remaining).is_some());
}

#[test]
fn inspecting_untimed_questions_keeps_them_untimed_and_preserves_custom_input() {
    let mut request = request(1, None);
    request.timeout_seconds = 0;
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    dialog.presented().unwrap();
    let keys = Keymap::defaults();
    dialog.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &keys);
    dialog.paste("unfinished answer");
    let before = dialog.rows(100, 30);
    let now = Instant::now();
    dialog.pause_timeout(now);
    dialog
        .resume_timeout(now + Duration::from_secs(10_000))
        .unwrap();
    assert!(dialog.deadline.is_none());
    assert!(
        dialog
            .expired_at(now + Duration::from_secs(20_000))
            .is_none()
    );
    assert_eq!(dialog.rows(100, 30), before);
}

#[test]
fn esc_cancels_from_the_choices_and_text_fields_keep_ctrl_a_as_line_start() {
    let single = request(1, None);
    let keys = Keymap::defaults();
    let mut dialog = Dialog::new(&single, Path::new("/tmp"));
    assert!(
        dialog.rows(80, 30).last().unwrap().ends_with("Esc cancel"),
        "the advertised way out is last"
    );
    assert!(matches!(
        dialog.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &keys),
        Some(QuestionOutcome::Cancelled)
    ));

    let mut dialog = Dialog::new(&single, Path::new("/tmp"));
    dialog.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &keys);
    for c in "ab".chars() {
        dialog.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &keys);
    }
    dialog.key(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &keys,
    );
    dialog.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &keys);
    let Mode::Text(_, editor) = &dialog.mode else {
        panic!("Ctrl+A stays in the text field")
    };
    assert_eq!(editor.text, "xab", "Ctrl+A moved to the line start");
    // Esc in a text field only leaves the field.
    assert!(
        dialog
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &keys)
            .is_none()
    );
    assert!(matches!(dialog.mode, Mode::Choices));
}

#[test]
fn choice_navigation_and_hints_follow_the_keymap() {
    let multi = request(2, None);
    let mut keys = Keymap::defaults();
    keys.rebind(Action::SelectDown, &["F5"]).unwrap();
    keys.rebind(Action::SelectConfirm, &["F6"]).unwrap();
    let mut dialog = Dialog::new(&multi, Path::new("/tmp")).with_keys(&keys);
    let footer = dialog.rows(80, 30).join("\n");
    assert!(
        footer.contains("↑/F5 choose") && footer.contains("F6 select"),
        "{footer}"
    );
    let start = dialog.cursor;
    dialog.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &keys);
    assert_eq!(dialog.cursor, start, "the unbound default does nothing");
    dialog.key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), &keys);
    assert_eq!(dialog.cursor, start + 1);
}

#[test]
fn ascii_dialog_hints_use_key_names_without_changing_question_text() {
    use crate::ui::theme::{Glyphs, Theme};
    let theme = Theme::dark().with_glyphs(Glyphs::Ascii);
    let mut multi = request(2, None);
    multi.questions[0].question = "Keep → in the prompt?".into();
    let mut keys = Keymap::defaults();
    keys.rebind(Action::SelectDown, &["F5"]).unwrap();
    let dialog = Dialog::new(&multi, Path::new("/tmp")).with_keys(&keys);
    let lines = framed_with_roles(
        dialog.rows_with_roles(80, 30, theme.glyphs()),
        80,
        30,
        &theme,
    );
    let view = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        view.contains("Up/F5 choose") && view.contains("Right next"),
        "{view}"
    );
    assert!(view.contains("Keep → in the prompt?"), "{view}");
}

#[test]
fn a_timeout_that_picks_an_answer_says_what_it_chose() {
    let request = request(2, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    let now = Instant::now();
    dialog.deadline = Some(now);
    assert!(dialog.expired_at(now).is_none(), "the first page moves on");
    assert_eq!(dialog.page, 1);
    assert_eq!(
        dialog.feedback,
        "No answer in time to \u{201c}Pick 0?\u{201d}; chose \u{201c}yes\u{201d}."
    );
    assert_eq!(dialog.timeout_notes(), [dialog.feedback.clone()]);
    dialog.deadline = Some(now);
    let Some(QuestionOutcome::Answered(answers)) = dialog.expired_at(now) else {
        panic!("the last page answers");
    };
    assert!(answers.iter().all(|answer| answer.timed_out));
    assert_eq!(dialog.timeout_notes().len(), 2);
}

#[test]
fn a_timeout_after_the_user_chose_reports_nothing_extra() {
    let request = request(1, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    dialog.answers[0].selected_options.push("yes".into());
    let now = Instant::now();
    dialog.deadline = Some(now);
    assert!(dialog.expired_at(now).is_some());
    assert!(dialog.timeout_notes().is_empty());
}

fn rendered(dialog: &Dialog<'_>, width: usize, height: usize) -> String {
    framed(
        dialog.rows(width, height),
        width,
        height,
        &crate::ui::theme::Theme::dark(),
    )
    .iter()
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn small_question_keeps_selection_and_confirm_cancel_controls() {
    let request = request(1, None);
    let mut keys = Keymap::defaults();
    keys.rebind(Action::SelectConfirm, &["F6"]).unwrap();
    keys.rebind(Action::SelectCancel, &["F7"]).unwrap();
    let dialog = Dialog::new(&request, Path::new("/tmp")).with_keys(&keys);
    for (width, height) in [(80, 6), (32, 6), (24, 7)] {
        let view = rendered(&dialog, width, height);
        assert!(view.contains("yes"), "{width}x{height}: {view}");
        assert!(view.contains("F6 select"), "{width}x{height}: {view}");
        assert!(view.contains("F7 cancel"), "{width}x{height}: {view}");
    }
}

#[test]
fn question_and_selected_details_are_reachable_without_changing_answers() {
    let mut request = request(1, None);
    request.questions[0].question = (0..15).map(|i| format!("question-line-{i}\n")).collect();
    request.questions[0].options[0].description = Some("description-end".into());
    request.questions[0].options[0].preview = Some("preview-end".into());
    let mut keys = Keymap::defaults();
    keys.rebind(Action::SelectPageDown, &["F8"]).unwrap();
    keys.rebind(Action::SelectPageUp, &["F9"]).unwrap();
    for width in [24, 60] {
        let mut dialog = Dialog::new(&request, Path::new("/tmp")).with_keys(&keys);
        let mut seen = String::new();
        for _ in 0..40 {
            seen.push_str(&rendered(&dialog, width, 6));
            dialog.key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE), &keys);
        }
        for expected in [
            "question-line-0",
            "question-line-7",
            "question-line-14",
            "description-end",
            "preview-end",
        ] {
            assert!(seen.contains(expected), "missing {expected}: {seen}");
        }
        assert!(dialog.answers[0].selected_options.is_empty());
        // Scrolling at the bottom must not accumulate an invisible overscroll debt.
        let end = rendered(&dialog, width, 6);
        dialog.key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), &keys);
        assert_ne!(rendered(&dialog, width, 6), end);
        for _ in 0..40 {
            dialog.key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), &keys);
        }
        assert!(rendered(&dialog, width, 6).contains("question-line-0"));
        dialog.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &keys);
        assert!(rendered(&dialog, width, 6).contains("Other"));
        assert!(dialog.answers[0].selected_options.is_empty());
    }
}

#[test]
fn small_text_and_attachment_views_keep_active_input_feedback_and_back() {
    let request = request(1, None);
    let keys = Keymap::defaults();
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    dialog.start_text(Field::Custom);
    dialog.paste(&format!("{}cursor-tail", "long answer ".repeat(40)));
    dialog.feedback = "Input too long".into();
    for width in [32, 80] {
        let view = rendered(&dialog, width, 8);
        assert!(view.contains("cursor-tail│"), "{view}");
        assert!(view.contains("Input too long"), "{view}");
        assert!(view.contains("Esc back"), "{view}");
        assert!(view.contains("Enter submit"), "{view}");
    }
    dialog.key(
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
        &keys,
    );
    dialog.paste(&format!("{}image.png", "long-path/".repeat(30)));
    dialog.feedback = "File not found".into();
    let view = rendered(&dialog, 40, 8);
    assert!(view.contains("image.png│"), "{view}");
    assert!(view.contains("File not found"), "{view}");
    assert!(view.contains("Enter attach"), "{view}");
    assert!(view.contains("Esc back"), "{view}");
}

#[test]
fn question_modes_obey_viewport_bounds_at_degenerate_sizes() {
    let mut request = request(1, None);
    request.questions[0].question = "Long question 界 ".repeat(40);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    let keys = Keymap::defaults();
    let theme = crate::ui::theme::Theme::dark();
    for mode in 0..3 {
        if mode == 1 {
            dialog.start_text(Field::Note);
            dialog.paste(&"input 界 │\n".repeat(30));
        } else if mode == 2 {
            dialog.key(
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &keys,
            );
            dialog.paste(&"long/path/".repeat(40));
        }
        dialog.feedback = "Validation feedback".into();
        for width in [0, 1, 7, 8, 24, 32, 80] {
            for height in 0..=12 {
                let rows = dialog.rows(width, height);
                assert!(
                    rows.len() <= height,
                    "mode={mode}, {width}x{height}: {rows:?}"
                );
                let lines = framed(rows, width, height, &theme);
                assert!(lines.len() <= height);
                assert!(lines.iter().all(|line| text::line_width(line) <= width));
            }
        }
    }
}

#[test]
fn question_layout_regression_choices_and_footer_stay_put_across_options() {
    let mut request = request(1, None);
    request.questions[0].options[0].preview = Some("Long preview line\n".repeat(40));
    request.questions[0].options.push(QuestionOption {
        label: "Short option".into(),
        description: None,
        preview: None,
    });
    for (width, height) in [(32, 12), (80, 30)] {
        let mut dialog = Dialog::new(&request, Path::new("/tmp"));
        let mut positions = Vec::new();
        for cursor in 0..request.questions[0].options.len() + 4 {
            dialog.cursor = cursor;
            let rows = dialog.rows(width, height);
            let first_choice = rows
                .iter()
                .position(|row| row.starts_with("> ") || row.starts_with("  "))
                .unwrap();
            positions.push((rows.len(), first_choice));
        }
        assert!(
            positions.windows(2).all(|pair| pair[0] == pair[1]),
            "{width}x{height}: {positions:?}"
        );
    }
}

#[test]
fn question_layout_regression_custom_input_is_bounded_and_not_muted() {
    let request = request(1, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    dialog.start_text(Field::Custom);
    dialog.paste("My own answer");
    let theme = crate::ui::theme::Theme::dark();
    let lines = framed(dialog.rows(60, 14), 60, 14, &theme);
    let view = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(view.contains("Your answer"), "{view}");
    assert!(view.contains("┌") && view.contains("└"), "{view}");
    let input = lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.content.contains("My own answer"))
        .unwrap();
    assert_eq!(input.style.fg, theme.fg(crate::ui::theme::Tok::Text).fg);
}

#[test]
fn input_boxes_keep_whitespace_and_support_ascii_borders() {
    let request = request(1, None);
    let mut dialog = Dialog::new(&request, Path::new("/tmp"));
    dialog.start_text(Field::Note);
    dialog.paste("  indented\nsecond line");
    let theme = crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let lines = framed(dialog.rows(40, 14), 40, 14, &theme);
    let view = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(view.contains("+- Your note"), "{view}");
    assert!(view.contains("|   indented"), "{view}");
    assert!(!view.contains('┌') && !view.contains('└'), "{view}");
    assert!(lines.iter().all(|line| text::line_width(line) == 40));
}

#[test]
fn scroll_hint_accounts_for_space_reserved_by_choices() {
    let mut request = request(1, None);
    request.questions[0].options[0].preview = Some("detail\n".repeat(10));
    let dialog = Dialog::new(&request, Path::new("/tmp"));
    let view = rendered(&dialog, 70, 18);
    assert!(view.contains("scroll details"), "{view}");
}

#[test]
fn footer_roles_are_styled_from_the_role_and_follow_the_glyph_preset() {
    use crate::ui::theme::{Glyphs, Theme, Tok};
    let hints = |items: &[&str]| Row::Hints(items.iter().map(|s| (*s).to_owned()).collect());
    let cases = [
        // (theme, separator): the ASCII preset swaps only the separator glyph.
        (Theme::dark(), " · "),
        (Theme::dark().with_glyphs(Glyphs::Ascii), " | "),
    ];
    for (theme, separator) in cases {
        let rows = vec![
            Row::Text("Question 1/1".into()),
            Row::Text("Pick one?".into()),
            hints(&["Enter select", "Esc cancel"]),
        ];
        let lines = framed_with_roles(rows, 60, 20, &theme);
        let footer = lines
            .iter()
            .find(|line| line.to_string().contains("Enter select"))
            .expect("the hint footer is drawn");
        let text = footer.to_string();
        assert!(
            text.contains(&format!("Enter select{separator}Esc cancel")),
            "{text}"
        );
        assert!(footer.spans.iter().any(|span| {
            span.content.contains("Enter select") && span.style.fg == theme.fg(Tok::Muted).fg
        }));
        for line in &lines {
            assert_eq!(text::line_width(line), 60);
        }
    }
}

#[test]
fn a_single_hint_footer_wraps_narrow_and_ascii_without_a_separator() {
    let theme = crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let rows = vec![
        Row::Text("Question 1/1".into()),
        Row::Text("Pick one?".into()),
        Row::Hints(vec!["Esc cancel".into()]),
    ];
    for width in [8usize, 12] {
        let lines = framed_with_roles(rows.clone(), width, 20, &theme);
        let view = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!view.contains(" | ") && !view.contains(" · "), "{view}");
        assert!(view.contains("Esc"), "{view}");
        assert!(
            lines.iter().all(|line| text::line_width(line) == width),
            "{view}"
        );
    }
}

#[test]
fn legacy_string_rows_keep_the_trailing_footer_classification() {
    let theme = crate::ui::theme::Theme::dark();
    // A trailing body row without the legacy separator is content, not a footer.
    let rows = vec!["Question 1/1".to_owned(), "plain trailing text".to_owned()];
    let lines = framed(rows, 40, 10, &theme);
    let plain: Vec<String> = lines.iter().map(ToString::to_string).collect();
    assert!(
        plain
            .iter()
            .any(|line| line.contains("plain trailing text"))
    );
    let muted = lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.content.contains("plain trailing text"))
        .unwrap();
    assert_eq!(muted.style.fg, theme.fg(crate::ui::theme::Tok::Text).fg);
}
