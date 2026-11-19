//! The sources lane: a settled `web_search` reads as the pages it found, not its raw text.
//!
//! ```text
//! ╭─ ⌕ Search · "rust trailing comma serde" ──────────────────────╮
//! │ 1  Serde attributes                               serde.rs     │
//! │ 2  Trailing commas in JSON5                       json5.org    │
//! │ +3 more                                                        │
//! ╰─ 5 results · via brave · 1.20s ────────────────────────────────╯
//! ```
//!
//! Titles and hosts come from the tool's own numbered list (`1. Title` then the URL). Output in
//! any other shape, a failure, or the expanded view keeps the ordinary card.
use super::{Rows, Tool, ToolState, frame};
use crate::ui::{
    text,
    theme::{Sym, Tok},
    transcript::TranscriptCtx,
};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

/// Results listed in the card before the rest are counted.
const LISTED: usize = 5;
/// Narrowest width that frames the list.
const CARD_MIN: usize = 32;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Found {
    pub query: String,
    pub via: Option<String>,
    pub results: Vec<(String, String)>,
}

/// The query, provider and `(title, url)` pairs of a `web_search` result text.
pub(super) fn parse(output: &str) -> Option<Found> {
    let lines: Vec<&str> = output.lines().collect();
    let head = lines.iter().position(|l| {
        let l = l.trim();
        l.ends_with(':')
            && l.split_once(" results for ")
                .is_some_and(|(n, _)| n.parse::<u64>().is_ok())
    })?;
    let (_, rest) = lines[head]
        .trim()
        .trim_end_matches(':')
        .split_once(" results for ")?;
    let (query, via) = match rest.rsplit_once(" via ") {
        Some((query, via)) if query.ends_with('"') => (query, Some(via.trim().to_owned())),
        _ => (rest, None),
    };
    let mut found = Found {
        query: query.trim().trim_matches('"').to_owned(),
        via,
        results: Vec::new(),
    };
    let mut i = head + 1;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("[End of untrusted") || line.starts_with("Fetched search pages") {
            break;
        }
        if let Some((n, title)) = line.split_once(". ")
            && n.parse::<u64>().is_ok()
            && let Some(url) = lines.get(i + 1).map(|l| l.trim())
            && (url.starts_with("http://") || url.starts_with("https://"))
        {
            found.results.push((text::one_line(title), url.to_owned()));
            i += 2;
            continue;
        }
        i += 1;
    }
    (!found.results.is_empty()).then_some(found)
}

/// `docs.rs` of `https://docs.rs/serde/latest`, without a leading `www.`.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

/// The sources card for a settled `web_search`, or `None` for anything else.
pub(super) fn sources_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Option<Rows> {
    if ctx.tools_expanded
        || t.name != "web_search"
        || t.state != ToolState::Completed
        || w < CARD_MIN
    {
        return None;
    }
    let found = parse(&t.output)?;
    let theme = ctx.theme;
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let border = theme.fg(Tok::SectionConnect);
    let mut card = frame::Frame::new(
        vec![
            Span::styled(if theme.ascii() { "?" } else { "⌕" }, border),
            Span::raw(" "),
            Span::styled(
                "Search",
                theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
            ),
            Span::styled(dot.clone(), theme.fg(Tok::Dim)),
            Span::styled(format!("\"{}\"", found.query), theme.fg(Tok::Muted)),
        ],
        border,
    );
    let inner = w.saturating_sub(frame::CHROME_W).max(1);
    let number_w = found.results.len().min(LISTED).to_string().len() + 2;
    let mut body: Rows = found
        .results
        .iter()
        .take(LISTED)
        .enumerate()
        .map(|(i, (title, url))| {
            let host = host(url);
            let host_w = text::width(host).min(inner / 3);
            let room = inner.saturating_sub(number_w + host_w + 1).max(1);
            let title = text::ellipsize(title, room, theme.ascii());
            let gap = inner
                .saturating_sub(number_w + text::width(&title) + host_w)
                .max(1);
            Line::from(vec![
                Span::styled(format!("{:<number_w$}", i + 1), theme.fg(Tok::Dim)),
                Span::styled(title, theme.fg(Tok::Text)),
                Span::raw(" ".repeat(gap)),
                Span::styled(
                    text::ellipsize(host, host_w.max(1), theme.ascii()),
                    theme.fg(Tok::Muted),
                ),
            ])
        })
        .collect();
    if found.results.len() > LISTED {
        body.push(Line::from(Span::styled(
            format!("+{} more", found.results.len() - LISTED),
            theme.fg(Tok::Dim),
        )));
    }
    card.sections.push((None, body));
    let mut facts = vec![super::plural(found.results.len(), "result")];
    if let Some(via) = &found.via {
        facts.push(format!("via {via}"));
    }
    if let Some(secs) = t.elapsed.filter(|s| *s >= 0.05) {
        facts.push(text::duration_label_precise(secs));
    }
    card.foot = vec![Span::styled(facts.join(&dot), theme.fg(Tok::Dim))];
    Some(card.rows(theme, w))
}

#[cfg(test)]
mod tests;
