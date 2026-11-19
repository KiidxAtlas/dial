//! The closing recap puts the completion record's paths and checks above the activity chips.
//! Expanded details retain the raw handoff; plain answers keep their one-line closing row.
use super::{
    super::tool_cards::{Tool, ToolState, Verdict, change_counts, frame, verdict},
    Line, Modifier, ReceiptCheck, Rows, Span, Sym, TranscriptCtx, TurnOutcome, TurnStats, text,
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
    /// An edit was attempted after the observed command; its result is historical.
    pub check_stale: bool,
    /// Numbered steps (`1.`, `2)`, …) in the turn's last answer.
    pub answer_steps: usize,
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
                self.check_stale |= self.check.is_some();
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
            self.check_stale = false;
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

/// A follow-up the last turn's recap offers under Alt+1, Alt+2, … in this order: its word on the
/// card and what the key does. Recovery comes first, so Alt+1 is always the most useful next step
/// for how the turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecapAction {
    pub label: &'static str,
    pub run: RecapRun,
}

/// What a recap follow-up does when its key is pressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecapRun {
    /// Run this slash command.
    Command(&'static str),
    /// Put this text in the empty composer for review; nothing runs until Enter.
    Draft(String),
}

impl RecapAction {
    fn command(label: &'static str, command: &'static str) -> Self {
        Self {
            label,
            run: RecapRun::Command(command),
        }
    }
}

/// The follow-ups for a turn that ended with `stats` after doing `recap`. Empty for a plain
/// answer: only a turn that made calls draws a recap card to carry them.
pub(super) fn actions(stats: &TurnStats, recap: &Recap) -> Vec<RecapAction> {
    let mut out = Vec::new();
    if recap.calls == 0 {
        return out;
    }
    // A failed check is the most specific thing to redo: its own command, as a shell line in
    // the draft, so the user sees exactly what will run.
    if let Some(check) = recap.check.as_ref().filter(|v| !v.ok) {
        out.push(RecapAction {
            label: "rerun check",
            run: RecapRun::Draft(format!("!{}", check.command)),
        });
    }
    let outcome = stats.outcome.as_ref();
    let status = outcome.map_or("", |o| o.status.as_str());
    if status == "failed" {
        out.push(RecapAction::command("retry", "/retry"));
    }
    if matches!(status, "blocked" | "needs_input" | "incomplete")
        && outcome.is_some_and(|o| o.open_tasks > 0)
    {
        out.push(RecapAction::command("plan", "/todo"));
    }
    if changed_count(outcome, recap) > 0 {
        out.push(RecapAction::command("review", "/review-file"));
        out.push(RecapAction::command("undo", "/undo"));
    }
    // An answer that lists steps can become tracked work; the request is drafted for review.
    if recap.answer_steps >= MIN_STEPS && !out.iter().any(|a| a.label == "plan") {
        out.push(RecapAction {
            label: "plan steps",
            run: RecapRun::Draft(PLAN_STEPS.to_owned()),
        });
    }
    out
}

/// Numbered steps an answer needs before the recap offers to plan them.
const MIN_STEPS: usize = 3;
/// The request the "plan steps" follow-up drafts.
const PLAN_STEPS: &str =
    "Turn the numbered steps in your last answer into a plan, then start the first step.";

/// How many numbered list items (`1.`, `2)`) start a line of `answer`, outside code fences.
pub(super) fn numbered_steps(answer: &str) -> usize {
    let mut fenced = false;
    answer
        .lines()
        .map(str::trim_start)
        .filter(|line| {
            if line.starts_with("```") || line.starts_with("~~~") {
                fenced = !fenced;
                return false;
            }
            let digits = line.chars().take_while(char::is_ascii_digit).count();
            !fenced
                && (1..=3).contains(&digits)
                && matches!(line[digits..].chars().next(), Some('.' | ')'))
                && line[digits + 1..].starts_with(' ')
        })
        .count()
}

/// Primary shortcut printed on this platform's recap card.
pub fn action_chord(index: usize) -> Option<crate::ui::keys::Chord> {
    action_chords(index).map(|chords| chords[0])
}

pub(super) fn action_chords(index: usize) -> Option<[crate::ui::keys::Chord; 2]> {
    use crate::ui::keys::Chord;
    use crossterm::event::{KeyCode, KeyModifiers};
    if index >= 9 {
        return None;
    }
    let number = index as u8 + 1;
    let digit = Chord {
        code: KeyCode::Char(char::from(b'0' + number)),
        mods: KeyModifiers::ALT,
    };
    let function = Chord {
        code: KeyCode::F(number),
        mods: KeyModifiers::SHIFT,
    };
    Some(if cfg!(target_os = "macos") {
        [function, digit]
    } else {
        [digit, function]
    })
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn changed_count(outcome: Option<&TurnOutcome>, recap: &Recap) -> usize {
    // Only the recorder knows which paths really changed (including batches and no-op edits).
    outcome.map_or(recap.files.len(), |o| o.changed_files)
}

/// A compact receipt replaces, rather than repeats, the verbose changed/check/evidence lines.
pub(super) fn handoff_rows(outcome: &TurnOutcome, width: usize, ctx: &TranscriptCtx) -> Rows {
    let detailed = ctx.tools_expanded || outcome.receipt.is_none();
    // Older unstructured producers keep the original footer bound. Typed handoffs are already
    // bounded by the projection, including its final recovery instruction.
    let limit = if outcome.receipt.is_some() {
        usize::MAX
    } else {
        MAX_HANDOFF
    };
    let rows = outcome
        .handoff
        .iter()
        .filter(move |line| {
            detailed
                || line.as_str() == "Changed: none recorded"
                || ![
                    "Changed:",
                    "Checks:",
                    "Evidence:",
                    "Open tasks:",
                    "Cost:",
                    "Model time:",
                    "Next: /review-file",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .take(limit)
        .map(|detail| {
            Line::from(Span::styled(
                text::one_line(detail),
                ctx.theme.fg(Tok::Text),
            ))
        })
        .collect();
    if ctx.tools_expanded && outcome.receipt.is_some() {
        text::wrap(rows, width)
    } else {
        rows
    }
}

/// Recorded facts take precedence over live tool cards, which may predate a later edit.
pub(super) fn receipt_rows(
    outcome: &TurnOutcome,
    recap: &Recap,
    w: usize,
    ctx: &TranscriptCtx,
) -> Rows {
    let Some(receipt) = outcome.receipt.as_ref().filter(|_| !ctx.tools_expanded) else {
        return Vec::new();
    };
    let theme = ctx.theme;
    let mut rows = Vec::new();
    let field = |name| Span::styled(name, theme.fg(Tok::Text).add_modifier(Modifier::BOLD));
    for (i, path) in receipt.paths.iter().take(MAX_FILES).enumerate() {
        let prefix = if i == 0 { "Changed  " } else { "         " };
        let counts = recap
            .lines
            .get(path)
            .map(|(a, r)| format!(" +{a} {}{r}", theme.sym(Sym::Minus)));
        let room = w.saturating_sub(prefix.len() + counts.as_deref().map_or(0, text::width));
        let mut spans = vec![
            field(prefix),
            Span::styled(
                text::ellipsize_start(path, room.max(1), theme.ascii()),
                theme.fg(Tok::Accent),
            ),
        ];
        if let Some(counts) = counts {
            spans.push(Span::styled(counts, theme.fg(Tok::Muted)));
        }
        rows.push(super::fitted(spans, w, theme));
    }
    let rest = outcome
        .changed_files
        .saturating_sub(receipt.paths.len().min(MAX_FILES));
    if rest > 0 {
        rows.push(super::fitted(
            vec![Span::styled(
                format!("         +{rest} more"),
                theme.fg(Tok::Muted),
            )],
            w,
            theme,
        ));
    }
    let tok = match receipt.check {
        ReceiptCheck::Passed => Tok::Success,
        ReceiptCheck::Failed => Tok::Error,
        ReceiptCheck::Stale | ReceiptCheck::Unverified => Tok::Warning,
    };
    let mut check = vec![
        field("Checks   "),
        Span::styled(receipt.check_label.clone(), theme.fg(tok)),
    ];
    if let Some(target) = &receipt.check_target {
        let mut add = |label, value: String, tok| {
            let used: usize = check.iter().map(|s| text::width(&s.content)).sum();
            if used + 3 + text::width(&value) > w {
                rows.push(super::fitted(std::mem::take(&mut check), w, theme));
                check.push(field(label));
            } else {
                check.push(Span::styled(
                    format!(" {} ", theme.sym(Sym::Dot)),
                    theme.fg(Tok::Dim),
                ));
            }
            check.push(Span::styled(value, theme.fg(tok)));
        };
        if !recap.check_stale
            && let Some(v) = &recap.check
            && v.command == *target
            && ((v.ok && receipt.check == ReceiptCheck::Passed)
                || (!v.ok && receipt.check == ReceiptCheck::Failed))
        {
            add("Last run ", format!("{} {}", v.what, v.summary), tok);
        }
        add("Target   ", target.clone(), Tok::Text);
    }
    rows.push(super::fitted(check, w, theme));
    if receipt.check_target.is_none()
        && let Some(v) = &recap.check
    {
        // Output without a recorded check isn't proof that the final revision was verified.
        // Keep the observed result, but do not promote the receipt to passed.
        let historical = if recap.check_stale { " (stale)" } else { "" };
        rows.push(super::fitted(
            vec![
                field("Last run "),
                Span::styled(
                    format!("{} {}{historical}", v.what, v.summary),
                    theme.fg(if v.ok { Tok::Text } else { Tok::Error }),
                ),
                Span::styled(
                    format!(
                        " {} {}",
                        theme.sym(Sym::Dot),
                        crate::storage::redact::redact_text(&v.command)
                    ),
                    theme.fg(Tok::Text),
                ),
            ],
            w,
            theme,
        ));
    }
    rows
}

/// The recap card for a turn with tool activity.
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
    let changed = changed_count(outcome, recap);
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
            theme.fg(outcome.map_or(Tok::Warning, |o| super::open_tasks_tok(&o.status))),
        ));
    }

    let mut card = frame::Frame::new(head, theme.fg(border));
    let inner = w - frame::CHROME_W;
    if let Some(o) = outcome {
        let receipt = receipt_rows(o, recap, inner, ctx);
        if !receipt.is_empty() {
            card.sections.push((None, receipt));
        }
    }

    // What the turn did, as chips that wrap to the card's width; the last check's verdict first.
    let mut chips: Vec<Span<'static>> = Vec::new();
    if outcome.is_none_or(|o| o.receipt.is_none())
        && let Some(check) = &recap.check
    {
        let (mark, tok) = if recap.check_stale {
            (theme.sym(Sym::Warn), Tok::Warning)
        } else if check.ok {
            (theme.sym(Sym::Check), Tok::Success)
        } else {
            (theme.sym(Sym::Cross2), Tok::Error)
        };
        let freshness = if recap.check_stale { "stale: " } else { "" };
        chips.push(frame::chip(
            theme,
            &format!("{mark} {freshness}{} {}", check.what, check.summary),
            tok,
        ));
    }
    // The total is only news when no breakdown chip below names where the calls went.
    let broken_down =
        recap.lookups + recap.edits + recap.commands + recap.web + recap.services + recap.workers
            > 0;
    chips.extend(
        [
            (
                if broken_down { 0 } else { recap.calls },
                "call",
                "calls",
                Tok::Text,
            ),
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
    if changed > 0 && outcome.is_none_or(|o| o.receipt.is_none()) && !recap.files.is_empty() {
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
        let rows = handoff_rows(o, inner, ctx);
        if !rows.is_empty() {
            card.sections.push((Some("Handoff".into()), rows));
        }
    }

    // Follow-ups use the primary shortcut on this platform.
    let follow_ups = actions(stats, recap);
    if !follow_ups.is_empty() {
        let mut spans = vec![Span::styled("next  ", theme.fg(Tok::Dim))];
        for (i, action) in follow_ups.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(dot.clone(), theme.fg(Tok::Dim)));
            }
            if let Some(chord) = action_chord(i) {
                spans.push(Span::styled(
                    format!("{} ", chord.display()),
                    theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(action.label, theme.fg(Tok::Accent)));
        }
        card.sections.push((None, vec![Line::from(spans)]));
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
