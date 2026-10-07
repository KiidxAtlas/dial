//! Bounded session and worker rows. Inputs are snapshots, never formatted tool-result strings.
use super::{
    Rows,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use crate::runtime::{Node, Phase};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
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
        Span::styled(
            "Session ",
            theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            text::ellipsize(
                &text::one_line(label),
                width.saturating_sub(30),
                theme.ascii(),
            ),
            theme.fg(Tok::Muted),
        ),
        Span::styled(
            format!(
                " · {} changed file{}",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
            theme.fg(Tok::Dim),
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
                theme.fg(Tok::Dim),
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

pub fn workers(nodes: &[Node], width: usize, max: usize, tick: u64, theme: &Theme) -> Rows {
    if width < 24 || max < 2 || nodes.is_empty() {
        return Vec::new();
    }
    let mut nodes: Vec<_> = nodes.iter().collect();
    nodes.sort_by_key(|n| {
        (
            n.result.phase != Phase::Running,
            std::cmp::Reverse(
                n.id.strip_prefix("runtime-")
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0),
            ),
        )
    });
    let active = nodes
        .iter()
        .filter(|n| n.result.phase == Phase::Running)
        .count();
    // Settled cards remain for the current turn; running cards always come first.
    let fit = |spans| Line::from(truncate_spans(spans, width, theme.sym(Sym::Ellipsis)));
    let mut rows = vec![fit(vec![
        Span::styled(
            "Workers",
            theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {active} active"), theme.fg(Tok::Dim)),
    ])];
    for node in nodes.iter().take(max - 1) {
        let data = node.result.data.as_ref();
        let phase = data.and_then(|d| d["phase"].as_str()).unwrap_or("working");
        let steps = data.and_then(|d| d["steps"].as_u64()).unwrap_or(0);
        let (glyph, tok, status) = match node.result.phase {
            Phase::Running => (theme.spinner(tick), Tok::Accent, phase),
            Phase::Done => (theme.sym(Sym::Check), Tok::Success, "done"),
            Phase::Failed => (theme.sym(Sym::Cross2), Tok::Error, "failed"),
            Phase::Interrupted => (theme.sym(Sym::Cross2), Tok::Muted, "interrupted"),
        };
        let preview = node
            .result
            .text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(text::one_line)
            .unwrap_or_default();
        rows.push(fit(vec![
            Span::styled(
                format!("  {glyph} {}", text::one_line(&node.label)),
                theme.fg(tok),
            ),
            Span::styled(format!(" · step {steps} · {status}"), theme.fg(Tok::Dim)),
            Span::styled(
                if preview.is_empty() {
                    String::new()
                } else {
                    format!(" · {preview}")
                },
                theme.fg(Tok::Muted),
            ),
        ]));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rails_fit_narrow_widths_and_keep_real_file_counts_and_worker_status() {
        let theme = Theme::dark();
        let files = vec![FileDiff {
            path: "app/components/Widget.vue".into(),
            added: Some(12),
            removed: Some(3),
            untracked: false,
        }];
        let tree = crate::runtime::Tree::default();
        let body = tree.start("worker", "Inspect UI", None);
        body.progress("generating", 2);
        body.append("Reviewing the header");
        for w in 1..150 {
            for row in session("abcdef123", "Porting UI", &files, w, 3, &theme)
                .into_iter()
                .chain(workers(&tree.snapshots(), w, 4, 1, &theme))
            {
                assert!(row.width() <= w);
            }
        }
        let plain = |rows: Rows| {
            rows.into_iter()
                .flat_map(|r| r.spans.into_iter())
                .map(|s| s.content.into_owned())
                .collect::<String>()
        };
        assert!(plain(session("id", "Porting UI", &files, 100, 3, &theme)).contains("+12 −3"));
        assert!(
            plain(workers(&tree.snapshots(), 100, 4, 1, &theme)).contains("step 2 · generating")
        );
    }
}
