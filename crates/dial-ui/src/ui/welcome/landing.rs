//! The live landing composition. The input stays near the window's midpoint; artwork yields
//! before actions or safety context when the terminal is short. Scrollback uses the banner API.
use super::*;
use crate::ui::composer::ComposerView;

/// A readable measure, with breathing room on either side of the launch composer.
pub fn content_width(width: u16) -> u16 {
    width
        .saturating_sub(if width >= 68 {
            16
        } else if width >= 44 {
            8
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

fn hero(width: usize, budget: usize, theme: &Theme, sweep: f32) -> Rows {
    if budget < 2 {
        return Vec::new();
    }
    let art = if theme.ascii() {
        None
    } else if budget > mark::LOCKUP.1 && width >= mark::LOCKUP.0 {
        Some(mark::lockup(theme, sweep))
    } else if budget > mark::WORDMARK.1 && width >= mark::WORDMARK.0 {
        Some(mark::wordmark(theme, sweep))
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
    if budget > rows.len() + 2 {
        rows.push(Line::default());
        rows.push(centered(
            vec![Span::styled("What are we building?", theme.fg(Tok::Muted))],
            width,
            theme,
        ));
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
    // Only exceptional setup/safety context is visible at rest. Session choices are revealed
    // by the existing Down-key navigation instead of competing with the input.
    let mut notices = Vec::new();
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
                text::one_line(&info.approval),
                text::one_line(&info.sandbox)
            )
            .replace('·', if theme.ascii() { ":" } else { "·" }),
            theme.fg(Tok::Warning),
        )]);
    }
    let place = place(info, theme, inner);
    let reserve =
        (usize::from(!place.is_empty()) + notices.len() + 2 + usize::from(view.selected.is_some()))
            .min(remaining);
    let above = (height / 2)
        .saturating_sub(composer.cursor.0 as usize)
        .min(remaining.saturating_sub(reserve));
    let hero = hero(inner, above, theme, view.sweep);
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
    for spans in notices
        .into_iter()
        .take(budget.saturating_sub(context.len()))
    {
        context.push(centered(spans, inner, theme));
    }
    let limit = budget.saturating_sub(context.len());
    if view.selected.is_some() {
        context.extend(
            list_lines(info, theme, &view, inner, limit)
                .into_iter()
                .map(|spans| centered(spans, inner, theme)),
        );
    } else if limit > 0 {
        let browse = if items(info) > 0 {
            if theme.ascii() {
                "  Down browse"
            } else {
                "  ↓ browse"
            }
        } else {
            ""
        };
        context.push(centered(
            vec![Span::styled(
                format!("Enter send  / commands{browse}"),
                theme.fg(Tok::Dim),
            )],
            inner,
            theme,
        ));
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
    let keep = room.saturating_sub(text::width(&sep) + text::width(&branch));
    if keep >= 6 {
        spans.push(Span::styled(sep, theme.fg(Tok::Dim)));
        spans.push(Span::styled(
            text::ellipsize_start(&project, keep, ascii),
            theme.fg(Tok::Muted),
        ));
        spans.push(Span::styled(branch, theme.fg(Tok::StatusGitClean)));
    }
    spans
}

#[cfg(test)]
mod tests;
