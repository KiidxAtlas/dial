//! The turn recap card: when a turn did work (tool calls), its closing line becomes a rounded card
//! in the same frame as the tool cards. The top edge says how the turn ended and what it changed,
//! the body shows the last test or build verdict and what the turn did as chips (lookups, edits,
//! commands, web and service calls, workers, failures), the files it changed with their line
//! counts and any recorded handoff, and the bottom edge carries time and tokens. A plain answer
//! keeps the one-line closing row, and so does a terminal too narrow for a card.
use super::{
    super::tool_cards::{Tool, ToolState, Verdict, change_counts, frame, verdict},
    Line, Modifier, Rows, Span, Sym, TranscriptCtx, TurnStats, text,
};
use crate::runtime::presentation::{Presentation, Tone};
use crate::ui::theme::Tok;
use std::collections::BTreeMap;

/// Narrowest width that draws the recap as a card.
const MIN_W: usize = 40;
/// Changed files listed before the rest are counted.
const MAX_FILES: usize = 6;
/// Handoff details shown; the closing line shows the same bound.
const MAX_HANDOFF: usize = 11;

/// What one turn's tool calls did, gathered when the turn ends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Recap {
    pub calls: usize,
    pub lookups: usize,
    pub commands: usize,
    pub edits: usize,
    pub web: usize,
    /// Calls to MCP services.
    pub services: usize,
    pub workers: usize,
    pub failed: usize,
    /// Files the turn's successful edits targeted, in first-touched order, without repeats.
    pub files: Vec<String>,
    /// Lines added and removed per changed file, summed over the edits that reported them.
    pub lines: BTreeMap<String, (u64, u64)>,
    /// The verdict of the turn's last settled test or build command.
    pub check: Option<Verdict>,
}

impl Recap {
    /// Count one settled or running call by what kind of work it is.
    pub fn note(&mut self, name: &str, state: ToolState, presentation: &Presentation) {
        self.calls += 1;
        if state == ToolState::Failed {
            self.failed += 1;
        }
        match presentation.tone {
            _ if name == "task" => self.workers += 1,
            _ if name.starts_with("mcp__") => self.services += 1,
            Tone::Workers => self.workers += 1,
            Tone::Shell => self.commands += 1,
            Tone::Connect => self.web += 1,
            Tone::Edit => {
                self.edits += 1;
                let target = text::one_line(presentation.target.trim());
                if state == ToolState::Completed
                    && !target.is_empty()
                    && target != "null"
                    && !self.files.contains(&target)
                {
                    self.files.push(target);
                }
            }
            Tone::Tools => self.lookups += 1,
        }
    }

    /// Add what a settled call reported: an edit's line counts, a check's verdict.
    pub fn note_result(&mut self, tool: &Tool) {
        if let Some(v) = verdict(tool) {
            self.check = Some(v);
        }
        if tool.presentation.tone == Tone::Edit
            && tool.state == ToolState::Completed
            && let Some((added, removed)) = change_counts(tool)
        {
            let target = text::one_line(tool.presentation.target.trim());
            if self.files.contains(&target) {
                let entry = self.lines.entry(target).or_default();
                entry.0 += added;
                entry.1 += removed;
            }
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The recap card for `stats`, or `None` when the turn made no calls, the width cannot hold a
/// card, or nothing about the turn is shown at all.
pub(super) fn card(stats: &TurnStats, recap: &Recap, w: u16, ctx: &TranscriptCtx) -> Option<Rows> {
    let w = w as usize;
    let timing = ctx.ui.show_turn_stats;
    if recap.calls == 0 || w < MIN_W || (!timing && stats.outcome.is_none()) {
        return None;
    }
    let theme = ctx.theme;
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let outcome = stats.outcome.as_ref();
    let (tok, glyph, word) = match outcome {
        Some(o) => super::outcome_mark(&o.status, theme),
        None => (Tok::Muted, theme.sym(Sym::Dot).to_owned(), "turn"),
    };
    let border = match tok {
        Tok::Error | Tok::Warning => tok,
        _ => Tok::BorderAccent,
    };

    // Top edge: how it ended, then what changed.
    let mut head = vec![
        Span::styled(glyph, theme.fg(tok)),
        Span::raw(" "),
        Span::styled(word.to_owned(), theme.fg(tok).add_modifier(Modifier::BOLD)),
    ];
    let changed = outcome
        .map_or(0, |o| o.changed_files)
        .max(recap.files.len());
    if changed > 0 {
        head.push(Span::styled(dot.clone(), theme.fg(Tok::Dim)));
        head.push(Span::styled(
            format!("{} changed", plural(changed, "file", "files")),
            theme.fg(Tok::Text),
        ));
    }
    let open = outcome.map_or(0, |o| o.open_tasks);
    if open > 0 {
        head.push(Span::styled(dot.clone(), theme.fg(Tok::Dim)));
        head.push(Span::styled(
            plural(open, "open task", "open tasks"),
            theme.fg(Tok::Warning),
        ));
    }

    let mut card = frame::Frame::new(head, theme.fg(border));
    let inner = w - frame::CHROME_W;

    // What the turn did, as chips that wrap to the card's width; the last check's verdict first.
    let mut chips: Vec<Span<'static>> = Vec::new();
    if let Some(check) = &recap.check {
        let (mark, tok) = if check.ok {
            (theme.sym(Sym::Check), Tok::Success)
        } else {
            (theme.sym(Sym::Cross2), Tok::Error)
        };
        chips.push(frame::chip(
            theme,
            &format!("{mark} {} {}", check.what, check.summary),
            tok,
        ));
    }
    chips.extend(
        [
            (recap.calls, "call", "calls", Tok::Text),
            (recap.lookups, "lookup", "lookups", Tok::Muted),
            (recap.edits, "edit", "edits", Tok::Accent),
            (recap.commands, "command", "commands", Tok::Muted),
            (recap.web, "web call", "web calls", Tok::Muted),
            (recap.services, "service call", "service calls", Tok::Muted),
            (recap.workers, "worker", "workers", Tok::Accent),
            (recap.failed, "failed", "failed", Tok::Error),
        ]
        .into_iter()
        .filter(|(n, ..)| *n > 0)
        .map(|(n, one, many, tok)| frame::chip(theme, &plural(n, one, many), tok)),
    );
    let mut activity: Rows = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for chip in chips {
        let cw = text::width(&chip.content);
        if used > 0 && used + 1 + cw > inner {
            activity.push(Line::from(std::mem::take(&mut line)));
            used = 0;
        }
        if used > 0 {
            line.push(Span::raw(" "));
            used += 1;
        }
        used += cw;
        line.push(chip);
    }
    if !line.is_empty() {
        activity.push(Line::from(line));
    }
    card.sections.push((None, activity));

    // The files it changed, the path's tail kept when it is long, line counts at the right edge.
    if !recap.files.is_empty() {
        let mark = theme.sym(Sym::Bullet).to_owned();
        let mut rows: Rows = recap
            .files
            .iter()
            .take(MAX_FILES)
            .map(|file| {
                let counts = recap.lines.get(file).map(|(added, removed)| {
                    (
                        format!("+{added}"),
                        format!("{}{removed}", theme.sym(Sym::Minus)),
                    )
                });
                let counts_w = counts
                    .as_ref()
                    .map_or(0, |(a, r)| text::width(a) + 1 + text::width(r) + 1);
                let room = inner.saturating_sub(text::width(&mark) + 1 + counts_w);
                let path = text::ellipsize_start(file, room.max(1), theme.ascii());
                let mut spans = vec![
                    Span::styled(format!("{mark} "), theme.fg(Tok::Accent)),
                    Span::styled(path.clone(), theme.fg(Tok::Text)),
                ];
                if let Some((added, removed)) = counts {
                    let used = text::width(&mark) + 1 + text::width(&path);
                    let tail = text::width(&added) + 1 + text::width(&removed);
                    spans.push(Span::raw(
                        " ".repeat(inner.saturating_sub(used + tail).max(1)),
                    ));
                    spans.push(Span::styled(added, theme.fg(Tok::Success)));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(removed, theme.fg(Tok::Error)));
                }
                Line::from(spans)
            })
            .collect();
        if recap.files.len() > MAX_FILES {
            rows.push(Line::from(Span::styled(
                format!("+{} more", recap.files.len() - MAX_FILES),
                theme.fg(Tok::Muted),
            )));
        }
        card.sections.push((Some("Changed".into()), rows));
    }

    // Recorded handoff details, as the closing line would show them.
    if let Some(o) = outcome
        && !o.handoff.is_empty()
    {
        let rows = o
            .handoff
            .iter()
            .take(MAX_HANDOFF)
            .map(|detail| Line::from(Span::styled(text::one_line(detail), theme.fg(Tok::Muted))))
            .collect();
        card.sections.push((Some("Handoff".into()), rows));
    }

    // Follow-ups for changed files, right where the change is reported.
    if changed > 0 {
        card.sections.push((
            None,
            vec![Line::from(vec![
                Span::styled("next  ", theme.fg(Tok::Dim)),
                Span::styled("/review-file", theme.fg(Tok::Accent)),
                Span::styled(dot.clone(), theme.fg(Tok::Dim)),
                Span::styled("/undo", theme.fg(Tok::Accent)),
            ])],
        ));
    }

    // Bottom edge: time and tokens.
    if timing {
        let mut facts = vec![text::duration_label_precise(stats.elapsed_secs)];
        if let Some(n) = stats.prompt_tokens {
            facts.push(format!("{} in", text::compact_count(n)));
        }
        if let Some(n) = stats.output_tokens {
            facts.push(format!("{} out", text::compact_count(n)));
        }
        if let Some(rate) = stats
            .tokens_per_second
            .filter(|r| r.is_finite() && *r > 0.0)
        {
            facts.push(format!("{rate:.1} tok/s"));
        }
        card.foot = vec![Span::styled(facts.join(&dot), theme.fg(Tok::Dim))];
    }

    // Full width, like the framed tool cards it sums up.
    Some(card.rows(theme, w))
}

#[cfg(test)]
mod tests;
