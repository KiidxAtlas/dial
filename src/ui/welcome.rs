//! The start-up banner: a rounded box with a greeting, a small "dial" mark and the active model on
//! the left; tips and recent sessions on the right. Under 70 cells it collapses to one column, and
//! under 8 cells (no room for a frame) to plain lines. Every row fits the width it is given.
use super::{
    Rows,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WelcomeInfo {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub project: String,
    pub tips: Vec<String>,
    /// `(title, age)` of recent sessions, newest first.
    pub recent: Vec<(String, String)>,
}

/// Two columns need room for both: at least this many cells.
const TWO_COLUMN_MIN: usize = 70;
/// Narrowest width that still draws a frame (corner, pad, one cell, pad, corner).
const FRAME_MIN: usize = 5;
const MAX_RECENT: usize = 5;
const LOGO_WIDTH: usize = 10;
const LOGO: [&str; 5] = [
    " ▄▄████▄▄ ",
    "██▀    ▀██",
    "██   ▄▀ ██",
    "██▄    ▄██",
    " ▀▀████▀▀ ",
];
const LOGO_ASCII: [&str; 5] = [
    " .-''''-. ",
    "/   /    \\",
    "|   o--> |",
    "\\        /",
    " '-....-' ",
];

/// One line of a column before it is fitted to the column width.
struct Entry {
    spans: Vec<Span<'static>>,
    center: bool,
}

fn entry(spans: Vec<Span<'static>>) -> Entry {
    Entry {
        spans,
        center: false,
    }
}

fn centered(spans: Vec<Span<'static>>) -> Entry {
    Entry {
        spans,
        center: true,
    }
}

fn blank() -> Entry {
    entry(Vec::new())
}

fn label(text: &str, max: usize, style: Style, ascii: bool) -> Span<'static> {
    Span::styled(text::ellipsize(&text::one_line(text), max, ascii), style)
}

/// Fit an entry to exactly `width` cells (ellipsized, then padded or centered).
fn fit(entry: Entry, width: usize, ellipsis: &str) -> Vec<Span<'static>> {
    let mut spans = truncate_spans(entry.spans, width, ellipsis);
    let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    let free = width - used;
    let (left, right) = if entry.center {
        (free / 2, free - free / 2)
    } else {
        (0, free)
    };
    if left > 0 {
        spans.insert(0, Span::raw(" ".repeat(left)));
    }
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

fn left_column(info: &WelcomeInfo, theme: &Theme, width: usize) -> Vec<Entry> {
    let ascii = theme.ascii();
    let bold = theme.fg(Tok::Text).add_modifier(Modifier::BOLD);
    let mut out = vec![
        centered(vec![label("Welcome back!", width, bold, ascii)]),
        blank(),
    ];
    if width >= LOGO_WIDTH {
        let rows = if ascii { LOGO_ASCII } else { LOGO };
        for row in rows {
            out.push(centered(vec![Span::styled(
                row.to_owned(),
                theme.fg(Tok::Accent),
            )]));
        }
        out.push(blank());
    }
    let model = if info.model.trim().is_empty() {
        "no model"
    } else {
        info.model.as_str()
    };
    out.push(centered(vec![label(
        model,
        width,
        theme.fg(Tok::Accent),
        ascii,
    )]));
    let place: Vec<&str> = [info.provider.as_str(), info.project.as_str()]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !place.is_empty() {
        let sep = format!(" {} ", theme.sym(Sym::Dot));
        out.push(centered(vec![label(
            &place.join(&sep),
            width,
            theme.fg(Tok::Muted),
            ascii,
        )]));
    }
    out
}

fn rule(theme: &Theme, width: usize) -> Entry {
    entry(vec![Span::styled(
        theme.sym(Sym::Hr).repeat(width),
        theme.fg(Tok::BorderMuted),
    )])
}

fn right_column(info: &WelcomeInfo, theme: &Theme, width: usize) -> Vec<Entry> {
    let ascii = theme.ascii();
    let heading = theme.fg(Tok::Accent).add_modifier(Modifier::BOLD);
    let mut out = Vec::new();
    if !info.tips.is_empty() {
        out.push(entry(vec![label("Get started", width, heading, ascii)]));
        for tip in &info.tips {
            out.push(entry(vec![label(tip, width, theme.fg(Tok::Muted), ascii)]));
        }
        out.push(rule(theme, width));
    }
    out.push(entry(vec![label(
        "Recent sessions  /resume",
        width,
        heading,
        ascii,
    )]));
    if info.recent.is_empty() {
        out.push(entry(vec![label(
            "No recent sessions",
            width,
            theme.fg(Tok::Muted),
            ascii,
        )]));
    }
    for (title, age) in info.recent.iter().take(MAX_RECENT) {
        let age = text::one_line(age);
        let age_w = text::width(&age);
        let (title_max, age) = if age_w + 3 <= width {
            (width - age_w - 1, Some(age))
        } else {
            (width, None)
        };
        let title = label(title, title_max, Style::default(), ascii);
        let used = text::width(&title.content);
        let mut spans = vec![title];
        if let Some(age) = age {
            spans.push(Span::raw(" ".repeat(width - used - age_w)));
            spans.push(Span::styled(age, theme.fg(Tok::Dim)));
        }
        out.push(entry(spans));
    }
    out
}

/// The banner as rows of at most `width` cells (exactly `width` when a frame is drawn).
pub fn rows(info: &WelcomeInfo, width: u16, theme: &Theme) -> Rows {
    let width = width as usize;
    if width == 0 {
        return Vec::new();
    }
    let ellipsis = theme.sym(Sym::Ellipsis).to_owned();
    if width < FRAME_MIN + 3 {
        let mut entries = left_column(info, theme, width);
        entries.retain(|e| !e.spans.is_empty());
        return entries
            .into_iter()
            .map(|e| Line::from(fit(e, width, &ellipsis)))
            .collect();
    }

    let two = width >= TWO_COLUMN_MIN;
    let inner = width - 4;
    let (entries_l, entries_r, left_w, right_w) = if two {
        let left_w = ((width - 7) * 2 / 5).clamp(24, 38);
        let right_w = width - 7 - left_w;
        (
            left_column(info, theme, left_w),
            right_column(info, theme, right_w),
            left_w,
            right_w,
        )
    } else {
        let mut all = left_column(info, theme, inner);
        all.push(rule(theme, inner));
        all.extend(right_column(info, theme, inner));
        (all, Vec::new(), inner, 0)
    };

    let border = theme.fg(Tok::Border);
    let v = theme.sym(Sym::BoxV).to_owned();
    let h = theme.sym(Sym::BoxH);
    let junction = left_w + 3;

    // top border carries the title: ╭─ dial v0.1.0 ───┬──────╮
    let title = if info.version.trim().is_empty() {
        "dial".to_owned()
    } else {
        format!("dial v{}", text::one_line(&info.version))
    };
    let mut cells: Vec<(String, Style)> = (0..width).map(|_| (h.to_owned(), border)).collect();
    cells[0].0 = theme.sym(Sym::BoxTl).to_owned();
    cells[width - 1].0 = theme.sym(Sym::BoxTr).to_owned();
    if two {
        cells[junction].0 = theme.sym(Sym::TeeDown).to_owned();
    }
    let title_room = if two {
        junction.saturating_sub(4)
    } else {
        width.saturating_sub(6)
    };
    if text::width(&title) <= title_room && title_room > 0 {
        let title_style = theme.fg(Tok::Accent).add_modifier(Modifier::BOLD);
        let mut at = 2;
        cells[at].0 = " ".to_owned();
        at += 1;
        for g in unicode_segmentation::UnicodeSegmentation::graphemes(title.as_str(), true) {
            let w = text::width(g);
            cells[at] = (g.to_owned(), title_style);
            for extra in 1..w {
                cells[at + extra].0 = String::new();
            }
            at += w.max(1);
        }
        cells[at].0 = " ".to_owned();
    }
    let mut top: Vec<Span<'static>> = Vec::new();
    for (g, style) in cells {
        match top.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(&g),
            _ => top.push(Span::styled(g, style)),
        }
    }
    let mut out = vec![Line::from(top)];

    let body = if two {
        entries_l.len().max(entries_r.len())
    } else {
        entries_l.len()
    };
    let mut left_iter = entries_l.into_iter();
    let mut right_iter = entries_r.into_iter();
    for _ in 0..body {
        let mut spans = vec![Span::styled(format!("{v} "), border)];
        spans.extend(fit(
            left_iter.next().unwrap_or_else(blank),
            left_w,
            &ellipsis,
        ));
        if two {
            spans.push(Span::styled(format!(" {v} "), border));
            spans.extend(fit(
                right_iter.next().unwrap_or_else(blank),
                right_w,
                &ellipsis,
            ));
        }
        spans.push(Span::styled(format!(" {v}"), border));
        out.push(Line::from(spans));
    }

    let mut bottom = format!(
        "{}{}{}",
        theme.sym(Sym::BoxBl),
        h.repeat(width - 2),
        theme.sym(Sym::BoxBr)
    );
    if two {
        // splice the tee into the rule at the column divider
        let rule_cells: Vec<String> = (0..width)
            .map(|i| match i {
                0 => theme.sym(Sym::BoxBl).to_owned(),
                i if i == width - 1 => theme.sym(Sym::BoxBr).to_owned(),
                i if i == junction => theme.sym(Sym::TeeUp).to_owned(),
                _ => h.to_owned(),
            })
            .collect();
        bottom = rule_cells.concat();
    }
    out.push(Line::from(Span::styled(bottom, border)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> WelcomeInfo {
        WelcomeInfo {
            version: "0.3.1".into(),
            provider: "lmstudio".into(),
            model: "qwen3.5-9b".into(),
            project: "dial".into(),
            tips: vec![
                "# prompt actions".into(),
                "/ commands".into(),
                "! bash".into(),
            ],
            recent: vec![
                ("fix the parser".into(), "2h".into()),
                ("add welcome banner".into(), "1d".into()),
            ],
        }
    }

    fn plain(rows: &Rows) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn never_wider_than_the_terminal_at_any_width() {
        for theme in [
            Theme::dark(),
            Theme::mono(),
            Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
        ] {
            for width in 1..=200u16 {
                let rows = rows(&info(), width, &theme);
                assert!(!rows.is_empty(), "width {width}");
                for row in &rows {
                    assert!(
                        text::line_width(row) <= width as usize,
                        "width {width}: {:?}",
                        plain(&vec![row.clone()])
                    );
                }
            }
        }
    }

    #[test]
    fn framed_banner_is_a_perfect_rectangle() {
        for width in 8..=200u16 {
            for row in rows(&info(), width, &Theme::dark()) {
                assert_eq!(text::line_width(&row), width as usize, "width {width}");
            }
        }
    }

    #[test]
    fn wide_banner_has_two_columns_with_tips_and_recent_sessions() {
        let text = plain(&rows(&info(), 100, &Theme::dark()));
        assert!(
            text[0].starts_with('╭') && text[0].contains("dial v0.3.1") && text[0].contains('┬')
        );
        assert!(text.last().unwrap().starts_with('╰') && text.last().unwrap().contains('┴'));
        let joined = text.join("\n");
        for needle in [
            "Welcome back!",
            "qwen3.5-9b",
            "lmstudio",
            "Get started",
            "# prompt actions",
            "! bash",
            "Recent sessions",
            "/resume",
            "fix the parser",
            "2h",
            "add welcome banner",
        ] {
            assert!(joined.contains(needle), "missing {needle:?}\n{joined}");
        }
        // columns: the welcome greeting and the tips share a row
        let tips_row = text.iter().find(|l| l.contains("Get started")).unwrap();
        assert_eq!(tips_row.matches('│').count(), 3, "{tips_row}");
        // the age sits at the right edge of its column
        let recent = text.iter().find(|l| l.contains("fix the parser")).unwrap();
        assert!(
            recent.trim_end_matches('│').trim_end().ends_with("2h"),
            "{recent}"
        );
    }

    #[test]
    fn narrow_banner_is_a_single_column_with_everything() {
        let rows = rows(&info(), 40, &Theme::dark());
        let text = plain(&rows);
        assert!(text.iter().all(|l| !l.contains('┬') && !l.contains('┴')));
        let joined = text.join("\n");
        for needle in [
            "Welcome back!",
            "qwen3.5-9b",
            "Get started",
            "/ commands",
            "Recent sessions",
            "/resume",
            "fix the parser",
        ] {
            assert!(joined.contains(needle), "missing {needle:?}\n{joined}");
        }
        assert!(
            text.iter().any(|l| l.contains("▄▀")),
            "logo is drawn when there is room"
        );
        assert!(text.iter().all(|l| text::width(l) == 40));
    }

    #[test]
    fn boundary_between_layouts_is_at_70() {
        assert!(
            plain(&rows(&info(), 69, &Theme::dark()))[0]
                .find('┬')
                .is_none()
        );
        assert!(plain(&rows(&info(), 70, &Theme::dark()))[0].contains('┬'));
    }

    #[test]
    fn empty_recent_says_so_and_empty_tips_drop_the_section() {
        let mut bare = info();
        bare.recent.clear();
        bare.tips.clear();
        let joined = plain(&rows(&bare, 90, &Theme::dark())).join("\n");
        assert!(joined.contains("No recent sessions"));
        assert!(!joined.contains("Get started"));
    }

    #[test]
    fn very_narrow_falls_back_to_plain_lines_without_a_frame() {
        let text = plain(&rows(&info(), 6, &Theme::dark()));
        assert!(text.iter().all(|l| !l.contains('╭')));
        assert!(text.iter().any(|l| l.contains("Welc")));
        assert_eq!(
            plain(&rows(&info(), 1, &Theme::dark()))
                .iter()
                .all(|l| text::width(l) <= 1),
            true
        );
    }

    #[test]
    fn long_and_hostile_fields_are_sanitized_and_ellipsized() {
        let hostile = WelcomeInfo {
            version: "1\x1b[31m".into(),
            provider: "p\x1b]0;x\x07rov".into(),
            model: "m".repeat(500),
            project: "日本語".repeat(50),
            tips: vec!["tip\x1b[2J".repeat(40)],
            recent: vec![("t\nitle".repeat(60), "\x1b[1m3d".into())],
        };
        for width in [10u16, 30, 69, 70, 120] {
            let rows = rows(&hostile, width, &Theme::dark());
            for row in &rows {
                assert!(text::line_width(row) <= width as usize);
                for span in &row.spans {
                    assert!(
                        !span.content.chars().any(char::is_control),
                        "{:?}",
                        span.content
                    );
                    assert!(!span.content.contains("[2J") && !span.content.contains("[31m"));
                }
            }
            assert!(plain(&rows).join("").contains('…'), "width {width}");
        }
    }

    #[test]
    fn ascii_theme_draws_with_ascii_only() {
        let theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
        for row in plain(&rows(&info(), 90, &theme)) {
            assert!(row.is_ascii(), "{row:?}");
        }
    }

    #[test]
    fn colors_come_from_the_theme() {
        let theme = Theme::dark();
        let rows = rows(&info(), 90, &theme);
        let model = rows
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("qwen3.5-9b"))
            .unwrap();
        assert_eq!(model.style.fg, theme.fg(Tok::Accent).fg);
        let border = &rows[1].spans[0];
        assert_eq!(border.style.fg, theme.fg(Tok::Border).fg);
    }
}
