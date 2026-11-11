//! The check lane: test and build commands (`cargo test`, `pytest`, `cargo clippy`, `tsc`, …).
//!
//! ```text
//! ✓  Test    cargo test -p dial-ui                       412 passed · 12.4s
//! ╭─ ✗ Test · cargo test -p dial-ui ──────────────────────────────────────╮
//! │ ✗ ui::notice::tests::turn_report_is_compact                           │
//! │ ✗ ui::tool_cards::tests::wraps                                        │
//! ╰─ 410 passed · 2 failed · 12.4s · 380 more lines (ctrl+o expands) ─────╯
//! ```
//!
//! A run reads as its result, not its log. A clean pass is one row with its counts. A run that
//! needs attention (failing tests, errors with where they are) is a red card, amber when it only
//! warns, with the counts and the hidden log on its bottom edge; narrow terminals keep the same
//! content as indented rows. The expand key shows the ordinary card with the complete output.
use super::{
    Fact, GUTTER, Rows, Tool, ToolState, VERB_W, fit_facts, frame, gutter_span, hidden_hint,
    phase_glyph, plural,
};
use crate::ui::{
    text,
    theme::{Sym, Tok},
    transcript::{TranscriptCtx, fitted},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

/// Failing tests or errors listed before the rest are counted.
const LISTED: usize = 5;
/// Narrowest width that frames a run needing attention; below it the rows stay unframed.
const CARD_MIN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Test,
    Build,
}

/// Whether a shell command runs tests or a build, from its words.
pub(super) fn kind(command: &str) -> Option<Kind> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let has = |pair: [&str; 2]| words.windows(2).any(|w| w == pair);
    let any = |names: &[&str]| {
        words
            .iter()
            .any(|w| names.contains(&w.rsplit('/').next().unwrap_or(w)))
    };
    if has(["cargo", "test"])
        || has(["cargo", "nextest"])
        || has(["go", "test"])
        || has(["npm", "test"])
        || has(["yarn", "test"])
        || has(["pnpm", "test"])
        || has(["make", "test"])
        || words
            .iter()
            .any(|w| w.starts_with("test-") && words.contains(&"make"))
        || any(&["pytest", "jest", "vitest"])
    {
        return Some(Kind::Test);
    }
    let cargo_build = ["build", "check", "clippy"]
        .iter()
        .any(|sub| has(["cargo", sub]));
    let script_build = ["npm", "yarn", "pnpm"]
        .iter()
        .any(|tool| has([tool, "build"]) || words.windows(3).any(|w| w == [*tool, "run", "build"]));
    if cargo_build || script_build || has(["go", "build"]) || any(&["tsc", "make", "ninja"]) {
        return Some(Kind::Build);
    }
    None
}

/// What a finished run reported.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Report {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    /// Whether a runner summary line (`test result:`, pytest's footer, `Tests:`) was seen.
    pub summarized: bool,
    pub failing: Vec<String>,
    /// Each error line with the location line that follows it, when there is one.
    pub errors: Vec<(String, Option<String>)>,
    pub warnings: u64,
    /// Whether a build reported finishing (`Finished`, `Compiled`).
    pub finished: bool,
}

fn summary_line(line: &str) -> bool {
    line.starts_with("test result:")
        || line.starts_with("Tests:")
        || (line.starts_with('=')
            && line.contains(" in ")
            && (line.contains(" passed") || line.contains(" failed")))
}

/// Adds `<n> passed|failed|ignored|skipped` counts from one summary line.
fn add_counts(report: &mut Report, line: &str) {
    let tokens: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '='))
        .filter(|t| !t.is_empty())
        .collect();
    for pair in tokens.windows(2) {
        let Ok(n) = pair[0].parse::<u64>() else {
            continue;
        };
        let word = pair[1].trim_end_matches(['.', ':']);
        match word {
            "passed" => report.passed += n,
            "failed" => report.failed += n,
            "ignored" | "skipped" => report.ignored += n,
            _ => {}
        }
    }
}

fn is_error(line: &str) -> bool {
    let lead = line.starts_with("error:") || line.starts_with("error[");
    (lead && !line.starts_with("error: could not compile") && !line.starts_with("error: aborting"))
        || line.contains(": error TS")
}

pub(super) fn parse(output: &str) -> Report {
    let mut report = Report {
        failing: crate::agent::run::outcome_gradient::failing_tests(output)
            .into_iter()
            .collect(),
        ..Report::default()
    };
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    for (i, line) in lines.iter().enumerate() {
        if summary_line(line) {
            report.summarized = true;
            add_counts(&mut report, line);
        } else if is_error(line) {
            let location = lines[i + 1..]
                .iter()
                .take(3)
                .find(|next| next.starts_with("--> "))
                .map(|next| next.trim_start_matches("--> ").to_owned());
            report.errors.push((line.to_string(), location));
        } else if line.starts_with("warning:") && !line.contains(" generated ") {
            report.warnings += 1;
        } else if line.starts_with("Finished ") || line.starts_with("Compiled ") {
            report.finished = true;
        }
    }
    report.failed = report.failed.max(report.failing.len() as u64);
    report
}

fn command(t: &Tool) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(&t.input).ok()?;
    args["command"].as_str().map(text::one_line)
}

/// A settled test or build run reduced to its verdict, as the turn recap shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) struct Verdict {
    pub ok: bool,
    /// `tests` or `build`.
    pub what: &'static str,
    /// `18 passed`, `2 failed`, `3 errors`, `clean`.
    pub summary: String,
}

/// The verdict of a settled test or build command, or `None` for any other call and for output
/// that names no result (a job moved to the background).
pub(in crate::ui) fn verdict(t: &Tool) -> Option<Verdict> {
    if t.name != "bash" || !matches!(t.state, ToolState::Completed | ToolState::Failed) {
        return None;
    }
    let kind = kind(&command(t)?)?;
    let report = parse(&t.output);
    let failed = t.state == ToolState::Failed;
    let summary = if report.failed > 0 {
        format!("{} failed", report.failed)
    } else if !report.errors.is_empty() {
        plural(report.errors.len(), "error")
    } else if failed {
        "failed".to_owned()
    } else if report.summarized {
        format!("{} passed", report.passed)
    } else if kind == Kind::Build && report.finished {
        "clean".to_owned()
    } else {
        return None;
    };
    Some(Verdict {
        ok: !failed && report.failed == 0 && report.errors.is_empty(),
        what: match kind {
            Kind::Test => "tests",
            Kind::Build => "build",
        },
        summary,
    })
}

/// The check card for a settled test or build command, or `None` for anything else, a running
/// call, an expanded view, or output this card cannot summarize faithfully.
pub(super) fn check_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Option<Rows> {
    if ctx.tools_expanded
        || t.name != "bash"
        || !matches!(t.state, ToolState::Completed | ToolState::Failed)
    {
        return None;
    }
    let command = command(t)?;
    let kind = kind(&command)?;
    let report = parse(&t.output);
    let failed = t.state == ToolState::Failed;
    // Nothing recognizable (for example a job moved to the background): keep the raw card.
    let recognized = report.summarized
        || !report.errors.is_empty()
        || !report.failing.is_empty()
        || (kind == Kind::Build && report.finished && !failed);
    // A failure we cannot attribute must keep its log visible.
    if !recognized || (failed && report.failing.is_empty() && report.errors.is_empty()) {
        return None;
    }

    let theme = ctx.theme;
    let mut facts = Vec::new();
    let keep = |text: String| Fact { text, drop: 0 };
    if report.summarized {
        facts.push(keep(format!("{} passed", report.passed)));
    }
    if report.failed > 0 {
        facts.push(keep(format!("{} failed", report.failed)));
    }
    if report.ignored > 0 {
        facts.push(Fact {
            text: format!("{} ignored", report.ignored),
            drop: 1,
        });
    }
    if !report.errors.is_empty() {
        facts.push(keep(plural(report.errors.len(), "error")));
    }
    if report.warnings > 0 {
        facts.push(keep(plural(report.warnings as usize, "warning")));
    }
    if facts.is_empty() {
        facts.push(keep("clean".to_owned()));
    }
    if let Some(secs) = t.elapsed.filter(|s| *s >= 0.05) {
        facts.push(Fact {
            text: text::duration_label_precise(secs),
            drop: 2,
        });
    }

    let (glyph, glyph_style) = if failed {
        phase_glyph(ToolState::Failed, ctx)
    } else if report.warnings > 0 {
        (theme.sym(Sym::Warn).to_owned(), theme.fg(Tok::Warning))
    } else {
        (theme.sym(Sym::Check).to_owned(), theme.fg(Tok::Success))
    };
    let verb = match kind {
        Kind::Test => "Test",
        Kind::Build => "Build",
    };
    let verb_style = theme
        .fg(if failed { Tok::Error } else { Tok::ToolTitle })
        .add_modifier(Modifier::BOLD);
    let facts_style = theme.fg(if failed { Tok::Error } else { Tok::Dim });
    let sep = format!(" {} ", theme.sym(Sym::Dot));

    // Only what needs attention: failing tests, else the first errors with their locations.
    let cross = theme.sym(Sym::Cross2);
    let mut attention: Vec<(String, Tok)> = Vec::new();
    let mut count = 0;
    if !report.failing.is_empty() {
        for name in report.failing.iter().take(LISTED) {
            attention.push((format!("{cross} {name}"), Tok::Error));
        }
        count = report.failing.len();
    } else {
        for (error, location) in report.errors.iter().take(LISTED) {
            attention.push((text::one_line(error), Tok::Error));
            if let Some(location) = location {
                attention.push((format!("  {location}"), Tok::Dim));
            }
        }
        count = count.max(report.errors.len());
    }
    if count > LISTED {
        attention.push((format!("+{} more", count - LISTED), Tok::Dim));
    }
    let log_lines = super::split_trailer(&t.output).lines().count();

    if !attention.is_empty() && w >= CARD_MIN {
        return Some(attention_card(
            &AttentionHead {
                glyph: (glyph, glyph_style),
                failed,
                verb: (verb, verb_style),
                command: &command,
                facts: (facts, facts_style),
            },
            attention,
            log_lines,
            w,
            ctx,
        ));
    }

    let lead = gutter_span(glyph, glyph_style);
    let lead_w: usize = lead.iter().map(|s| text::width(&s.content)).sum();
    let want = text::width(&command).min(super::TARGET_MIN);
    let (facts_text, room) = fit_facts(facts, w, lead_w + VERB_W + 1, want, &sep);
    let mut head = lead;
    head.push(Span::styled(format!("{verb:<VERB_W$}"), verb_style));
    head.push(Span::raw(" "));
    head.push(Span::styled(
        text::ellipsize(&command, room.max(1), theme.ascii()),
        theme.fg(Tok::Muted),
    ));
    if !facts_text.is_empty() {
        let used: usize = head.iter().map(|s| text::width(&s.content)).sum();
        let right_w = text::width(&facts_text);
        head.push(Span::raw(
            " ".repeat(w.saturating_sub(used + right_w).max(1)),
        ));
        head.push(Span::styled(facts_text, facts_style));
    }
    let mut rows = vec![fitted(head, w, theme)];
    let indent = " ".repeat(GUTTER);
    let room = w.saturating_sub(GUTTER).max(1);
    for (line, tok) in &attention {
        rows.push(fitted(
            vec![
                Span::raw(indent.clone()),
                Span::styled(text::ellipsize(line, room, theme.ascii()), theme.fg(*tok)),
            ],
            w,
            theme,
        ));
    }
    if rows.len() > 1 && log_lines > 0 {
        rows.push(fitted(
            vec![
                Span::raw(indent),
                hidden_hint(log_lines, ctx.expand_key, theme),
            ],
            w,
            theme,
        ));
    }
    Some(rows)
}

/// What a check's header carries: its mark, verb, command and counts with their styles.
struct AttentionHead<'a> {
    glyph: (String, Style),
    failed: bool,
    verb: (&'static str, Style),
    command: &'a str,
    facts: (Vec<Fact>, Style),
}

/// A run that needs attention as a framed card: the mark, verb and command on the top edge, the
/// failing tests or errors inside, and the counts with the hidden log on the bottom edge. The
/// outline is red for a failure and amber when the run only warns.
fn attention_card(
    head: &AttentionHead<'_>,
    attention: Vec<(String, Tok)>,
    log_lines: usize,
    w: usize,
    ctx: &TranscriptCtx,
) -> Rows {
    let theme = ctx.theme;
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let (glyph, glyph_style) = &head.glyph;
    let (verb, verb_style) = head.verb;
    let border = if head.failed {
        Tok::Error
    } else {
        Tok::Warning
    };
    let mut card = frame::Frame::new(
        vec![
            Span::styled(glyph.clone(), *glyph_style),
            Span::raw(" "),
            Span::styled(verb, verb_style),
            Span::styled(dot.clone(), theme.fg(Tok::Dim)),
            Span::styled(text::one_line(head.command), theme.fg(Tok::Muted)),
        ],
        theme.fg(border),
    );
    let inner = w.saturating_sub(frame::CHROME_W).max(1);
    card.sections.push((
        None,
        attention
            .into_iter()
            .map(|(line, tok)| {
                Line::from(Span::styled(
                    text::ellipsize(&line, inner, theme.ascii()),
                    theme.fg(tok),
                ))
            })
            .collect(),
    ));
    let (facts, facts_style) = &head.facts;
    let text = facts
        .iter()
        .map(|f| f.text.as_str())
        .collect::<Vec<_>>()
        .join(&dot);
    card.foot = vec![Span::styled(text, *facts_style)];
    if log_lines > 0 {
        card.foot.push(Span::styled(dot, theme.fg(Tok::Dim)));
        card.foot
            .push(hidden_hint(log_lines, ctx.expand_key, theme));
    }
    card.rows(theme, w)
}

#[cfg(test)]
mod tests;
