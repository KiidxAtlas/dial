use super::*;

#[test]
fn tags_and_status_are_plain_text_without_fill() {
    let theme = Theme::dark();
    assert_eq!(tag(&theme, Section::Learn, "memory").content, "MEMORY");
    assert_eq!(tag(&theme, Section::Learn, "memory").style.bg, None);
    assert_eq!(
        status_chip(&theme, Status::Ready, "ready").content,
        "● ready"
    );
    assert_eq!(status_chip(&theme, Status::Ready, "ready").style.bg, None);
}

#[test]
fn framed_lines_have_one_width_and_keep_the_title() {
    let theme = Theme::dark();
    for width in [6usize, 20, 80] {
        let rows = vec![Line::from(
            "a row that is longer than a narrow frame can hold",
        )];
        let lines = framed_lines(&theme, Section::Dial, "Sessions", "3 found", rows, width);
        for line in &lines {
            assert_eq!(text::line_width(line), width, "width {width}");
        }
    }
    let lines = framed_lines(&theme, Section::Dial, "Sessions", "3 found", vec![], 40);
    let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(top.contains("Sessions") && top.contains("3 found"), "{top}");
}

#[test]
fn every_tab_strip_marks_the_active_tab_as_a_pill_on_the_panel_fill() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let active = tab_style(&theme, true);
    let idle = tab_style(&theme, false);
    assert_eq!(active.fg, theme.fg(Tok::Accent).fg);
    assert!(
        active
            .add_modifier
            .contains(Modifier::UNDERLINED | Modifier::BOLD)
    );
    assert_eq!(active.bg, surface(&theme).bg);
    assert_eq!(idle.fg, theme.fg(Tok::Muted).fg);
    assert!(idle.bg.is_none());
}

#[test]
fn selected_rows_read_in_one_body_color_on_the_fill() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let mut spans = vec![
        Span::styled("› ", theme.fg(Tok::Accent)),
        Span::styled("Local models", theme.fg(Tok::Text)),
        Span::styled("● cached", theme.fg(Tok::Success)),
        Span::styled(
            "8 models",
            theme.fg(Tok::Muted).add_modifier(Modifier::BOLD),
        ),
    ];
    select_spans(&theme, &mut spans);
    let fill = theme.bg(Tok::SelectedBg).bg;
    for span in &spans {
        assert_eq!(span.style.bg, fill, "{:?}", span.content);
        assert_eq!(span.style.fg, theme.fg(Tok::Text).fg, "{:?}", span.content);
    }
    assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
    assert!(
        !spans[3].style.add_modifier.contains(Modifier::BOLD),
        "details stay secondary"
    );
}

#[test]
fn selected_row_fills_exactly_the_row_width() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let fill = theme.bg(Tok::SelectedBg).bg;
    for width in [4usize, 12, 40] {
        let row = selected_row(
            &theme,
            vec![Span::styled("a long selected row", theme.fg(Tok::Muted))],
            width,
        );
        assert_eq!(text::line_width(&row), width, "width {width}");
        assert!(row.spans.iter().all(|s| s.style.bg == fill), "{row:?}");
    }
}

fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn hint_line_keeps_the_way_out_and_drops_middle_hints_whole() {
    let theme = Theme::dark();
    let hints = [
        ("enter", "open"),
        ("/", "search"),
        ("", ""),
        ("ctrl+d", "delete"),
        ("esc", "close"),
    ];
    let cases: &[(usize, &str)] = &[
        (80, "enter open · / search · ctrl+d delete · esc close"),
        (40, "enter open · / search · esc close"),
        (25, "enter open · esc close"),
        (12, "esc close"),
        (6, "esc c…"),
    ];
    for (width, want) in cases {
        let line = hint_line(&theme, &hints, *width);
        assert_eq!(plain(&line), *want, "width {width}");
        assert!(text::line_width(&line) <= *width);
    }
    let led = hint_line_after(&theme, "3/12", &hints, 30);
    assert_eq!(plain(&led), "3/12 · enter open · esc close");
    assert_eq!(plain(&hint_line_after(&theme, "3/12", &[], 24)), "3/12");
}

#[test]
fn state_marks_share_one_vocabulary_and_only_failure_is_an_error() {
    let ascii = Theme::dark().with_glyphs(super::super::theme::Glyphs::Ascii);
    let words: std::collections::HashSet<&str> = StateMark::ALL.iter().map(|m| m.word()).collect();
    assert_eq!(words.len(), StateMark::ALL.len());
    for mark in StateMark::ALL {
        assert!(!mark.glyph(&ascii).is_empty() && mark.glyph(&ascii).is_ascii());
        let want = match mark {
            StateMark::Failed => Tok::Error,
            StateMark::Running => Tok::Accent,
            StateMark::Blocked => Tok::Warning,
            StateMark::Done | StateMark::Answered => Tok::Success,
            _ => Tok::Muted,
        };
        assert_eq!(mark.tok(), want, "{mark:?}");
    }
    let theme = Theme::dark();
    assert_eq!(StateMark::Done.chip_span(&theme).content, "✓ done");
    assert_eq!(StateMark::Interrupted.mark_span(&theme).content, "⊘");
}

#[test]
fn approval_modes_use_their_friendly_name_and_safety_color() {
    let theme = Theme::dark();
    for (mode, name, tok) in [
        ("ask", "Ask first", Tok::Success),
        ("readonly", "Read-only", Tok::Success),
        ("write", "Edit freely", Tok::Accent),
        ("auto", "Full auto", Tok::Warning),
        ("custom", "custom", Tok::Muted),
    ] {
        let spans = approval_spans(&theme, mode);
        assert_eq!(spans[0].style.fg, theme.fg(tok).fg, "{mode}");
        assert_eq!(spans[1].content, name);
    }
}

#[test]
fn bottom_hint_fills_only_a_blank_bottom_row() {
    use ratatui::{buffer::Buffer, layout::Rect};
    let theme = Theme::dark();
    let area = Rect::new(0, 0, 40, 5);
    let row = |buf: &Buffer, y: u16| -> String {
        (0..area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_owned())
            .collect()
    };
    let mut buf = Buffer::empty(area);
    assert!(stamp_bottom_hint(&mut buf, area, &theme, "Ctrl+C closes"));
    assert!(
        row(&buf, 4).ends_with("Ctrl+C closes "),
        "{:?}",
        row(&buf, 4)
    );

    let mut busy = Buffer::empty(area);
    busy.set_string(1, 4, "footer", Style::default());
    assert!(!stamp_bottom_hint(&mut busy, area, &theme, "Ctrl+C closes"));
    assert_eq!(row(&busy, 4).trim(), "footer", "content is never covered");

    let narrow = Rect::new(0, 0, 12, 5);
    let mut small = Buffer::empty(narrow);
    assert!(!stamp_bottom_hint(
        &mut small,
        narrow,
        &theme,
        "Ctrl+C closes"
    ));
}
