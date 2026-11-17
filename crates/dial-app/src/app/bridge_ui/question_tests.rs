use super::*;
use crate::agent::workflow::questions::{Question, QuestionOption, QuestionRequest};
use crate::app::shell::prompts::Focus;
use crate::ui::{keys::Keymap, questions::Dialog, theme::Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::Path;

fn request() -> QuestionRequest {
    QuestionRequest {
        question_id: "question".into(),
        generation: 1,
        session_id: "session".into(),
        owner_task_id: None,
        questions: vec![Question {
            id: "q".into(),
            question: "Choose how to proceed with this work?".into(),
            header: None,
            options: vec![QuestionOption {
                label: "Continue safely".into(),
                description: None,
                preview: None,
            }],
            multi: false,
            recommended: None,
        }],
        timeout_seconds: 0,
        notify: false,
    }
}

#[test]
fn short_question_panels_keep_answers_controls_and_ancillary_focus() {
    let request = request();
    let keys = Keymap::defaults();
    let theme = Theme::dark();
    for width in [38, 80] {
        for focus in [Focus::Choices, Focus::Draft] {
            for note in [
                None,
                Some("Your draft (Tab to return to the choices):\nretained draft\nsecond\nthird"),
            ] {
                let mut dialog = Dialog::new(&request, Path::new("/tmp"));
                for text_mode in [false, true] {
                    if text_mode {
                        dialog.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &keys);
                        dialog.paste("custom answer");
                    }
                    // A ten-row terminal leaves one row for shell status.
                    let rows = question_panel(&dialog, width, 9, note, focus, &theme);
                    let view = rows
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(rows.len() <= 9, "{width}x10: {view}");
                    assert!(rows.iter().all(|row| text::line_width(row) <= width));
                    for expected in if text_mode {
                        ["custom answer│", "Enter submit", "Esc back"]
                    } else {
                        ["Continue safely", "Enter select", "Esc cancel"]
                    } {
                        assert!(
                            view.contains(expected),
                            "{width}x10 missing {expected}: {view}"
                        );
                    }
                    assert!(view.contains("F6 check"), "{view}");
                    if note.is_some() {
                        assert!(view.contains("retained draft"), "{view}");
                        let expected = if focus == Focus::Draft {
                            "Tab: choices"
                        } else {
                            "Tab: edit"
                        };
                        assert!(view.contains(expected), "{view}");
                    }
                }
            }
        }
    }
}

#[test]
fn roomy_question_panels_preserve_normal_ancillary_rows() {
    let request = request();
    let dialog = Dialog::new(&request, Path::new("/tmp"));
    let note = "Your draft (Tab to edit your draft):\nfirst draft line\nsecond draft line\nthird draft line\nfourth omitted";
    let rows = question_panel(&dialog, 100, 23, Some(note), Focus::Choices, &Theme::dark());
    let view = rows
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        ATTENTION_HINT,
        "Your draft (Tab to edit your draft):",
        "first draft line",
        "third draft line",
    ] {
        assert!(view.contains(expected), "{view}");
    }
    assert!(!view.contains("fourth omitted"));
}

#[test]
fn minimal_question_panels_bound_rows_and_keep_the_active_draft_focus_hint() {
    let request = request();
    let dialog = Dialog::new(&request, Path::new("/tmp"));
    let note = "Your draft (Tab to return to the choices):\nunfinished";
    for width in [0, 1, 7, 8, 38, 80] {
        for height in 0..=9 {
            let rows = question_panel(
                &dialog,
                width,
                height,
                Some(note),
                Focus::Draft,
                &Theme::dark(),
            );
            assert!(rows.len() <= height, "{width}x{height}: {rows:?}");
            assert!(rows.iter().all(|row| text::line_width(row) <= width));
            if width >= 38 && height >= 5 {
                let view = rows
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    view.contains("Tab: choices") || view.contains("Tab to return to the choices"),
                    "{width}x{height}: {view}"
                );
                assert!(view.contains("Continue safely"), "{width}x{height}: {view}");
            }
        }
    }
}
