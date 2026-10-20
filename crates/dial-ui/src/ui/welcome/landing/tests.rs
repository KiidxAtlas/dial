use super::*;
use crate::ui::{
    composer::{Composer, ComposerCtx},
    keys::Keymap,
    status::StatusData,
};
use dial_llm::llm::model_config::UiConfig;

fn info() -> WelcomeInfo {
    WelcomeInfo {
        model: "local-model".into(),
        project: "~/projects/dial".into(),
        approval: "ask".into(),
        sandbox: "dev".into(),
        setup_hint: "Ctrl+K to choose one".into(),
        tips: vec![("/".into(), "commands".into())],
        ..Default::default()
    }
}

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|row| row.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn render_home(width: u16, height: u16, selected: Option<usize>, draft: &str) -> ComposerView {
    let theme = Theme::builtin("dial-dark").unwrap();
    let keys = Keymap::defaults();
    let ui = UiConfig::default();
    let status = StatusData::default();
    let mut composer = Composer::new(vec![], 100);
    composer.set_draft(draft);
    let view = composer.home_view(
        content_width(width),
        &ComposerCtx {
            theme: &theme,
            keys: &keys,
            ui: &ui,
            status: &status,
        },
    );
    let mut info = info();
    info.approval = "auto".into();
    info.sandbox = "off".into();
    frame(
        &info,
        width,
        height,
        &theme,
        View {
            selected,
            live: true,
            ..Default::default()
        },
        view,
        vec![],
    )
}

#[test]
fn input_is_centered_and_cursor_tracks_the_real_draft() {
    for (width, height) in [(120, 40), (100, 24), (80, 30)] {
        let empty = render_home(width, height, None, "");
        let typed = render_home(width, height, None, "hello");
        assert_eq!(empty.rows.len(), height as usize);
        assert_eq!(empty.cursor.0, height / 2);
        assert_eq!(typed.cursor, (empty.cursor.0, empty.cursor.1 + 5));
        let line = &typed.rows[typed.cursor.0 as usize];
        assert!(line.spans.iter().any(|s| s.content.contains("hello")));
        assert!(typed.cursor.1 >= (width - content_width(width)) / 2);
    }
}

#[test]
fn resting_home_has_no_metadata_stack_or_action_list() {
    let theme = Theme::dark();
    let input = ComposerView {
        rows: vec![Line::from("draft")],
        cursor: (0, 0),
    };
    let view = frame(&info(), 120, 40, &theme, View::default(), input, vec![]);
    let text = plain(&view.rows).join("\n");
    assert!(
        text.contains("local-model") && text.contains("~/projects/dial"),
        "the model and current directory stay visible: {text}"
    );
    assert!(!text.contains(STARTERS[0]) && !text.contains("sandbox"));
    assert!(content_width(120) <= 56);
    assert!(
        text.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
        "the braille knob lockup fills a tall window: {text}"
    );
}

#[test]
fn knob_turns_into_place_then_rests_on_one_fixed_frame() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let rest = mark::lockup(&theme, 1.0);
    assert_eq!(rest.len(), mark::LOCKUP.1);
    for row in &rest {
        let width: usize = row.iter().map(|s| text::width(&s.content)).sum();
        assert_eq!(width, mark::LOCKUP.0);
    }
    assert_eq!(rest, mark::lockup(&theme, 4.0), "settled frames never move");
    assert_ne!(
        mark::lockup(&theme, 0.0),
        rest,
        "the needle starts at its stop"
    );
    assert_ne!(mark::lockup(&theme, 0.5), rest);
    let word = mark::wordmark(&theme, 1.0);
    assert_eq!(word.len(), mark::WORDMARK.1);
    assert!(
        rest.iter()
            .chain(&word)
            .flatten()
            .flat_map(|s| s.content.chars())
            .all(|c| c == ' ' || ('\u{2801}'..='\u{28ff}').contains(&c))
    );
}

#[test]
fn resizing_keeps_input_and_selected_actions_visible_without_overflow() {
    for width in [1, 12, 38, 60, 80, 140] {
        for height in [12, 16, 20, 40] {
            let view = render_home(width, height, Some(2), "a draft that can wrap");
            assert!(view.rows.len() <= height as usize);
            assert!((view.cursor.0 as usize) < view.rows.len());
            assert!(view.cursor.1 < width);
            for row in &view.rows {
                assert!(
                    text::line_width(row) <= width as usize,
                    "{width}x{height}: {row:?}"
                );
            }
            if width >= 38 {
                let plain = plain(&view.rows).join("\n");
                assert!(plain.contains(STARTERS[2]), "{width}x{height}: {plain}");
                assert!(plain.contains("sandbox") && plain.contains("off"));
            }
        }
    }
}

#[test]
fn missing_model_recovery_and_auxiliary_work_are_preserved() {
    let theme = Theme::dark();
    let mut info = info();
    info.model.clear();
    let input = ComposerView {
        rows: vec![Line::from("draft")],
        cursor: (0, 5),
    };
    let view = frame(
        &info,
        100,
        24,
        &theme,
        View::default(),
        input,
        vec![Line::from("active worker")],
    );
    let plain = plain(&view.rows).join("\n");
    assert!(plain.contains("no model selected") && plain.contains("Ctrl+K to choose one"));
    assert!(plain.contains("active worker"));
    assert_eq!(view.rows.len(), 24);
}

#[test]
fn ascii_mode_keeps_the_launch_graphic_and_context_ascii() {
    let theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let info = info();
    let view = frame(
        &info,
        100,
        40,
        &theme,
        View::default(),
        ComposerView {
            rows: vec![Line::from("draft")],
            cursor: (0, 0),
        },
        vec![],
    );
    assert!(plain(&view.rows).iter().all(|line| line.is_ascii()));
}
