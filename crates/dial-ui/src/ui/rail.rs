//! Bounded session rows. Inputs are snapshots, never formatted tool-result strings.
use super::{
    Rows,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    #[serde(default)]
    pub untracked: bool,
}

pub fn session(
    id: &str,
    title: &str,
    files: &[FileDiff],
    width: usize,
    max: usize,
    theme: &Theme,
) -> Rows {
    if width < 24 || max == 0 {
        return Vec::new();
    }
    let fit = |spans| Line::from(truncate_spans(spans, width, theme.sym(Sym::Ellipsis)));
    let label = if title.trim().is_empty() {
        id.get(..8).unwrap_or(id)
    } else {
        title
    };
    let mut rows = vec![fit(vec![
        Span::styled("workspace ", theme.fg(Tok::Dim)),
        Span::styled(
            text::ellipsize(
                &text::one_line(label),
                width.saturating_sub(30),
                theme.ascii(),
            ),
            theme.fg(Tok::Muted),
        ),
        Span::styled(
            if files.is_empty() {
                format!(" {} no changes", theme.sym(Sym::Dot))
            } else {
                format!(
                    " {} {} changed file{}",
                    theme.sym(Sym::Dot),
                    files.len(),
                    if files.len() == 1 { "" } else { "s" }
                )
            },
            theme.fg(if files.is_empty() {
                Tok::Dim
            } else {
                Tok::Muted
            }),
        ),
    ])];
    for file in files.iter().take(max.saturating_sub(1)) {
        let stats = match (file.added, file.removed) {
            _ if file.untracked => "new".into(),
            (Some(a), Some(r)) => format!("+{a} −{r}"),
            _ => "binary".into(),
        };
        let room = width.saturating_sub(text::width(&stats) + 4);
        let path = text::ellipsize_start(&text::one_line(&file.path), room, theme.ascii());
        let padding = width
            .saturating_sub(text::width(&path) + text::width(&stats) + 2)
            .max(2);
        rows.push(fit(vec![
            Span::styled(format!("  {path}"), theme.fg(Tok::Muted)),
            Span::styled(
                format!("{}{stats}", " ".repeat(padding)),
                theme.fg(if file.untracked {
                    Tok::Accent
                } else {
                    Tok::Success
                }),
            ),
        ]));
    }
    if files.len() > max.saturating_sub(1) && max > 1 {
        rows.pop();
        rows.push(fit(vec![Span::styled(
            format!(
                "  {} {} more files · /changes",
                theme.sym(Sym::Ellipsis),
                files.len().saturating_sub(max.saturating_sub(2))
            ),
            theme.fg(Tok::Dim),
        )]));
    }
    rows
}

#[cfg(test)]
mod tests;
