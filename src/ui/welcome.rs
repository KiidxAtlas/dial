//! The start-up banner: a gradient wordmark beside the name, version and tagline; a framed card
//! that says what dial is ready to work with (model, project and branch, approval and sandbox);
//! numbered starter requests on a first visit or the recent sessions on a return; and the key
//! hints. Every row fits the width it is given. Below `CARD_MIN` columns, or with the ASCII glyph
//! set, the banner drops the wordmark and the frame and keeps the same facts as plain rows.
use super::{
    Rows,
    chrome::{key_hint_spans, label},
    fx,
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
    /// `(key, what it does)` hints, drawn as amber keys with muted labels.
    pub tips: Vec<(String, String)>,
    /// `(title, age)` of recent sessions, newest first.
    pub recent: Vec<(String, String)>,
    /// How to reopen a recent session, shown under the list.
    pub resume_hint: String,
    /// How to pick a model, shown when none is selected.
    pub setup_hint: String,
    /// Current git branch; empty outside a repository.
    pub branch: String,
    /// Approval mode as configured (`write`, `ask`, `auto`, `readonly`); empty hides the row.
    pub approval: String,
    /// Sandbox mode (`dev`, `strict`, `off`).
    pub sandbox: String,
    /// The shell `bash` commands run in (from `shell::shell_label`); empty hides the row.
    pub shell: String,
}

/// Starter requests shown to a project with no history, so the first message is never a blank page.
const STARTERS: [&str; 3] = [
    "explain this project",
    "find and fix a failing test",
    "review my uncommitted changes",
];

const MAX_RECENT: usize = 3;
/// Recent-session rows never stretch wider than this, so the age stays near its title.
const RECENT_MAX: usize = 72;
/// The mark that leads the first line.
const MARK: &str = "◉";
const MARK_ASCII: &str = "*";
const TAGLINE: &str = "Your coding agent, wired into this project.";
/// Narrowest width that gets the wordmark and the framed card.
const CARD_MIN: usize = 56;
/// The card never grows past this, so it reads as a card rather than a rule on wide terminals.
const CARD_MAX: usize = 92;
/// "dial" in half-block pixels, two rows tall.
const WORDMARK: [&str; 2] = [
    "\u{2584}\u{2584}\u{2588} \u{2580} \u{2584}\u{2584}\u{2584}  \u{2588} ",
    "\u{2588}\u{2584}\u{2588} \u{2588} \u{2588}\u{2584}\u{2588}\u{2584} \u{2588}\u{2584}",
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

/// What an approval mode means, in a few words.
fn approval_meaning(mode: &str) -> &'static str {
    match mode {
        "write" => "edits and safe commands run; risky ones ask",
        "ask" => "asks before every edit and command",
        "auto" => "runs tools without asking",
        "readonly" => "reads only; no edits or commands",
        _ => "",
    }
}

/// The facts the card shows, as `(label, value spans)` rows.
fn facts(
    info: &WelcomeInfo,
    theme: &Theme,
    room: usize,
) -> Vec<(&'static str, Vec<Span<'static>>)> {
    let ascii = theme.ascii();
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let muted = theme.fg(Tok::Muted);
    let mut out = Vec::new();

    let model = text::one_line(info.model.trim());
    let provider = text::one_line(info.provider.trim());
    let mut spans = Vec::new();
    if model.is_empty() {
        spans.push(Span::styled(
            format!("{} no model selected", theme.sym(Sym::Warn)),
            theme.fg(Tok::Warning).add_modifier(Modifier::BOLD),
        ));
        let hint = text::one_line(info.setup_hint.trim());
        if !hint.is_empty() {
            spans.push(Span::styled(format!("{dot}{hint}"), muted));
        }
    } else {
        spans.push(Span::styled(
            model,
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        ));
        if !provider.is_empty() {
            spans.push(Span::styled(format!("  via {provider}"), muted));
        }
    }
    out.push(("model", spans));

    let project = text::one_line(info.project.trim());
    let branch = text::one_line(info.branch.trim());
    if !project.is_empty() || !branch.is_empty() {
        let branch_part = if branch.is_empty() {
            String::new()
        } else {
            let icon = theme.sym(Sym::IconGit);
            let icon = if icon.is_empty() { "on" } else { icon };
            format!("  {icon} {branch}")
        };
        // The tail of a path names the project; cut its head when the row is too long.
        let keep = room.saturating_sub(text::width(&branch_part)).max(12);
        let mut spans = vec![Span::styled(
            text::ellipsize_start(&project, keep, ascii),
            theme.fg(Tok::Text),
        )];
        if !branch_part.is_empty() {
            spans.push(Span::styled(branch_part, theme.fg(Tok::StatusGitClean)));
        }
        out.push(("project", spans));
    }

    let approval = text::one_line(info.approval.trim());
    if !approval.is_empty() {
        let tok = match approval.as_str() {
            "auto" => Tok::Warning,
            "readonly" => Tok::StatusSandbox,
            _ => Tok::Success,
        };
        let mut spans = vec![Span::styled(
            approval.clone(),
            theme.fg(tok).add_modifier(Modifier::BOLD),
        )];
        let meaning = approval_meaning(&approval);
        if !meaning.is_empty() {
            spans.push(Span::styled(format!("{dot}{meaning}"), muted));
        }
        let sandbox = text::one_line(info.sandbox.trim());
        if !sandbox.is_empty() {
            // Auto approval with the sandbox off is the unguarded pairing: flag it in the warning colour.
            let unguarded = approval == "auto" && sandbox.eq_ignore_ascii_case("off");
            let style = if unguarded {
                theme.fg(Tok::Warning)
            } else {
                muted
            };
            spans.push(Span::styled(format!("{dot}sandbox {sandbox}"), style));
        }
        out.push(("mode", spans));
    }
    let shell = text::one_line(info.shell.trim());
    if !shell.is_empty() {
        out.push(("shell", vec![Span::styled(shell, muted)]));
    }
    out
}

/// One row inside the card frame: `│ spans … │`, padded to `inner` cells.
fn card_row(
    theme: &Theme,
    margin: usize,
    inner: usize,
    spans: Vec<Span<'static>>,
    ellipsis: &str,
) -> Line<'static> {
    let border = theme.fg(Tok::BorderMuted);
    let v = theme.sym(Sym::BoxV).to_owned();
    let body = truncate_spans(spans, inner, ellipsis);
    let used: usize = body.iter().map(|s| text::width(&s.content)).sum();
    let mut out = vec![
        Span::raw(" ".repeat(margin)),
        Span::styled(v.clone(), border),
    ];
    out.extend(body);
    out.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
    out.push(Span::styled(v, border));
    Line::from(out)
}

/// A horizontal card edge: `╭──────╮` or `╰──────╯`.
fn card_edge(theme: &Theme, margin: usize, inner: usize, top: bool) -> Line<'static> {
    let (left, right) = if theme.ascii() {
        ("+", "+")
    } else if top {
        ("\u{256d}", "\u{256e}")
    } else {
        ("\u{2570}", "\u{256f}")
    };
    let h = theme.sym(Sym::BoxH);
    Line::from(vec![
        Span::raw(" ".repeat(margin)),
        Span::styled(
            format!("{left}{}{right}", h.repeat(inner)),
            theme.fg(Tok::BorderMuted),
        ),
    ])
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
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let dot_w = text::width(&dot);
    let muted = theme.fg(Tok::Muted);
    let pad = |n: usize| Span::raw(" ".repeat(n));
    let greeting = if info.recent.is_empty() {
        "welcome"
    } else {
        "welcome back"
    };
    let version = text::one_line(info.version.trim());

    let mut out: Rows = vec![Line::default()];
    let framed = width >= CARD_MIN && !ascii;
    if framed {
        // Wordmark on the left, identity on the right.
        let title = |line: usize| -> Vec<Span<'static>> {
            match line {
                0 => {
                    let mut spans = vec![Span::styled(
                        "dial",
                        theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                    )];
                    if !version.is_empty() {
                        spans.push(Span::styled(format!(" {version}"), muted));
                    }
                    spans.push(Span::styled(format!("{dot}{greeting}"), muted));
                    spans
                }
                _ => vec![Span::styled(TAGLINE, muted)],
            }
        };
        for (i, mark) in WORDMARK.iter().enumerate() {
            let mut spans = fx::gradient(theme, mark, i as f32 * 0.08, Style::default());
            spans.push(pad(4));
            spans.extend(title(i));
            out.push(row(margin, spans, width, &ellipsis));
        }
        out.push(Line::default());

        let card_inner = inner.saturating_sub(2).min(CARD_MAX);
        let label_w = 9;
        let value_room = card_inner.saturating_sub(label_w + 3);
        out.push(card_edge(theme, margin, card_inner, true));
        for (name, value) in facts(info, theme, value_room) {
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(format!("{name:<label_w$}"), theme.fg(Tok::Dim)),
                Span::raw(" "),
            ];
            spans.extend(value);
            out.push(card_row(theme, margin, card_inner, spans, &ellipsis));
        }
        out.push(card_edge(theme, margin, card_inner, false));
    } else {
        // Compact: the same facts as plain rows.
        let mark = if ascii { MARK_ASCII } else { MARK };
        let mut head = vec![
            Span::styled(mark, theme.fg(Tok::Accent)),
            Span::raw(" "),
            Span::styled("dial", theme.fg(Tok::Text).add_modifier(Modifier::BOLD)),
        ];
        if !version.is_empty() {
            head.push(Span::styled(format!(" {version}"), muted));
        }
        head.push(Span::styled(format!("{dot}{greeting}"), muted));
        out.push(row(margin, head, width, &ellipsis));
        out.push(row(
            margin,
            vec![Span::styled(TAGLINE, muted)],
            width,
            &ellipsis,
        ));
        for (name, value) in facts(info, theme, inner.saturating_sub(9)) {
            let mut spans = vec![Span::styled(format!("{name:<8} "), theme.fg(Tok::Dim))];
            spans.extend(value);
            out.push(row(margin, spans, width, &ellipsis));
        }
    }

    // first visit to this project: numbered things to try
    if info.recent.is_empty() && !info.tips.is_empty() {
        out.push(Line::default());
        out.push(row(
            margin,
            vec![label(theme, "start with")],
            width,
            &ellipsis,
        ));
        for starter in STARTERS.iter() {
            // Bullets, not numbers: digits typed into the composer are text, not a starter pick.
            let spans = vec![
                pad(1),
                Span::styled(theme.sym(Sym::Bullet).to_owned(), theme.fg(Tok::Accent)),
                Span::styled(format!("  {starter}"), theme.fg(Tok::Text)),
            ];
            out.push(row(margin, spans, width, &ellipsis));
        }
    }

    // recent sessions: a label, then title … age
    if !info.recent.is_empty() {
        out.push(Line::default());
        out.push(row(margin, vec![label(theme, "recent")], width, &ellipsis));
        let room = inner.min(RECENT_MAX).saturating_sub(1);
        for (title, age) in info.recent.iter().take(MAX_RECENT) {
            let title = text::one_line(title);
            let age = text::one_line(age);
            let age_w = text::width(&age);
            let bullet = Span::styled(
                format!("{} ", theme.sym(Sym::Bullet)),
                theme.fg(Tok::Accent),
            );
            let spans = if room >= age_w + 8 {
                let title = text::ellipsize(&title, room - age_w - 4, ascii);
                let gap = room.saturating_sub(age_w + text::width(&title) + 2);
                vec![
                    pad(1),
                    bullet,
                    Span::styled(title, theme.fg(Tok::Text)),
                    pad(gap),
                    Span::styled(age, muted),
                ]
            } else {
                vec![pad(1), bullet, Span::styled(title, theme.fg(Tok::Text))]
            };
            out.push(row(margin, spans, width, &ellipsis));
        }
        let hint = text::one_line(info.resume_hint.trim());
        if !hint.is_empty() {
            out.push(row(
                margin,
                vec![pad(1), Span::styled(hint, muted)],
                width,
                &ellipsis,
            ));
        }
    }

    // key hints, wrapped to the width only when they cannot fit
    if !info.tips.is_empty() {
        out.push(Line::default());
        let mut line: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for (key, what) in &info.tips {
            let key = text::one_line(key);
            let what = text::one_line(what);
            let w = text::width(&key) + 1 + text::width(&what);
            if used > 0 && used + dot_w + w > inner {
                out.push(row(margin, std::mem::take(&mut line), width, &ellipsis));
                used = 0;
            } else if used > 0 {
                line.push(Span::styled(dot.clone(), muted));
                used += dot_w;
            }
            line.extend(key_hint_spans(theme, &[(key.as_str(), what.as_str())]));
            used += w;
        }
        out.push(row(margin, line, width, &ellipsis));
    }

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
            resume_hint: "/resume to continue one".into(),
            setup_hint: "Ctrl+K to choose one".into(),
            branch: String::new(),
            approval: "ask".into(),
            sandbox: "dev".into(),
            shell: "bash (/bin/bash)".into(),
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
    fn wide_banner_shows_wordmark_card_identity_hints_and_recent_sessions() {
        let text = plain(&rows(&info(), 100, &Theme::dark()));
        let joined = text.join("\n");
        for needle in [
            WORDMARK[0],
            WORDMARK[1],
            "dial 0.3.1",
            "welcome back",
            TAGLINE,
            "qwen3.5-9b",
            "via lmstudio",
            "ask",
            "asks before every edit and command",
            "sandbox dev",
            "Ctrl+P",
            "actions",
            "RECENT",
            "fix the parser",
            "add welcome banner",
        ] {
            assert!(joined.contains(needle), "missing {needle:?}\n{joined}");
        }
        assert!(
            joined.contains('\u{256d}') && joined.contains('\u{256f}'),
            "the facts sit in a rounded card"
        );
        let card_rows: Vec<&String> = text.iter().filter(|l| l.contains('\u{2502}')).collect();
        assert!(
            card_rows.len() >= 2,
            "model and mode rows are inside the card"
        );
        let widths: std::collections::HashSet<usize> = card_rows
            .iter()
            .map(|l| text::width(l.trim_end()))
            .collect();
        assert_eq!(
            widths.len(),
            1,
            "card rows share one right edge: {card_rows:?}"
        );
        let recent = text.iter().find(|l| l.contains("fix the parser")).unwrap();
        assert!(recent.trim_end().ends_with("2h"), "{recent}");
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
        assert!(!joined.contains("RECENT"));
        assert!(!joined.contains("Ctrl+P"));
        assert!(
            !joined.contains("TRY"),
            "starters need the tips to be enabled"
        );
    }

    #[test]
    fn a_first_visit_suggests_requests_and_a_return_visit_says_how_to_resume() {
        let mut first = info();
        first.recent.clear();
        let text = plain(&rows(&first, 120, &Theme::dark())).join("\n");
        assert!(text.contains("START WITH"), "{text}");
        let bullet = Theme::dark().sym(Sym::Bullet).to_owned();
        for starter in STARTERS.iter() {
            assert!(text.contains(&format!("{bullet}  {starter}")), "{text}");
        }
        assert!(!text.contains("/resume"));
        let back = plain(&rows(&info(), 120, &Theme::dark())).join("\n");
        assert!(back.contains("/resume to continue one") && !back.contains("explain this project"));
        let ascii = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
        assert!(
            plain(&rows(&first, 120, &ascii))
                .iter()
                .all(|r| r.is_ascii())
        );
    }
    #[test]
    fn a_missing_model_is_a_warning_with_the_way_to_fix_it() {
        let mut none = info();
        none.model.clear();
        let theme = Theme::dark();
        let rows = rows(&none, 100, &theme);
        let warn = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .find(|s| s.content.contains("no model selected"))
            .expect("warning");
        assert_eq!(warn.style.fg, theme.fg(Tok::Warning).fg);
        assert!(plain(&rows).join("\n").contains("Ctrl+K to choose one"));
    }

    #[test]
    fn auto_approval_with_the_sandbox_off_reads_as_a_sentence_and_is_flagged() {
        let theme = Theme::dark();
        let mut risky = info();
        risky.approval = "auto".into();
        risky.sandbox = "off".into();
        let lines = rows(&risky, 120, &theme);
        assert!(
            plain(&lines)
                .join("\n")
                .contains("auto · runs tools without asking · sandbox off")
        );
        let spans: Vec<Span<'_>> = lines.iter().flat_map(|l| l.spans.iter().cloned()).collect();
        let sandbox = spans
            .iter()
            .find(|span| span.content.contains("sandbox off"))
            .expect("sandbox span");
        assert_eq!(sandbox.style.fg, theme.fg(Tok::Warning).fg);
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
    fn wordmark_rows_share_one_width_so_the_title_column_lines_up() {
        assert_eq!(text::width(WORDMARK[0]), text::width(WORDMARK[1]));
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
            resume_hint: "/resume\x1b[2J".repeat(30),
            setup_hint: String::new(),
            branch: String::new(),
            approval: String::new(),
            sandbox: String::new(),
            shell: String::new(),
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
    fn colors_come_from_the_theme_without_fills() {
        let theme = Theme::builtin("dial-dark").unwrap();
        let rows = rows(&info(), 100, &theme);
        let spans: Vec<&Span> = rows.iter().flat_map(|l| &l.spans).collect();
        assert!(
            spans.iter().all(|s| s.style.bg.is_none()),
            "nothing paints a background"
        );
        // The wordmark is a gradient: its glyphs take several brand colors, none of them default.
        let mark_colors: std::collections::HashSet<_> = rows[1]
            .spans
            .iter()
            .filter(|s| {
                s.content
                    .chars()
                    .any(|c| matches!(c, '\u{2580}' | '\u{2584}' | '\u{2588}'))
            })
            .map(|s| s.style.fg)
            .collect();
        assert!(mark_colors.len() >= 3, "{mark_colors:?}");
        assert!(!mark_colors.contains(&None));
        let model = spans.iter().find(|s| s.content == "qwen3.5-9b").unwrap();
        assert_eq!(model.style.fg, theme.fg(Tok::Accent).fg);
        let key = spans.iter().find(|s| s.content == "Ctrl+P").unwrap();
        assert_eq!(key.style.fg, theme.fg(Tok::Accent).fg);
        let edge = spans
            .iter()
            .find(|s| s.content.starts_with('\u{256d}'))
            .unwrap();
        assert_eq!(edge.style.fg, theme.fg(Tok::BorderMuted).fg);
    }
}
