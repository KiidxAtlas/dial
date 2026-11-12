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
fn resting_home_has_no_metadata_stack_and_offers_starters_where_room_allows() {
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
    // A tall first visit fills its spare room with the starters, in a card under the key hint.
    assert!(
        text.contains(STARTERS[0]) && !text.contains("sandbox"),
        "{text}"
    );
    let hint = text.find("Enter send").expect("key hint");
    assert!(hint < text.find(STARTERS[0]).unwrap(), "{text}");
    assert!(
        !text.contains("browse"),
        "the list is already shown: {text}"
    );
    assert!(content_width(120) <= 56);
    assert!(
        text.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
        "the braille knob lockup fills a tall window: {text}"
    );
}

#[test]
fn starters_yield_to_first_run_steps_and_short_windows() {
    let theme = Theme::dark();
    let render = |info: &WelcomeInfo, height| {
        let input = ComposerView {
            rows: vec![Line::from("draft")],
            cursor: (0, 0),
        };
        plain(&frame(info, 100, height, &theme, View::default(), input, vec![]).rows).join("\n")
    };
    let mut tour = info();
    tour.tour = true;
    let text = render(&tour, 40);
    assert!(
        text.contains("first steps") && !text.contains(STARTERS[0]),
        "{text}"
    );
    // Short: the hint row alone, still offering to browse the list.
    let short = render(&info(), 16);
    assert!(!short.contains(STARTERS[0]), "{short}");
    assert!(
        short.contains("Enter send") && short.contains("browse"),
        "{short}"
    );
}

#[test]
fn knob_turns_into_place_then_rests_on_one_fixed_frame() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let rest = mark::lockup(&theme, 1.0, 0);
    assert_eq!(rest.len(), mark::LOCKUP.1);
    for row in &rest {
        let width: usize = row.iter().map(|s| text::width(&s.content)).sum();
        assert_eq!(width, mark::LOCKUP.0);
    }
    assert_eq!(
        rest,
        mark::lockup(&theme, 4.0, 0),
        "settled frames never move"
    );
    assert_ne!(
        mark::lockup(&theme, 0.0, 0),
        rest,
        "the needle starts at its stop"
    );
    assert_ne!(mark::lockup(&theme, 0.5, 0), rest);
    let word = mark::wordmark(&theme, 1.0, 0);
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
fn ambient_motion_keeps_the_mark_alive_without_changing_its_size() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let rest = mark::lockup(&theme, 1.0, 0);
    let word_rest = mark::wordmark(&theme, 1.0, 0);
    let frames: Vec<_> = (0..200).map(|t| mark::lockup(&theme, 1.0, t)).collect();
    let words: Vec<_> = (0..200).map(|t| mark::wordmark(&theme, 1.0, t)).collect();
    for frame in &frames {
        assert_eq!(frame.len(), mark::LOCKUP.1);
        for row in frame {
            let width: usize = row.iter().map(|s| text::width(&s.content)).sum();
            assert_eq!(
                width,
                mark::LOCKUP.0,
                "ambient frames keep the lockup width"
            );
        }
    }
    for word in &words {
        assert_eq!(word.len(), mark::WORDMARK.1);
    }
    let glyphs = |rows: &Vec<Vec<Span<'static>>>| -> String {
        rows.iter().flatten().map(|s| s.content.as_ref()).collect()
    };
    assert!(
        frames.iter().any(|f| glyphs(f) != glyphs(&rest)),
        "the needle sways across tick marks"
    );
    assert!(
        frames
            .iter()
            .any(|f| f != &rest && glyphs(f) == glyphs(&rest)),
        "a glint changes colors only"
    );
    assert!(
        words.iter().any(|w| w != &word_rest),
        "the wordmark alone glints too"
    );
    assert_eq!(frames[0], rest, "tick 0 is the resting frame");
    assert_eq!(frames[96], rest, "the sway is periodic and the glint rests");
}

#[test]
fn the_bezel_traces_in_with_a_comet_before_the_needle_swings() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let (trace, level) = mark::boot_phases(0.0);
    assert_eq!(
        (trace, level),
        (0.0, 0.0),
        "nothing is drawn at the first frame"
    );
    let (trace, level) = mark::boot_phases(0.3);
    assert!(trace > 0.5 && trace < 1.0 && level < 0.5, "{trace} {level}");
    assert_eq!(mark::boot_phases(1.0), (1.0, 1.0));
    let dots = |rows: &Vec<Vec<Span<'static>>>| -> u32 {
        rows.iter()
            .flatten()
            .flat_map(|s| s.content.chars())
            .map(|c| (c as u32).saturating_sub(0x2800).count_ones())
            .sum()
    };
    let frames: Vec<u32> = [0.0, 0.15, 0.3, 0.6, 1.0]
        .iter()
        .map(|p| dots(&mark::lockup(&theme, *p, 0)))
        .collect();
    assert!(
        frames.windows(2).all(|w| w[0] <= w[1] + 8),
        "the mark fills in as it powers up: {frames:?}"
    );
    // The untuned wordmark is already there, dim; the bezel and ticks are not.
    assert!(frames[0] + 100 < frames[4], "{frames:?}");
    for p in [0.0, 0.2, 0.45, 0.7, 0.95] {
        for row in mark::lockup(&theme, p, 0) {
            let width: usize = row.iter().map(|s| text::width(&s.content)).sum();
            assert_eq!(width, mark::LOCKUP.0, "boot frames keep the lockup width");
        }
    }
}

#[test]
fn the_tagline_types_in_without_moving_and_context_stands_in_cards() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let mut info = info();
    info.version = "0.4.2".into();
    let full = tagline(&info, &theme, 1.0);
    let half = tagline(&info, &theme, 0.2);
    let width =
        |spans: &Vec<Span<'static>>| spans.iter().map(|s| text::width(&s.content)).sum::<usize>();
    assert_eq!(
        width(&full),
        width(&half),
        "typing never shifts the centered row"
    );
    let text_of =
        |spans: &Vec<Span<'static>>| spans.iter().map(|s| s.content.as_ref()).collect::<String>();
    assert!(text_of(&full).starts_with("v0.4.2"), "{}", text_of(&full));
    assert!(text_of(&half).trim_end().len() < text_of(&full).len());

    let view = render_home(120, 40, Some(0), "");
    let rows = plain(&view.rows);
    let all = rows.join("\n");
    assert!(all.contains(TAGLINE), "{all}");
    let tl = theme.sym(Sym::BoxTl);
    let heads: Vec<&String> = rows.iter().filter(|r| r.contains(tl)).collect();
    assert!(
        heads.iter().any(|r| r.contains("heads up")),
        "warnings stand in their own card: {all}"
    );
    assert!(
        heads.iter().any(|r| r.contains("START WITH")),
        "the list's heading rides its card's top edge: {all}"
    );
    assert!(
        all.contains("to pick one"),
        "the hint rides the bottom edge: {all}"
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

#[test]
fn tiny_launch_screens_keep_a_margin_and_name_the_project_whole() {
    let theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let keys = Keymap::defaults();
    let ui = UiConfig::default();
    let status = StatusData::default();
    let composer = Composer::new(vec![], 100);
    let mut info = info();
    info.project = "~/projects/atlas".into();
    info.branch = "main".into();
    for width in [30u16, 38, 43] {
        let input = composer.home_view(
            content_width(width),
            &ComposerCtx {
                theme: &theme,
                keys: &keys,
                ui: &ui,
                status: &status,
            },
        );
        let view = frame(&info, width, 16, &theme, View::default(), input, vec![]);
        let rows = plain(&view.rows);
        assert!(view.cursor.1 >= 1, "{width}: the input is inset");
        assert!(
            rows.iter()
                .all(|row| !row.starts_with('|') && text::width(row) <= width as usize),
            "{width}: {rows:?}"
        );
        let place = rows
            .iter()
            .find(|row| row.contains("local-model"))
            .expect("place row");
        assert!(place.contains("atlas"), "{width}: {place:?}");
        assert!(!place.contains("..."), "no mid-word cut: {place:?}");
    }
}

#[test]
fn recent_sessions_are_visible_at_rest_but_yield_to_warnings() {
    let theme = Theme::dark();
    let mut info = info();
    info.recent = vec![
        ("fix parser".into(), "2h".into()),
        ("add tests".into(), "1d".into()),
    ];
    info.model.clear();
    let render = |height| {
        frame(
            &info,
            100,
            height,
            &theme,
            View::default(),
            ComposerView {
                rows: vec![Line::from("draft")],
                cursor: (0, 0),
            },
            vec![],
        )
    };
    let tall = plain(&render(30).rows).join("\n");
    assert!(tall.contains("fix parser") && tall.contains("add tests"));
    let short = plain(&render(6).rows).join("\n");
    assert!(short.contains("no model selected"));
    assert!(!short.contains("fix parser"));
}

#[test]
fn first_run_guidance_fits_and_respects_ascii_glyphs() {
    let theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let mut info = info();
    info.tour = true;
    let view = frame(
        &info,
        100,
        30,
        &theme,
        View::default(),
        ComposerView {
            rows: vec![Line::from("draft")],
            cursor: (0, 0),
        },
        vec![],
    );
    let lines = plain(&view.rows);
    assert!(lines.iter().all(|line| line.is_ascii()));
    let text = lines.join("\n");
    assert!(
        text.contains("Type a request")
            && text.contains("F1 opens actions")
            && text.contains("/models")
    );
}

#[test]
fn the_home_names_the_approval_mode_and_the_key_that_changes_it() {
    let theme = Theme::dark();
    let draw = |info: &WelcomeInfo| {
        let input = ComposerView {
            rows: vec![Line::from("draft")],
            cursor: (0, 0),
        };
        plain(&frame(info, 120, 40, &theme, View::default(), input, vec![]).rows).join("\n")
    };
    let mut with_key = info();
    with_key.approval_key = "Shift+Tab".into();
    let text = draw(&with_key);
    assert!(text.contains("Shift+Tab mode: Ask first"), "{text}");
    // Without a bound key the hint row stays as it was.
    let text = draw(&info());
    assert!(!text.contains("mode:"), "{text}");
}
