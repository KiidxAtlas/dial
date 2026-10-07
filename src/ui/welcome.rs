//! The start-up banner. Deliberately unframed: a small level-meter mark (dial's signature) beside
//! the wordmark, the active model and project on one quiet line, a row of keycap hints, the most
//! recent sessions, and a hairline that hands over to the transcript. Every row fits the width it is
//! given; narrow terminals wrap the hints and drop the mark.
use super::{
    Rows,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WelcomeInfo {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub project: String,
    /// `(key, what it does)` hints, drawn as keycaps.
    pub tips: Vec<(String, String)>,
    /// `(title, age)` of recent sessions, newest first.
    pub recent: Vec<(String, String)>,
}

const MAX_RECENT: usize = 3;
/// Recent-session rows never stretch wider than this, so the age stays near its title.
const RECENT_MAX: usize = 72;
/// The mark: a rising level meter.
const MARK: [&str; 5] = ["▁", "▃", "▄", "▆", "█"];
const MARK_ASCII: [&str; 5] = [".", "-", "=", "#", "#"];
const MARK_GAP: usize = 2;
const MARK_TOKS: [Tok; 5] = [
    Tok::StatusPath,
    Tok::StatusContext,
    Tok::Accent,
    Tok::Accent,
    Tok::Warning,
];

/// Left margin, then `spans` clipped to the remaining width.
fn row(margin: usize, spans: Vec<Span<'static>>, width: usize, ellipsis: &str) -> Line<'static> {
    let mut out = Vec::new();
    if margin > 0 {
        out.push(Span::raw(" ".repeat(margin)));
    }
    out.extend(truncate_spans(spans, width - margin, ellipsis));
    Line::from(out)
}

/// The banner as rows of at most `width` cells.
pub fn rows(info: &WelcomeInfo, width: u16, theme: &Theme) -> Rows {
    let width = width as usize;
    if width == 0 {
        return Vec::new();
    }
    let ascii = theme.ascii();
    let ellipsis = theme.sym(Sym::Ellipsis).to_owned();
    let margin = if width >= 24 { 2 } else { 0 };
    let inner = width - margin;
    let with_mark = inner >= 30;
    let indent = if with_mark { MARK.len() + MARK_GAP } else { 0 };
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let muted = theme.fg(Tok::Muted);
    let dim = theme.fg(Tok::Dim);
    let pad = |n: usize| Span::raw(" ".repeat(n));

    let mut out: Rows = vec![Line::default()];

    // ▁▃▄▆█  dial 0.1.0 · welcome back
    let mut head: Vec<Span<'static>> = Vec::new();
    if with_mark {
        let glyphs = if ascii { MARK_ASCII } else { MARK };
        for (g, tok) in glyphs.iter().zip(MARK_TOKS) {
            head.push(Span::styled(*g, theme.fg(tok)));
        }
        head.push(pad(MARK_GAP));
    }
    head.push(Span::styled(
        "dial",
        theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
    ));
    let version = text::one_line(info.version.trim());
    if !version.is_empty() {
        head.push(Span::styled(format!(" {version}"), dim));
    }
    let greeting = if info.recent.is_empty() {
        "welcome"
    } else {
        "welcome back"
    };
    head.push(Span::styled(format!("{dot}{greeting}"), muted));
    out.push(row(margin, head, width, &ellipsis));

    // model · provider · …/project
    let model = text::one_line(info.model.trim());
    let mut place: Vec<Span<'static>> = vec![pad(indent)];
    place.push(Span::styled(
        if model.is_empty() {
            "no model selected".to_owned()
        } else {
            model
        },
        theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
    ));
    let provider = text::one_line(info.provider.trim());
    if !provider.is_empty() {
        place.push(Span::styled(format!("{dot}{provider}"), muted));
    }
    let project = text::one_line(info.project.trim());
    if !project.is_empty() {
        // The tail of a path names the project; cut its head when the line is too long.
        let used: usize = place.iter().map(|s| text::width(&s.content)).sum();
        let room = inner
            .saturating_sub(used + text::width(&dot))
            .max(12.min(inner));
        place.push(Span::styled(dot.clone(), muted));
        place.push(Span::styled(
            text::ellipsize_start(&project, room, ascii),
            theme.fg(Tok::StatusPath),
        ));
    }
    out.push(row(margin, place, width, &ellipsis));

    // keycap hints, wrapped to the width
    if !info.tips.is_empty() {
        out.push(Line::default());
        let cap = theme
            .on(Tok::Text, Tok::SelectedBg)
            .add_modifier(Modifier::BOLD);
        let mut line: Vec<Span<'static>> = vec![pad(indent)];
        let mut used = indent;
        for (key, what) in &info.tips {
            let key = format!(" {} ", text::one_line(key));
            let what = format!(" {}", text::one_line(what));
            let w = text::width(&key) + text::width(&what);
            if used > indent && used + 3 + w > inner {
                out.push(row(margin, std::mem::take(&mut line), width, &ellipsis));
                line.push(pad(indent));
                used = indent;
            } else if used > indent {
                line.push(pad(3));
                used += 3;
            }
            line.push(Span::styled(key, cap));
            line.push(Span::styled(what, muted));
            used += w;
        }
        out.push(row(margin, line, width, &ellipsis));
    }

    // recent sessions: title … age
    if !info.recent.is_empty() {
        out.push(Line::default());
        let label = "recent";
        let label_w = text::width(label) + 2;
        let room = inner.min(RECENT_MAX).saturating_sub(indent + label_w);
        for (i, (title, age)) in info.recent.iter().take(MAX_RECENT).enumerate() {
            let mut spans = vec![pad(indent)];
            if i == 0 {
                spans.push(Span::styled(format!("{label}  "), dim));
            } else {
                spans.push(pad(label_w));
            }
            let title = text::one_line(title);
            let age = text::one_line(age);
            let age_w = text::width(&age);
            if room >= age_w + 6 {
                let title = text::ellipsize(&title, room - age_w - 2, ascii);
                let gap = room - age_w - text::width(&title);
                spans.push(Span::styled(title, theme.fg(Tok::Text)));
                spans.push(pad(gap));
                spans.push(Span::styled(age, dim));
            } else {
                spans.push(Span::styled(title, theme.fg(Tok::Text)));
            }
            out.push(row(margin, spans, width, &ellipsis));
        }
    }

    // hairline handing over to the transcript
    out.push(Line::default());
    out.push(row(
        margin,
        vec![Span::styled(
            theme.sym(Sym::Hr).repeat(inner),
            theme.fg(Tok::BorderMuted),
        )],
        width,
        &ellipsis,
    ));
    out.push(Line::default());
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
                ("Ctrl+P".into(), "actions".into()),
                ("/".into(), "commands".into()),
                ("!".into(), "bash".into()),
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
    fn wide_banner_shows_mark_identity_hints_and_recent_sessions() {
        let text = plain(&rows(&info(), 100, &Theme::dark()));
        let joined = text.join("\n");
        for needle in [
            "▁▃▄▆█",
            "dial 0.3.1",
            "welcome back",
            "qwen3.5-9b",
            "lmstudio",
            " Ctrl+P ",
            "actions",
            " ! ",
            "recent",
            "fix the parser",
            "add welcome banner",
        ] {
            assert!(joined.contains(needle), "missing {needle:?}\n{joined}");
        }
        assert!(!joined.contains('╭'), "the banner is unframed");
        let recent = text.iter().find(|l| l.contains("fix the parser")).unwrap();
        assert!(recent.trim_end().ends_with("2h"), "{recent}");
        assert!(
            text.iter().any(|l| l.contains("────")),
            "ends with a hairline"
        );
    }

    #[test]
    fn hints_wrap_instead_of_overflowing() {
        let mut many = info();
        many.tips = (0..12)
            .map(|i| (format!("Key{i}"), format!("does thing {i}")))
            .collect();
        let text = plain(&rows(&many, 60, &Theme::dark())).join("\n");
        for i in 0..12 {
            assert!(text.contains(&format!("Key{i}")), "{text}");
        }
    }

    #[test]
    fn empty_recent_and_tips_drop_their_sections() {
        let mut bare = info();
        bare.recent.clear();
        bare.tips.clear();
        let joined = plain(&rows(&bare, 90, &Theme::dark())).join("\n");
        assert!(joined.contains("welcome"));
        assert!(!joined.contains("welcome back"));
        assert!(!joined.contains("recent"));
        assert!(!joined.contains("Ctrl+P"));
    }

    #[test]
    fn long_project_path_preserves_project_name() {
        let mut welcome = info();
        welcome.project = "/a/very/long/parent/path/to/somewhere/deep/my-project".into();
        let rendered = plain(&rows(&welcome, 50, &Theme::dark())).join("\n");
        assert!(rendered.contains('…'));
        assert!(rendered.contains("my-project"), "{rendered}");
    }

    #[test]
    fn narrow_drops_the_mark_but_keeps_identity() {
        let text = plain(&rows(&info(), 20, &Theme::dark())).join("\n");
        assert!(!text.contains('▁'));
        assert!(text.contains("dial"));
    }

    #[test]
    fn long_and_hostile_fields_are_sanitized_and_ellipsized() {
        let hostile = WelcomeInfo {
            version: "1\x1b[31m".into(),
            provider: "p\x1b]0;x\x07rov".into(),
            model: "m".repeat(500),
            project: "日本語".repeat(50),
            tips: vec![("k\x1b[2J".into(), "tip\x1b[2J".repeat(40))],
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
        let spans: Vec<&Span> = rows.iter().flat_map(|l| &l.spans).collect();
        let word = spans.iter().find(|s| s.content == "dial").unwrap();
        assert_eq!(word.style.fg, theme.fg(Tok::Accent).fg);
        let cap = spans.iter().find(|s| s.content == " Ctrl+P ").unwrap();
        assert_eq!(cap.style.bg, theme.bg(Tok::SelectedBg).bg);
    }
}
