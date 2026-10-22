//! The home screen: a block wordmark over a dotted axis and its faint reflection, with the name,
//! version and tagline on its baseline; borderless facts that say what dial is ready to work with
//! (model, project and branch, approval and sandbox); starter requests on a first visit or the
//! recent sessions on a return; and the key hints. Every row fits the width it is given. Short
//! terminals drop the reflection, then swap in a two-row wordmark; below `ART_MIN` columns, or with
//! the ASCII glyph set, the art drops and the same facts stay as plain rows.
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

mod landing;
mod mark;
pub use landing::{content_width, frame};

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
/// Narrowest width that gets the wordmark.
const ART_MIN: usize = 56;
/// The facts column never grows past this, so values stay near their labels on wide terminals.
const FACTS_MAX: usize = 92;
/// Narrowest width that puts the list beside the facts instead of under them.
const TWO_COLUMN_MIN: usize = 110;
/// Cells between the facts and the list in two columns.
const COLUMN_GAP: usize = 4;
/// "dial" in full blocks, six rows tall; the last two rows carry the name and tagline beside it.
const WORDMARK: [&str; 6] = [
    "      \u{2588}\u{2588}  \u{2588}\u{2588}            \u{2588}\u{2588}",
    "      \u{2588}\u{2588}                \u{2588}\u{2588}",
    " \u{2584}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}  \u{2588}\u{2588}   \u{2584}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}  \u{2588}\u{2588}",
    "\u{2588}\u{2588}    \u{2588}\u{2588}  \u{2588}\u{2588}  \u{2588}\u{2588}    \u{2588}\u{2588}  \u{2588}\u{2588}",
    "\u{2588}\u{2588}    \u{2588}\u{2588}  \u{2588}\u{2588}  \u{2588}\u{2588}    \u{2588}\u{2588}  \u{2588}\u{2588}",
    " \u{2580}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}  \u{2588}\u{2588}   \u{2580}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}  \u{2588}\u{2588}",
];
/// The wordmark's last rows seen in still water: the first bright, the second broken up and faint.
const REFLECTION: [&str; 2] = [
    " \u{2584}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}  \u{2588}\u{2588}   \u{2584}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}  \u{2588}\u{2588}",
    "\u{2588}\u{2580}    \u{2588}\u{2580}  \u{2588}\u{2580}  \u{2588}\u{2580}    \u{2588}\u{2580}  \u{2588}\u{2580}",
];
/// "dial" in half-block pixels, two rows tall, for terminals too short for the full wordmark.
const WORDMARK_SMALL: [&str; 2] = [
    "\u{2584}\u{2584}\u{2588} \u{2580} \u{2584}\u{2584}\u{2584}  \u{2588} ",
    "\u{2588}\u{2584}\u{2588} \u{2588} \u{2588}\u{2584}\u{2588}\u{2584} \u{2588}\u{2584}",
];

/// How much artwork the home draws, largest first; [`render`] takes the first that fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Art {
    Reflected,
    Large,
    Small,
}

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

/// Whether a fact row is a warning the user should act on (rather than reassurance).
fn fact_warns(info: &WelcomeInfo, name: &str) -> bool {
    match name {
        "model" | "next" => info.model.trim().is_empty(),
        "approval" => info.approval.trim() == "auto",
        "sandbox" => info.sandbox.trim().eq_ignore_ascii_case("off"),
        _ => false,
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
    if info.model.trim().is_empty() {
        let hint = text::one_line(info.setup_hint.trim());
        if !hint.is_empty() {
            out.push(("next", vec![Span::styled(hint, theme.fg(Tok::Accent))]));
        }
    }

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
        out.push(("approval", spans));
    }
    // Isolation is a separate fact, not a tail that disappears behind a long approval sentence.
    let sandbox = text::one_line(info.sandbox.trim());
    if !sandbox.is_empty() {
        let unguarded = approval == "auto" && sandbox.eq_ignore_ascii_case("off");
        let style = if unguarded {
            theme.fg(Tok::Warning)
        } else {
            muted
        };
        let mut spans = vec![Span::styled(sandbox, style.add_modifier(Modifier::BOLD))];
        if unguarded {
            spans.push(Span::styled(format!("{dot}tools are not isolated"), style));
        }
        out.push(("sandbox", spans));
    }
    out
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

/// Rows for `view`: the largest layout that fits `view.max_rows`, else the compact one, cut to fit.
pub fn render(info: &WelcomeInfo, width: u16, theme: &Theme, view: View) -> Rows {
    for art in [Art::Reflected, Art::Large, Art::Small] {
        let out = layout(info, width as usize, theme, &view, false, art);
        if out.len() <= view.max_rows {
            return out;
        }
    }
    let mut out = layout(info, width as usize, theme, &view, true, Art::Small);
    out.truncate(view.max_rows);
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
    limit: usize,
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
    if limit == 0 {
        return out;
    }
    let total = items(info);
    let shown = total.min(limit.saturating_sub(2).max(1));
    let start = view
        .selected
        .unwrap_or(0)
        .saturating_sub(shown.saturating_sub(1))
        .min(total.saturating_sub(shown));
    let heading = |title: &str| {
        let mut spans = vec![label(theme, title)];
        if shown < total {
            let range = if shown == 1 {
                format!("  {}/{total}", start + 1)
            } else {
                format!("  {}-{}/{total}", start + 1, start + shown)
            };
            spans.push(Span::styled(range, muted));
        }
        spans
    };

    // first visit to this project: things to try
    if info.recent.is_empty() && !info.tips.is_empty() {
        if limit > 1 {
            out.push(heading("start with"));
        }
        for (i, starter) in STARTERS.iter().enumerate().skip(start).take(shown) {
            // Bullets, not numbers: digits typed into the composer are text, not a starter pick.
            out.push(vec![
                pad(1),
                marker(i),
                Span::styled(format!("  {starter}"), body(i)),
            ]);
        }
        if view.live && out.len() < limit {
            let down = if ascii { "Down" } else { "\u{2193}" };
            out.push(vec![
                pad(1),
                Span::styled(format!("{down} to pick one, Enter to edit it"), muted),
            ]);
        }
    }

    // recent sessions: a label, then title … age
    if !info.recent.is_empty() {
        if limit > 1 {
            out.push(heading("recent"));
        }
        let room = room.min(RECENT_MAX).saturating_sub(1);
        for (i, (title, age)) in info.recent.iter().enumerate().skip(start).take(shown) {
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
        if !hint.is_empty() && out.len() < limit {
            out.push(vec![pad(1), Span::styled(hint, muted)]);
        }
    }
    out
}

fn layout(
    info: &WelcomeInfo,
    width: usize,
    theme: &Theme,
    view: &View,
    compact: bool,
    art: Art,
) -> Rows {
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

    let mut out: Rows = if compact {
        Vec::new()
    } else {
        vec![Line::default()]
    };
    let framed = width >= ART_MIN && !ascii && !compact;
    let mut list_placed = false;
    if framed {
        // Wordmark on the left; the name and tagline sit beside its last two rows.
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
        let marks: &[&str] = if art == Art::Small {
            &WORDMARK_SMALL
        } else {
            &WORDMARK
        };
        let side = marks.len() - 2;
        for (i, mark) in marks.iter().enumerate() {
            let mut spans = fx::sweep(theme, mark, i as f32 * 0.08, view.sweep, Style::default());
            if i >= side {
                spans.push(pad(4));
                spans.extend(title(i - side));
            }
            out.push(row(margin, spans, width, &ellipsis));
        }
        if art == Art::Reflected {
            let art_w = text::width(WORDMARK[0]);
            out.push(row(
                margin,
                vec![Span::styled(
                    "\u{2508}".repeat(art_w),
                    theme.fg(Tok::BorderMuted),
                )],
                width,
                &ellipsis,
            ));
            for (mark, tok) in REFLECTION.iter().zip([Tok::Dim, Tok::Border]) {
                out.push(row(
                    margin,
                    vec![Span::styled(*mark, theme.fg(tok))],
                    width,
                    &ellipsis,
                ));
            }
        }
        out.push(Line::default());

        // The facts stand free: a dim label column, then the value. Wide terminals put the list
        // beside them, so the home stays short.
        let two_columns = width >= TWO_COLUMN_MIN && items(info) > 0;
        let facts_w = if two_columns {
            ((inner - COLUMN_GAP) * 11 / 20).min(FACTS_MAX)
        } else {
            inner.min(FACTS_MAX)
        };
        let label_w = 9;
        let fact_rows: Vec<Vec<Span<'static>>> =
            facts(info, theme, facts_w.saturating_sub(label_w + 1))
                .into_iter()
                .map(|(name, value)| {
                    let mut spans = vec![
                        Span::styled(format!("{name:<label_w$}"), theme.fg(Tok::Dim)),
                        Span::raw(" "),
                    ];
                    spans.extend(value);
                    truncate_spans(spans, facts_w, &ellipsis)
                })
                .collect();
        if two_columns {
            let right_w = inner - facts_w - COLUMN_GAP;
            let list = list_lines(info, theme, view, right_w, usize::MAX);
            for i in 0..fact_rows.len().max(list.len()) {
                let mut spans = vec![pad(margin)];
                let used = match fact_rows.get(i) {
                    Some(line) => {
                        spans.extend(line.iter().cloned());
                        line.iter().map(|s| text::width(&s.content)).sum()
                    }
                    None => 0,
                };
                if let Some(line) = list.get(i) {
                    spans.push(pad(facts_w.saturating_sub(used) + COLUMN_GAP));
                    spans.extend(truncate_spans(line.clone(), right_w, &ellipsis));
                }
                out.push(Line::from(spans));
            }
            list_placed = true;
        } else {
            for spans in fact_rows {
                out.push(row(margin, spans, width, &ellipsis));
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
        if !compact || view.max_rows > usize::from(items(info) > 0) {
            out.push(row(margin, head, width, &ellipsis));
        }
        if !compact {
            out.push(row(
                margin,
                vec![Span::styled(TAGLINE, muted)],
                width,
                &ellipsis,
            ));
        }
        let mut facts = facts(info, theme, inner.saturating_sub(9));
        if compact {
            // Keep a selected action visible even in very short terminals. When every fact cannot
            // fit, the reassuring ones (a safe sandbox, a guarded approval mode, the model and
            // project) leave first and whole, so only warnings compete with the list.
            let room = view
                .max_rows
                .saturating_sub(out.len() + usize::from(items(info) > 0));
            if facts.len() > room {
                facts.retain(|(name, _)| fact_warns(info, name));
            }
            while facts.len() > room {
                let remove = ["next", "model", "approval"]
                    .iter()
                    .find_map(|name| facts.iter().position(|(label, _)| label == name))
                    .unwrap_or(0);
                facts.remove(remove);
            }
        }
        for (name, value) in facts {
            let mut spans = vec![Span::styled(format!("{name:<8} "), theme.fg(Tok::Dim))];
            spans.extend(value);
            out.push(row(margin, spans, width, &ellipsis));
        }
    }

    if !list_placed {
        let limit = if compact {
            view.max_rows.saturating_sub(out.len())
        } else {
            usize::MAX
        };
        let list = list_lines(info, theme, view, inner, limit);
        if !list.is_empty() && !compact {
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

    if !compact {
        out.push(Line::default());
    }
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

    /// Compose the centered launch screen with the live input and auxiliary rows.
    pub fn frame(
        &mut self,
        width: u16,
        height: u16,
        theme: &Theme,
        composer: super::composer::ComposerView,
        auxiliary: Rows,
    ) -> super::composer::ComposerView {
        let sweep = self.sweep();
        frame(
            &self.info,
            width,
            height,
            theme,
            View {
                selected: self.selected,
                sweep,
                max_rows: height as usize,
                live: true,
            },
            composer,
            auxiliary,
        )
    }

    fn sweep(&mut self) -> f32 {
        let sweep = if self.motion {
            let started = *self.started.get_or_insert_with(Instant::now);
            fx::progress(started.elapsed(), fx::TURN)
        } else {
            1.0
        };
        if sweep >= 1.0 {
            self.settled = true;
        }
        sweep
    }

    /// The live banner rows, at most `max_rows`; advances the sweep.
    pub fn rows(&mut self, width: u16, max_rows: usize, theme: &Theme) -> Rows {
        let sweep = self.sweep();
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
mod tests;
