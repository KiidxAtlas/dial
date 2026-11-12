//! The live landing composition. The input stays near the window's midpoint; artwork yields
//! before actions or safety context when the terminal is short. Below the input, the context
//! stands in rounded cards drawn with the transcript's card frame, so the launch screen and the
//! conversation share one visual language: warnings in a warning-outlined card, the recent
//! sessions or starters in a bordered list, first-run steps in an accent card. Each card falls
//! back to plain centered rows when the window cannot spare its two edge rows. Scrollback uses
//! the banner API.
use super::*;
use crate::ui::composer::ComposerView;
use crate::ui::tool_cards::frame::{CHROME_W, Frame};

/// Narrowest content width that draws its context as cards.
const CARD_MIN: usize = 24;

/// A readable measure, with breathing room on either side of the launch composer. Even a tiny
/// terminal keeps a one-cell margin, so the input card never sits flush against the window edge.
pub fn content_width(width: u16) -> u16 {
    width
        .saturating_sub(if width >= 68 {
            16
        } else if width >= 44 {
            8
        } else if width >= 24 {
            2
        } else {
            0
        })
        .min(56)
}

fn centered(spans: Vec<Span<'static>>, width: usize, theme: &Theme) -> Line<'static> {
    let spans = truncate_spans(spans, width, theme.sym(Sym::Ellipsis));
    let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    let mut out = vec![Span::raw(" ".repeat(width.saturating_sub(used) / 2))];
    out.extend(spans);
    Line::from(out)
}

/// A rounded card exactly `width` cells wide, or `None` when the width cannot hold one.
fn card(
    theme: &Theme,
    head: Vec<Span<'static>>,
    body: Vec<Vec<Span<'static>>>,
    foot: Option<Vec<Span<'static>>>,
    border: Tok,
    width: usize,
) -> Option<Rows> {
    if width < CARD_MIN {
        return None;
    }
    let mut frame = Frame::new(head, theme.fg(border));
    frame.sections.push((
        None,
        body.into_iter()
            .map(|row| Line::from(trim_lead(row)))
            .collect(),
    ));
    if let Some(foot) = foot {
        frame.foot = trim_lead(foot);
    }
    Some(frame.rows(theme, width))
}

/// Spans without the blank padding spans that lead them (a card edge brings its own spacing).
fn trim_lead(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .skip_while(|s| s.content.trim().is_empty())
        .collect()
}

/// `v0.4.2 · Your coding agent, wired into this project.`, typed in while the mark powers up.
/// The row keeps its full width at every frame, so the centered text never shifts.
fn tagline(info: &WelcomeInfo, theme: &Theme, sweep: f32) -> Vec<Span<'static>> {
    let version = text::one_line(info.version.trim());
    let mut parts: Vec<(String, Style)> = Vec::new();
    if !version.is_empty() {
        let version = if version.starts_with(|c: char| c.is_ascii_digit()) {
            format!("v{version}")
        } else {
            version
        };
        parts.push((version, theme.fg(Tok::Dim)));
        parts.push((format!(" {} ", theme.sym(Sym::Dot)), theme.fg(Tok::Dim)));
    }
    parts.push((TAGLINE.to_owned(), theme.fg(Tok::Muted)));
    let total: usize = parts.iter().map(|(s, _)| s.chars().count()).sum();
    let mut visible = if (0.0..1.0).contains(&sweep) {
        ((sweep * 1.6).min(1.0) * total as f32).round() as usize
    } else {
        total
    };
    let mut spans = Vec::new();
    for (part, style) in parts {
        let n = part.chars().count();
        let shown: String = part.chars().take(visible).collect();
        let hidden = n.saturating_sub(visible);
        visible = visible.saturating_sub(n);
        if !shown.is_empty() {
            spans.push(Span::styled(shown, style));
        }
        if hidden > 0 {
            spans.push(Span::raw(" ".repeat(hidden)));
        }
    }
    spans
}

fn hero(info: &WelcomeInfo, width: usize, budget: usize, theme: &Theme, view: &View) -> Rows {
    if budget < 2 {
        return Vec::new();
    }
    let (sweep, idle) = (view.sweep, view.idle);
    let art = if theme.ascii() {
        None
    } else if budget > mark::LOCKUP.1 && width >= mark::LOCKUP.0 {
        Some(mark::lockup(theme, sweep, idle))
    } else if budget > mark::WORDMARK.1 && width >= mark::WORDMARK.0 {
        Some(mark::wordmark(theme, sweep, idle))
    } else {
        None
    };
    let mut rows: Rows = match art {
        Some(art) => art
            .into_iter()
            .map(|spans| centered(spans, width, theme))
            .collect(),
        None => vec![centered(
            vec![Span::styled(
                "d i a l",
                theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
            )],
            width,
            theme,
        )],
    };
    // The tagline sits under the mark when the window can spare the row and its whole width.
    let line = tagline(info, theme, sweep);
    let line_w: usize = line.iter().map(|s| text::width(&s.content)).sum();
    if budget >= rows.len() + 2 && line_w <= width {
        rows.push(centered(line, width, theme));
    }
    rows.push(Line::default());
    rows
}

/// Assemble the actual launch viewport, including input cursor translation. Callers render the
/// composer at [`content_width`] first. Auxiliary work rows keep their full terminal width.
pub fn frame(
    info: &WelcomeInfo,
    width: u16,
    height: u16,
    theme: &Theme,
    view: View,
    composer: ComposerView,
    auxiliary: Rows,
) -> ComposerView {
    let width = width as usize;
    let height = height as usize;
    let inner = content_width(width as u16) as usize;
    let inset = (width - inner) / 2;
    let remaining = height.saturating_sub(composer.rows.len() + auxiliary.len());
    // Safety context is reserved first; recent sessions use only spare space below it.
    let mut notices = Vec::new();
    if info.doctor_issues > 0 {
        notices.push(vec![Span::styled(
            format!("{} issues · /doctor", info.doctor_issues),
            theme.fg(Tok::Warning),
        )]);
    }
    if info.model.trim().is_empty() {
        let hint = text::one_line(&info.setup_hint);
        notices.push(vec![Span::styled(
            format!("no model selected  {hint}"),
            theme.fg(Tok::Warning),
        )]);
    }
    if info.approval == "auto" || info.sandbox.eq_ignore_ascii_case("off") {
        notices.push(vec![Span::styled(
            format!(
                "approval {} · sandbox {}",
                text::one_line(crate::ui::status::approval_label(&info.approval)),
                text::one_line(&info.sandbox)
            )
            .replace('·', if theme.ascii() { ":" } else { "·" }),
            theme.fg(Tok::Warning),
        )]);
    }
    let place = place(info, theme, inner);
    // Warnings keep room for their card's two edges, so a tall window never trades them away.
    let chrome = if notices.is_empty() { 0 } else { 2 };
    let reserve = (usize::from(!place.is_empty())
        + notices.len()
        + chrome
        + 2
        + usize::from(view.selected.is_some()))
    .min(remaining);
    let above = (height / 2)
        .saturating_sub(composer.cursor.0 as usize)
        .min(remaining.saturating_sub(reserve));
    let hero = hero(info, inner, above, theme, &view);
    let mut rows = vec![Line::default(); above.saturating_sub(hero.len())];
    let indent = |mut line: Line<'static>| {
        line.spans.insert(0, Span::raw(" ".repeat(inset)));
        line
    };
    rows.extend(hero.into_iter().map(&indent));
    let cursor = (
        rows.len() as u16 + composer.cursor.0,
        inset as u16 + composer.cursor.1,
    );
    rows.extend(composer.rows.into_iter().map(&indent));

    let budget = height.saturating_sub(rows.len() + auxiliary.len());
    let mut context = Vec::new();
    if !place.is_empty() && budget > 0 {
        context.push(centered(place, inner, theme));
    }
    if budget > context.len() {
        context.push(Line::default());
    }

    // Warnings: one card outlined in the warning color, else plain rows.
    let left = budget.saturating_sub(context.len());
    let warned = !notices.is_empty();
    if warned {
        let head = vec![Span::styled(
            format!("{} heads up", theme.sym(Sym::Warn)),
            theme.fg(Tok::Warning).add_modifier(Modifier::BOLD),
        )];
        match (left >= notices.len() + 2)
            .then(|| card(theme, head, notices.clone(), None, Tok::Warning, inner))
            .flatten()
        {
            Some(card) => context.extend(card),
            None => context.extend(
                notices
                    .into_iter()
                    .take(left)
                    .map(|spans| centered(spans, inner, theme)),
            ),
        }
    }

    // The key hint stays whenever there is a row for it, so the way to send never disappears
    // behind a list. `↓ browse` is offered only while the list is not already on screen.
    let mut limit = budget.saturating_sub(context.len());
    // At rest, a first visit to the project shows its starters, as long as the first-run steps
    // do not need the room and the card (two edges, a heading edge and its rows) fits whole.
    let starters = info.recent.is_empty()
        && !info.tour
        && view.selected.is_none()
        && limit >= STARTERS.len() + 2 + 2 + usize::from(warned);
    let wants_list = view.selected.is_some() || (!info.recent.is_empty() && limit >= 4) || starters;
    if view.selected.is_none() && limit > 0 {
        let browse = if items(info) > 0 && !wants_list {
            if theme.ascii() {
                "  Down browse"
            } else {
                "  ↓ browse"
            }
        } else {
            ""
        };
        // How Dial will act on the first request, and the key that changes it, before it is sent.
        let mode = if info.approval.is_empty() || info.approval_key.is_empty() {
            String::new()
        } else {
            format!(
                "  {} mode: {}",
                text::one_line(&info.approval_key),
                crate::ui::status::approval_label(&info.approval)
            )
        };
        let full = format!("Enter send  / commands{browse}{mode}");
        let hint = if text::width(&full) <= inner {
            full
        } else {
            format!("Enter send  / commands{browse}")
        };
        context.push(centered(
            vec![Span::styled(hint, theme.fg(Tok::Dim))],
            inner,
            theme,
        ));
        limit -= 1;
    }
    if wants_list && limit >= 6 && (warned || view.selected.is_none()) {
        context.push(Line::default());
        limit -= 1;
    }
    // The recent sessions or starters: a bordered list card whose edges carry its heading and hint.
    if wants_list {
        let framed = (limit >= 3)
            .then(|| {
                let parts = list_parts(
                    info,
                    theme,
                    &view,
                    inner.saturating_sub(CHROME_W),
                    limit - 2,
                );
                card(
                    theme,
                    parts.heading,
                    parts.rows,
                    parts.hint,
                    Tok::Border,
                    inner,
                )
            })
            .flatten();
        match framed {
            Some(card) => context.extend(card),
            None => context.extend(
                list_lines(info, theme, &view, inner, limit)
                    .into_iter()
                    .map(|spans| centered(spans, inner, theme)),
            ),
        }
    }
    let left = budget.saturating_sub(context.len());
    if info.tour && view.selected.is_none() && left >= 4 {
        let hints = [
            ("1", "Type a request in your own words"),
            ("2", "Enter sends · F1 opens actions"),
            ("3", "/models picks a model · Esc dismisses"),
        ]
        .map(|(n, hint)| (n, hint.replace('·', theme.sym(Sym::Dot))));
        let steps: Vec<Vec<Span<'static>>> = hints
            .iter()
            .map(|(n, hint)| {
                vec![
                    Span::styled(
                        format!("{n}  "),
                        theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(hint.clone(), theme.fg(Tok::Text)),
                ]
            })
            .collect();
        let head = vec![Span::styled(
            "first steps",
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        )];
        let framed = (left >= hints.len() + 3)
            .then(|| card(theme, head, steps, None, Tok::BorderAccent, inner))
            .flatten();
        match framed {
            Some(card) => {
                context.push(Line::default());
                context.extend(card);
            }
            None => {
                // One left-aligned block centered as a whole, so the step numbers form a column.
                let lines: Vec<String> = hints
                    .iter()
                    .map(|(n, hint)| format!("{n}  {hint}"))
                    .collect();
                let block = lines.iter().map(|l| text::width(l)).max().unwrap_or(0);
                for line in lines {
                    let pad = block.saturating_sub(text::width(&line));
                    context.push(centered(
                        vec![Span::styled(
                            format!("{line}{}", " ".repeat(pad)),
                            theme.fg(Tok::Muted),
                        )],
                        inner,
                        theme,
                    ));
                }
            }
        }
    }
    rows.extend(context.into_iter().map(indent));
    rows.extend(auxiliary);
    // Occupy the launch viewport so the graphic and input are centered in the window, rather
    // than merely centered inside a short block at the top of the terminal.
    rows.resize(rows.len().max(height), Line::default());
    ComposerView { rows, cursor }
}

/// What will answer and where it will work: the model, then the current directory and branch.
/// The directory keeps its informative tail when the line is short.
fn place(info: &WelcomeInfo, theme: &Theme, width: usize) -> Vec<Span<'static>> {
    let ascii = theme.ascii();
    let model = text::one_line(info.model.trim());
    let project = text::one_line(info.project.trim());
    let branch = text::one_line(info.branch.trim());
    let mut spans = Vec::new();
    let mut room = width;
    if !model.is_empty() {
        let model = text::ellipsize(&model, (width / 2).max(8), ascii);
        room = room.saturating_sub(text::width(&model));
        spans.push(Span::styled(model, theme.fg(Tok::Accent)));
    }
    if project.is_empty() {
        return spans;
    }
    let sep = if spans.is_empty() {
        String::new()
    } else {
        format!(" {} ", theme.sym(Sym::Dot))
    };
    let branch = if branch.is_empty() {
        String::new()
    } else {
        let icon = theme.sym(Sym::IconGit);
        let icon = if icon.is_empty() { "on" } else { icon };
        format!("  {icon} {branch}")
    };
    // The tail of the path names the project. When the whole path does not fit, its last
    // component alone reads better than a path cut mid-word; the branch goes before that does.
    let fits = |project: &str, branch: &str| {
        text::width(&sep) + text::width(project) + text::width(branch) <= room
    };
    let last = project
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(&project)
        .to_owned();
    let (project, branch) = if fits(&project, &branch) {
        (project, branch)
    } else if fits(&last, &branch) {
        (last, branch)
    } else if fits(&last, "") {
        (last, String::new())
    } else {
        let keep = room.saturating_sub(text::width(&sep));
        if keep < 6 {
            return spans;
        }
        (text::ellipsize_start(&last, keep, ascii), String::new())
    };
    spans.push(Span::styled(sep, theme.fg(Tok::Dim)));
    spans.push(Span::styled(project, theme.fg(Tok::Muted)));
    if !branch.is_empty() {
        spans.push(Span::styled(branch, theme.fg(Tok::StatusGitClean)));
    }
    spans
}

#[cfg(test)]
mod tests;
