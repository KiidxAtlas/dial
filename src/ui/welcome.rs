//! The home screen: a gradient wordmark beside the name, version and tagline; a framed card that
//! says what dial is ready to work with (model, project and branch, approval and sandbox); starter
//! requests on a first visit or the recent sessions on a return; and the key hints. Every row fits
//! the width it is given. Below `CARD_MIN` columns, or with the ASCII glyph set, the wordmark and
//! the frame drop and the same facts stay as plain rows.
//!
//! [`Home`] keeps it live in the viewport until the conversation starts: its list rows are
//! selectable, the wordmark gets one start-up sweep, and on the first message it collapses to the
//! one-line [`Home::summary`] in scrollback.
use super::{
    Rows,
    chrome::{key_hint_spans, label, selected_style},
    fx,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::time::Instant;

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
/// Narrowest width that puts the list beside the card instead of under it.
const TWO_COLUMN_MIN: usize = 110;
/// Cells between the card and the list in two columns.
const COLUMN_GAP: usize = 4;
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

/// How the banner draws: the selected list row, how far the start-up sweep has run, and how many
/// rows it may take.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub selected: Option<usize>,
    /// Sweep progress; `1.0` and above is the resting frame.
    pub sweep: f32,
    pub max_rows: usize,
    /// Live in the viewport, where list rows can be picked, rather than a scrollback banner.
    pub live: bool,
}

impl Default for View {
    fn default() -> Self {
        View {
            selected: None,
            sweep: 1.0,
            max_rows: usize::MAX,
            live: false,
        }
    }
}

/// The banner as rows of at most `width` cells.
pub fn rows(info: &WelcomeInfo, width: u16, theme: &Theme) -> Rows {
    render(info, width, theme, View::default())
}

/// Rows for `view`: the full layout when it fits `view.max_rows`, else the compact one, cut to fit.
pub fn render(info: &WelcomeInfo, width: u16, theme: &Theme, view: View) -> Rows {
    let mut out = layout(info, width as usize, theme, &view, false);
    if out.len() > view.max_rows {
        out = layout(info, width as usize, theme, &view, true);
        out.truncate(view.max_rows);
    }
    out
}

/// Rows that can be picked: recent sessions on a return, starters on a first visit.
fn items(info: &WelcomeInfo) -> usize {
    if !info.recent.is_empty() {
        info.recent.len().min(MAX_RECENT)
    } else if !info.tips.is_empty() {
        STARTERS.len()
    } else {
        0
    }
}

/// The list section (label, rows, hint) as unpadded lines; `room` is the width it may use.
fn list_lines(
    info: &WelcomeInfo,
    theme: &Theme,
    view: &View,
    room: usize,
) -> Vec<Vec<Span<'static>>> {
    let ascii = theme.ascii();
    let muted = theme.fg(Tok::Muted);
    let pad = |n: usize| Span::raw(" ".repeat(n));
    let picked = |i: usize| view.selected == Some(i);
    let marker = |i: usize| {
        if picked(i) {
            Span::styled(
                theme.sym(Sym::Pointer).to_owned(),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(theme.sym(Sym::Bullet).to_owned(), theme.fg(Tok::Accent))
        }
    };
    let body = |i: usize| {
        if picked(i) {
            selected_style(theme)
        } else {
            theme.fg(Tok::Text)
        }
    };
    let mut out = Vec::new();

    // first visit to this project: things to try
    if info.recent.is_empty() && !info.tips.is_empty() {
        out.push(vec![label(theme, "start with")]);
        for (i, starter) in STARTERS.iter().enumerate() {
            // Bullets, not numbers: digits typed into the composer are text, not a starter pick.
            out.push(vec![
                pad(1),
                marker(i),
                Span::styled(format!("  {starter}"), body(i)),
            ]);
        }
        if view.live {
            let down = if ascii { "Down" } else { "\u{2193}" };
            out.push(vec![
                pad(1),
                Span::styled(format!("{down} to pick one, Enter to use it"), muted),
            ]);
        }
    }

    // recent sessions: a label, then title … age
    if !info.recent.is_empty() {
        out.push(vec![label(theme, "recent")]);
        let room = room.min(RECENT_MAX).saturating_sub(1);
        for (i, (title, age)) in info.recent.iter().take(MAX_RECENT).enumerate() {
            let title = text::one_line(title);
            let age = text::one_line(age);
            let age_w = text::width(&age);
            let mut spans = vec![pad(1), marker(i), Span::raw(" ")];
            if room >= age_w + 8 {
                let title = text::ellipsize(&title, room - age_w - 4, ascii);
                let gap = room.saturating_sub(age_w + text::width(&title) + 2);
                spans.extend([
                    Span::styled(title, body(i)),
                    pad(gap),
                    Span::styled(age, muted),
                ]);
            } else {
                spans.push(Span::styled(title, body(i)));
            }
            out.push(spans);
        }
        let hint = text::one_line(info.resume_hint.trim());
        if !hint.is_empty() {
            out.push(vec![pad(1), Span::styled(hint, muted)]);
        }
    }
    out
}

fn layout(info: &WelcomeInfo, width: usize, theme: &Theme, view: &View, compact: bool) -> Rows {
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
    let framed = width >= CARD_MIN && !ascii && !compact;
    let mut list_placed = false;
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
            let mut spans = fx::sweep(theme, mark, i as f32 * 0.08, view.sweep, Style::default());
            spans.push(pad(4));
            spans.extend(title(i));
            out.push(row(margin, spans, width, &ellipsis));
        }
        out.push(Line::default());

        // Wide terminals put the list beside the card, so the home stays short.
        let two_columns = width >= TWO_COLUMN_MIN && items(info) > 0;
        let card_w = if two_columns {
            ((inner - COLUMN_GAP) * 11 / 20).min(CARD_MAX + 2)
        } else {
            inner.min(CARD_MAX + 2)
        };
        let card_inner = card_w.saturating_sub(2);
        let label_w = 9;
        let value_room = card_inner.saturating_sub(label_w + 3);
        let mut card = vec![card_edge(theme, 0, card_inner, true)];
        for (name, value) in facts(info, theme, value_room) {
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(format!("{name:<label_w$}"), theme.fg(Tok::Dim)),
                Span::raw(" "),
            ];
            spans.extend(value);
            card.push(card_row(theme, 0, card_inner, spans, &ellipsis));
        }
        card.push(card_edge(theme, 0, card_inner, false));
        if two_columns {
            let right_w = inner - card_w - COLUMN_GAP;
            let list = list_lines(info, theme, view, right_w);
            for i in 0..card.len().max(list.len()) {
                let mut spans = vec![pad(margin)];
                match card.get(i) {
                    Some(line) => spans.extend(line.spans.iter().skip(1).cloned()),
                    None => spans.push(pad(card_w)),
                }
                if let Some(line) = list.get(i) {
                    spans.push(pad(COLUMN_GAP));
                    spans.extend(truncate_spans(line.clone(), right_w, &ellipsis));
                }
                out.push(Line::from(spans));
            }
            list_placed = true;
        } else {
            for line in card {
                let mut spans = vec![pad(margin)];
                spans.extend(line.spans.into_iter().skip(1));
                out.push(Line::from(spans));
            }
        }
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
        if !compact {
            out.push(row(
                margin,
                vec![Span::styled(TAGLINE, muted)],
                width,
                &ellipsis,
            ));
        }
        for (name, value) in facts(info, theme, inner.saturating_sub(9)) {
            let mut spans = vec![Span::styled(format!("{name:<8} "), theme.fg(Tok::Dim))];
            spans.extend(value);
            out.push(row(margin, spans, width, &ellipsis));
        }
    }

    if !list_placed {
        let list = list_lines(info, theme, view, inner);
        if !list.is_empty() {
            out.push(Line::default());
        }
        for spans in list {
            out.push(row(margin, spans, width, &ellipsis));
        }
    }

    // key hints, wrapped to the width only when they cannot fit
    if !info.tips.is_empty() && !compact {
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

/// One line that stands for the home once the conversation starts:
/// `◉ dial 0.3.1 · model via provider · project  branch`.
pub fn summary(info: &WelcomeInfo, width: u16, theme: &Theme) -> Rows {
    let width = width as usize;
    if width == 0 {
        return Vec::new();
    }
    let ellipsis = theme.sym(Sym::Ellipsis).to_owned();
    let margin = if width >= 24 { 2 } else { 0 };
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let muted = theme.fg(Tok::Muted);
    let mark = if theme.ascii() { MARK_ASCII } else { MARK };
    let mut spans = vec![
        Span::styled(mark, theme.fg(Tok::Accent)),
        Span::raw(" "),
        Span::styled("dial", theme.fg(Tok::Text).add_modifier(Modifier::BOLD)),
    ];
    let version = text::one_line(info.version.trim());
    if !version.is_empty() {
        spans.push(Span::styled(format!(" {version}"), muted));
    }
    let model = text::one_line(info.model.trim());
    if !model.is_empty() {
        spans.push(Span::styled(dot.clone(), muted));
        spans.push(Span::styled(model, theme.fg(Tok::Accent)));
    }
    let project = text::one_line(info.project.trim());
    if !project.is_empty() {
        spans.push(Span::styled(dot, muted));
        spans.push(Span::styled(project, theme.fg(Tok::Text)));
    }
    let branch = text::one_line(info.branch.trim());
    if !branch.is_empty() {
        spans.push(Span::styled(
            format!("  {branch}"),
            theme.fg(Tok::StatusGitClean),
        ));
    }
    vec![
        Line::default(),
        row(margin, spans, width, &ellipsis),
        Line::default(),
    ]
}

/// What picking a home row asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Reopen this session id.
    Resume(String),
    /// Put this request in the input, ready to edit or send.
    Draft(&'static str),
}

/// The home while it is live in the viewport: selection, and the one start-up sweep.
pub struct Home {
    pub info: WelcomeInfo,
    /// Session ids, parallel to `info.recent`.
    sessions: Vec<String>,
    selected: Option<usize>,
    motion: bool,
    /// Set by the first draw, so the sweep runs on screen rather than during start-up work.
    started: Option<Instant>,
    settled: bool,
}

impl Home {
    pub fn new(info: WelcomeInfo, sessions: Vec<String>, motion: bool) -> Self {
        Home {
            info,
            sessions,
            selected: None,
            motion,
            started: None,
            settled: !motion,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Move the selection down (entering the list from the input); false when there is no list.
    pub fn down(&mut self) -> bool {
        let n = items(&self.info);
        if n == 0 {
            return false;
        }
        self.selected = Some(self.selected.map_or(0, |i| (i + 1).min(n - 1)));
        true
    }

    /// Move the selection up; above the first row it returns to the input. False when nothing is
    /// selected, so the key keeps its usual meaning.
    pub fn up(&mut self) -> bool {
        match self.selected {
            None => false,
            Some(0) => {
                self.selected = None;
                true
            }
            Some(i) => {
                self.selected = Some(i - 1);
                true
            }
        }
    }

    pub fn deselect(&mut self) -> bool {
        self.selected.take().is_some()
    }

    /// The selected row's request, clearing the selection.
    pub fn pick(&mut self) -> Option<Pick> {
        let i = self.selected.take()?;
        if self.info.recent.is_empty() {
            STARTERS.get(i).map(|s| Pick::Draft(s))
        } else {
            self.sessions.get(i).cloned().map(Pick::Resume)
        }
    }

    /// Whether the start-up sweep still needs frames.
    pub fn animating(&self) -> bool {
        !self.settled
    }

    /// The live rows, at most `max_rows`; advances the sweep.
    pub fn rows(&mut self, width: u16, max_rows: usize, theme: &Theme) -> Rows {
        let sweep = if self.motion {
            let started = *self.started.get_or_insert_with(Instant::now);
            fx::progress(started.elapsed(), fx::SWEEP)
        } else {
            1.0
        };
        if sweep >= 1.0 {
            self.settled = true;
        }
        render(
            &self.info,
            width,
            theme,
            View {
                selected: self.selected,
                sweep,
                max_rows,
                live: true,
            },
        )
    }

    pub fn summary(&self, width: u16, theme: &Theme) -> Rows {
        summary(&self.info, width, theme)
    }
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

    fn info_without_recent() -> WelcomeInfo {
        let mut first = info();
        first.recent.clear();
        first
    }

    #[test]
    fn wide_terminals_put_the_list_beside_the_card() {
        let text = plain(&render(
            &info(),
            140,
            &Theme::dark(),
            View {
                live: true,
                ..View::default()
            },
        ));
        let beside = text
            .iter()
            .find(|l| l.contains("fix the parser"))
            .expect("recent row");
        assert!(
            beside.contains('\u{2502}'),
            "list shares rows with the card: {text:?}"
        );
        let narrow = plain(&rows(&info(), 100, &Theme::dark()));
        let alone = narrow
            .iter()
            .find(|l| l.contains("fix the parser"))
            .unwrap();
        assert!(!alone.contains('\u{2502}'), "{narrow:?}");
        for width in 1..=220u16 {
            for row in render(&info(), width, &Theme::dark(), View::default()) {
                assert!(text::line_width(&row) <= width as usize, "width {width}");
            }
        }
    }

    #[test]
    fn the_home_selects_rows_and_picks_sessions_or_starters() {
        let mut home = Home::new(info(), vec!["s1".into(), "s2".into()], false);
        assert!(!home.up(), "nothing selected: Up keeps its history meaning");
        assert!(home.down() && home.down() && home.down());
        assert_eq!(home.selected(), Some(1), "stops at the last row");
        let theme = Theme::dark();
        let pointer = theme.sym(Sym::Pointer);
        let shown = plain(&home.rows(140, 40, &theme)).join("\n");
        assert!(
            shown.contains(&format!("{pointer} add welcome banner")),
            "{shown}"
        );
        assert_eq!(home.pick(), Some(Pick::Resume("s2".into())));
        assert_eq!(home.selected(), None);
        assert!(home.down() && home.up() && home.selected().is_none());

        let mut first = Home::new(info_without_recent(), Vec::new(), false);
        assert!(first.down());
        assert_eq!(first.pick(), Some(Pick::Draft(STARTERS[0])));
        assert!(
            plain(&first.rows(100, 40, &theme))
                .join("\n")
                .contains("to pick one, Enter to use it")
        );
        let mut bare = info_without_recent();
        bare.tips.clear();
        assert!(
            !Home::new(bare, Vec::new(), false).down(),
            "no list, no selection"
        );
    }

    #[test]
    fn short_terminals_get_the_compact_home_within_the_rows_given() {
        let theme = Theme::dark();
        let mut home = Home::new(info(), vec!["s1".into(), "s2".into()], false);
        let full = home.rows(100, usize::MAX, &theme);
        let short = home.rows(100, 12, &theme);
        assert!(
            short.len() <= 12 && full.len() > 12,
            "{} {}",
            full.len(),
            short.len()
        );
        let text = plain(&short).join("\n");
        assert!(!text.contains(WORDMARK[0]), "{text}");
        assert!(
            text.contains("dial 0.3.1") && text.contains("fix the parser"),
            "{text}"
        );
    }

    #[test]
    fn the_sweep_brightens_the_wordmark_once_then_rests_on_the_gradient() {
        let theme = Theme::builtin("dial-dark").unwrap();
        let at = |sweep: f32| {
            render(
                &info(),
                100,
                &theme,
                View {
                    sweep,
                    ..View::default()
                },
            )
        };
        assert_ne!(at(0.4)[1], at(1.0)[1], "mid-sweep differs");
        assert_eq!(
            at(1.0),
            rows(&info(), 100, &theme),
            "rest is the plain banner"
        );
        let mut still = Home::new(info(), Vec::new(), false);
        assert!(!still.animating());
        let mut moving = Home::new(info(), Vec::new(), true);
        assert!(moving.animating());
        moving.rows(100, 40, &theme);
        std::thread::sleep(fx::SWEEP);
        moving.rows(100, 40, &theme);
        assert!(!moving.animating(), "one sweep, then still");
        still.rows(100, 40, &theme);
    }

    #[test]
    fn the_home_collapses_to_one_summary_line() {
        let mut with_branch = info();
        with_branch.branch = "main".into();
        let summary = plain(&summary(&with_branch, 100, &Theme::dark()));
        let lines: Vec<&String> = summary.iter().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "{summary:?}");
        assert!(
            lines[0].contains("dial 0.3.1 · qwen3.5-9b · dial  main"),
            "{summary:?}"
        );
        for width in 1..=120u16 {
            for row in super::summary(&with_branch, width, &Theme::dark()) {
                assert!(text::line_width(&row) <= width as usize);
            }
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
