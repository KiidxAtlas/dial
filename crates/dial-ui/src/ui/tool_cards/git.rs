//! The git lane: a settled `git commit` or `git push` reads as what it recorded, not its log.
//!
//! ```text
//! ●  Commit  1a2b3c4 fix trailing comma parsing            3 files · +20 −4
//! ↑  Push    main → origin                                 1a2b3c4..def5678
//! ```
//!
//! Only a completed call whose output carries git's own summary (`[branch hash] message`, the
//! `To <remote>` ref update) is summarized; anything else, including every failure, keeps the
//! ordinary card with its full output. The expand key also shows the ordinary card.
use super::{Fact, Rows, Tool, ToolState, VERB_W, fit_facts, gutter_span};
use crate::ui::{
    text,
    theme::{Sym, Tok},
    transcript::{TranscriptCtx, ellipsis, fitted},
};
use ratatui::{style::Modifier, text::Span};

/// What a commit recorded.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Commit {
    pub hash: String,
    pub branch: String,
    pub message: String,
    pub files: Option<u64>,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// What a push updated.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Push {
    pub remote: String,
    /// `main -> main` as `(local, remote)`.
    pub refs: Option<(String, String)>,
    /// `1a2b3c4..def5678`, `[new branch]`, or `up to date`.
    pub range: String,
}

/// `[main 1a2b3c4] message` or `[main (root-commit) 1a2b3c4] message`, then the stat line.
pub(super) fn parse_commit(output: &str) -> Option<Commit> {
    let mut commit = None;
    for line in output.lines().map(str::trim) {
        if commit.is_none()
            && let Some(rest) = line.strip_prefix('[')
            && let Some((head, message)) = rest.split_once("] ")
        {
            let words: Vec<&str> = head.split_whitespace().collect();
            let (Some(branch), Some(hash)) = (words.first(), words.last()) else {
                continue;
            };
            if words.len() < 2 || hash.len() < 7 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            commit = Some(Commit {
                hash: (*hash).to_owned(),
                branch: (*branch).to_owned(),
                message: text::one_line(message),
                ..Commit::default()
            });
        } else if let Some(commit) = commit.as_mut()
            && line.contains(" changed")
            && (line.contains("file changed") || line.contains("files changed"))
        {
            for part in line.split(',').map(str::trim) {
                let n = part.split_whitespace().next().and_then(|n| n.parse().ok());
                if part.contains("changed") {
                    commit.files = n;
                } else if part.contains("insertion") {
                    commit.added = n;
                } else if part.contains("deletion") {
                    commit.removed = n;
                }
            }
        }
    }
    commit
}

/// `To github.com:o/r.git` followed by `   1a2b..3c4d  main -> main`, or `Everything up-to-date`.
pub(super) fn parse_push(output: &str) -> Option<Push> {
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    if lines.contains(&"Everything up-to-date") {
        return Some(Push {
            range: "up to date".into(),
            ..Push::default()
        });
    }
    let at = lines.iter().position(|l| l.starts_with("To "))?;
    let remote = remote_name(lines[at].trim_start_matches("To ").trim());
    let update = lines[at + 1..].iter().find(|l| l.contains(" -> "))?;
    let update = update.trim_start_matches(['*', '+', '-', '!', '=']).trim();
    let (range, refs) = if let Some(rest) = update.strip_prefix('[') {
        let (label, refs) = rest.split_once(']')?;
        (label.trim().to_owned(), refs.trim())
    } else {
        let (range, refs) = update.split_once(char::is_whitespace)?;
        (range.trim().to_owned(), refs.trim())
    };
    let (local, remote_ref) = refs.split_once(" -> ")?;
    Some(Push {
        remote,
        refs: Some((local.trim().to_owned(), remote_ref.trim().to_owned())),
        range,
    })
}

/// `owner/repo` of a remote URL, or the URL itself when it has no recognizable path.
fn remote_name(url: &str) -> String {
    let path = url
        .rsplit_once(':')
        .filter(|(scheme, _)| !scheme.ends_with("https") && !scheme.ends_with("http"))
        .map(|(_, path)| path)
        .unwrap_or(url);
    let parts: Vec<&str> = path.trim_end_matches(".git").rsplit('/').take(2).collect();
    match parts.as_slice() {
        [repo, owner] if !owner.is_empty() && !owner.contains('.') => format!("{owner}/{repo}"),
        _ => text::one_line(url),
    }
}

/// Whether the shell command runs `git <sub>` (directly or in a chain such as `git add && git commit`).
fn runs(command: &str, sub: &str) -> bool {
    command
        .split(['&', ';', '|', '\n'])
        .map(str::split_whitespace)
        .any(|mut words| {
            words
                .next()
                .is_some_and(|p| p.rsplit('/').next() == Some("git"))
                && words.find(|w| !w.starts_with('-')) == Some(sub)
        })
}

fn command(t: &Tool) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(&t.input).ok()?;
    args["command"].as_str().map(str::to_owned)
}

/// The git rows for a settled commit or push, or `None` for anything else.
pub(super) fn git_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Option<Rows> {
    if ctx.tools_expanded || t.name != "bash" || t.state != ToolState::Completed {
        return None;
    }
    let command = command(t)?;
    let commit = runs(&command, "commit")
        .then(|| parse_commit(&t.output))
        .flatten();
    let push = runs(&command, "push")
        .then(|| parse_push(&t.output))
        .flatten();
    if commit.is_none() && push.is_none() {
        return None;
    }
    let theme = ctx.theme;
    let ascii = theme.ascii();
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let mut rows = Vec::new();
    if let Some(c) = commit {
        let mut facts = Vec::new();
        if let Some(files) = c.files {
            facts.push(Fact {
                text: super::plural(files as usize, "file"),
                drop: 1,
            });
        }
        let minus = if ascii { "-" } else { "−" };
        let lines = match (c.added, c.removed) {
            (Some(a), Some(r)) => Some(format!("+{a} {minus}{r}")),
            (Some(a), None) => Some(format!("+{a}")),
            (None, Some(r)) => Some(format!("{minus}{r}")),
            (None, None) => None,
        };
        if let Some(lines) = lines {
            facts.push(Fact {
                text: lines,
                drop: 0,
            });
        }
        let short: String = c.hash.chars().take(7).collect();
        rows.push(row(
            if ascii { "*" } else { "●" },
            "Commit",
            vec![
                Span::styled(short, theme.fg(Tok::Accent)),
                Span::raw(" "),
                Span::styled(c.message, theme.fg(Tok::Text)),
            ],
            facts,
            &dot,
            w,
            ctx,
        ));
    }
    if let Some(p) = push {
        let arrow = if ascii { "->" } else { "→" };
        let target = match (&p.refs, p.remote.is_empty()) {
            (Some((local, _)), false) => format!("{local} {arrow} {}", p.remote),
            (Some((local, remote)), true) => format!("{local} {arrow} {remote}"),
            (None, _) => p.remote.clone(),
        };
        rows.push(row(
            if ascii { "^" } else { "↑" },
            "Push",
            vec![Span::styled(target, theme.fg(Tok::Muted))],
            vec![Fact {
                text: p.range,
                drop: 0,
            }],
            &dot,
            w,
            ctx,
        ));
    }
    Some(rows)
}

/// One summary row: mark, verb, target spans and the facts at the right edge.
fn row(
    glyph: &str,
    verb: &str,
    target: Vec<Span<'static>>,
    facts: Vec<Fact>,
    sep: &str,
    w: usize,
    ctx: &TranscriptCtx,
) -> ratatui::text::Line<'static> {
    let theme = ctx.theme;
    let lead = gutter_span(glyph.to_owned(), theme.fg(Tok::Success));
    let lead_w: usize = lead.iter().map(|s| text::width(&s.content)).sum();
    let want: usize = target.iter().map(|s| text::width(&s.content)).sum();
    let (facts_text, room) = fit_facts(
        facts,
        w,
        lead_w + VERB_W + 1,
        want.min(super::TARGET_MIN),
        sep,
    );
    let mut head = lead;
    head.push(Span::styled(
        format!("{verb:<VERB_W$}"),
        theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
    ));
    head.push(Span::raw(" "));
    head.extend(crate::ui::markdown::truncate_spans(
        target,
        room.max(1),
        &ellipsis(theme),
    ));
    if !facts_text.is_empty() {
        let used: usize = head.iter().map(|s| text::width(&s.content)).sum();
        let right_w = text::width(&facts_text);
        head.push(Span::raw(
            " ".repeat(w.saturating_sub(used + right_w).max(1)),
        ));
        head.push(Span::styled(facts_text, theme.fg(Tok::Dim)));
    }
    fitted(head, w, theme)
}

#[cfg(test)]
mod tests;
