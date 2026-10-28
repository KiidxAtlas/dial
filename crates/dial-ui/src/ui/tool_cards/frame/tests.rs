use super::*;
use crate::ui::theme::Glyphs;

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn sample(theme: &Theme, head: &str) -> Frame {
    let mut frame = Frame::new(
        vec![Span::styled(head.to_owned(), theme.fg(Tok::ToolTitle))],
        theme.fg(Tok::Border),
    );
    frame.sections = vec![
        (None, vec![Line::from("$ cargo test")]),
        (
            Some("Output".into()),
            vec![
                Line::from("test result: ok. 18 passed; 0 failed"),
                Line::from(""),
            ],
        ),
    ];
    let dot = theme.sym(Sym::Dot);
    frame.foot = vec![Span::styled(
        format!("exit 0 {dot} 1.20s"),
        theme.fg(Tok::Dim),
    )];
    frame
}

#[test]
fn frames_are_exactly_as_wide_as_asked_with_titled_edges_and_sections() {
    let theme = Theme::builtin("dial-dark").unwrap();
    for w in [MIN_W, 20, 41, 80] {
        let rows = sample(&theme, "Run cargo test").rows(&theme, w);
        for row in &rows {
            assert_eq!(text::line_width(row), w, "{:?}", plain(&rows));
        }
        let lines = plain(&rows);
        assert!(lines[0].starts_with("╭─") && lines[0].ends_with('╮'));
        assert!(lines.last().unwrap().starts_with("╰─"));
        assert!(lines.last().unwrap().ends_with('╯'));
        assert!(lines[2].starts_with("├─") && lines[2].ends_with('┤'));
        for body in [&lines[1], &lines[3], &lines[4]] {
            assert!(body.starts_with("│ ") && body.ends_with(" │"), "{body:?}");
        }
        if w >= 40 {
            assert!(lines[0].contains(" Run cargo test "), "{:?}", lines[0]);
            assert!(lines[2].contains(" Output "));
            assert!(lines.last().unwrap().contains(" exit 0 · 1.20s "));
        }
    }
}

#[test]
fn long_titles_and_rows_are_clipped_with_an_ellipsis() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let rows = sample(&theme, &"word ".repeat(40)).rows(&theme, 30);
    let lines = plain(&rows);
    assert!(
        lines[0].contains('…') && lines[0].ends_with("─╮"),
        "{:?}",
        lines[0]
    );
    assert!(lines[3].contains('…'), "{:?}", lines[3]);
    assert!(rows.iter().all(|r| text::line_width(r) == 30));
}

#[test]
fn tiny_widths_never_overflow() {
    let theme = Theme::builtin("dial-dark").unwrap();
    for w in 0..MIN_W {
        for row in sample(&theme, "Run").rows(&theme, w) {
            assert!(text::line_width(&row) <= w.max(2), "{w}: {row:?}");
        }
    }
}

#[test]
fn ascii_frames_and_chips_use_ascii_glyphs() {
    let theme = Theme::builtin("dial-dark")
        .unwrap()
        .with_glyphs(Glyphs::Ascii);
    let lines = plain(&sample(&theme, "Run").rows(&theme, 40));
    assert!(lines[0].starts_with("+-") && lines[0].ends_with('+'));
    assert!(lines[1].starts_with("| ") && lines[1].ends_with(" |"));
    assert!(lines.iter().all(|l| l.is_ascii()), "{lines:?}");
    assert_eq!(chip(&theme, "done", Tok::Success).content, "[done]");
    let unicode = Theme::builtin("dial-dark").unwrap();
    assert_eq!(chip(&unicode, "done", Tok::Success).content, "⟦done⟧");
}

#[test]
fn the_fill_reaches_every_cell_without_overriding_span_backgrounds() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let mut frame = sample(&theme, "MCP");
    frame.fill = theme.bg(Tok::PanelBg);
    let fill = theme.bg(Tok::PanelBg).bg;
    for row in frame.rows(&theme, 40) {
        assert!(row.spans.iter().all(|s| s.style.bg == fill), "{row:?}");
    }
}
