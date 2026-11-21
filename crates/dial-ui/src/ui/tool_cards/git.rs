//! The git lane: a settled `git status`, `git diff`, `git commit` or `git push` reads as what it
//! reported, not its log.
//!
//! ```text
//! ◇  Status  main ↑2                         1 staged · 2 changed · 1 untracked
//!      A src/new.rs
//!      M src/parser.rs
//! ±  Diff    3 files                                                  +20 −4
//!      src/parser.rs                                                  +12 −3
//! ●  Commit  1a2b3c4 fix trailing comma parsing            3 files · +20 −4
//! ↑  Push    main → origin                                 1a2b3c4..def5678
//! ```
//!
//! Only a completed call whose output carries git's own report is summarized: the status
//! sections, `diff --git` hunks, `[branch hash] message`, the `To <remote>` ref update. Anything
//! else, every failure, piped or redirected status and diff output, and a diff whose middle was
//! cut keeps the ordinary card with its full output. The expand key also shows the ordinary card.
use super::{Fact, GUTTER, Rows, Tool, ToolState, VERB_W, fit_facts, gutter_span};
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

/// What `git status` reported, in its long or short (`-s`, `--porcelain`) form.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Status {
    pub branch: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    /// `(code, path)` in git's order: `A` added, `M` modified, `D` deleted, `R` renamed, `T` type
    /// changed, `U` unmerged, `?` untracked.
    pub files: Vec<(char, String)>,
    pub staged: usize,
    pub changed: usize,
    pub untracked: usize,
    pub conflicts: usize,
}

/// One file of a unified diff.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DiffFile {
    pub path: String,
    pub added: u64,
    pub removed: u64,
    pub binary: bool,
}

/// Lines a tool or the shell adds around git's own output.
fn decoration(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || (line.starts_with('[') && line.ends_with(']'))
}

/// The code for a long-form status entry such as `modified:   src/a.rs`.
fn entry_code(kind: &str) -> Option<char> {
    Some(match kind {
        "new file" => 'A',
        "modified" => 'M',
        "deleted" => 'D',
        "renamed" | "copied" => 'R',
        "typechange" => 'T',
        "both modified" | "both added" | "both deleted" | "added by us" | "added by them"
        | "deleted by us" | "deleted by them" => 'U',
        _ => return None,
    })
}

/// `ahead 2, behind 1` (short form) or `Your branch is ahead of 'origin/main' by 2 commits.`
fn tracking(status: &mut Status, line: &str) {
    let words: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '.'))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, word) in words.iter().enumerate() {
        let number = |at: usize| words.get(at).and_then(|n| n.parse::<u64>().ok());
        match *word {
            "ahead" => status.ahead = number(i + 1).or_else(|| number(i + 4)).unwrap_or(0),
            "behind" => status.behind = number(i + 1).or_else(|| number(i + 4)).unwrap_or(0),
            _ => {}
        }
    }
    // A diverged branch continues on the next line: `and have 2 and 1 different commits each`.
    if line.contains("different commits")
        && let Some(at) = words.iter().position(|w| *w == "have")
        && let Some([ahead, "and", behind, ..]) = words.get(at + 1..)
    {
        status.ahead = ahead.parse().unwrap_or(0);
        status.behind = behind.parse().unwrap_or(0);
    }
}

/// The long form: `On branch`, the tracking line, then one tab-indented entry per file under
/// each section header. `None` when the output is not a status report.
fn parse_long_status(output: &str) -> Option<Status> {
    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        None,
        Staged,
        Changed,
        Unmerged,
        Untracked,
    }
    let mut status = Status::default();
    let mut section = Section::None;
    let mut recognized = false;
    for raw in output.lines() {
        let line = raw.trim();
        if let Some(branch) = line.strip_prefix("On branch ") {
            status.branch = Some(branch.to_owned());
            recognized = true;
        } else if let Some(at) = line.strip_prefix("HEAD detached at ") {
            status.branch = Some(format!("detached {at}"));
            recognized = true;
        } else if line.starts_with("Your branch ") || line.starts_with("and have ") {
            tracking(&mut status, line);
        } else if line == "Changes to be committed:" {
            section = Section::Staged;
        } else if line == "Changes not staged for commit:" {
            section = Section::Changed;
        } else if line == "Unmerged paths:" {
            section = Section::Unmerged;
        } else if line == "Untracked files:" {
            section = Section::Untracked;
        } else if line.starts_with("nothing to commit") || line.starts_with("nothing added to") {
            recognized = true;
        } else if raw.starts_with('\t') && section != Section::None {
            let (code, path) = if section == Section::Untracked {
                ('?', line.to_owned())
            } else {
                let (kind, path) = line.split_once(':')?;
                (entry_code(kind.trim())?, path.trim().to_owned())
            };
            match section {
                Section::Staged => status.staged += 1,
                Section::Changed => status.changed += 1,
                Section::Unmerged => status.conflicts += 1,
                Section::Untracked => status.untracked += 1,
                Section::None => {}
            }
            status.files.push((code, path));
        } else if !decoration(raw) && section == Section::None && !recognized {
            // Text before git's report that is not git's: not a plain status.
            return None;
        }
    }
    recognized.then_some(status)
}

/// The short form: an optional `## main...origin/main [ahead 2]` line, then `XY path` per file.
fn parse_short_status(output: &str) -> Option<Status> {
    let mut status = Status::default();
    for line in output.lines().filter(|line| !decoration(line)) {
        if let Some(head) = line.strip_prefix("## ") {
            let branch = head.split(['.', ' ']).next().unwrap_or(head);
            status.branch = Some(branch.to_owned());
            tracking(&mut status, head);
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            return None;
        }
        let (x, y, path) = (bytes[0] as char, bytes[1] as char, line[3..].to_owned());
        let known = |c: char| " MADRCTU?!".contains(c);
        if !known(x) || !known(y) {
            return None;
        }
        let code = match (x, y) {
            ('?', '?') => {
                status.untracked += 1;
                '?'
            }
            ('!', '!') => continue,
            (x, y) if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') => {
                status.conflicts += 1;
                'U'
            }
            (x, y) => {
                if x != ' ' {
                    status.staged += 1;
                }
                if y != ' ' {
                    status.changed += 1;
                }
                if x == 'C' || y == 'C' {
                    'R'
                } else if x != ' ' {
                    x
                } else {
                    y
                }
            }
        };
        status.files.push((code, path));
    }
    Some(status)
}

/// A `git status` report in either form, or `None` for output that is not one.
pub(super) fn parse_status(output: &str, short: bool) -> Option<Status> {
    if short {
        parse_short_status(output)
    } else {
        parse_long_status(output)
    }
}

/// The files of a unified diff with their added and removed line counts. `None` when the output
/// holds no `diff --git` header or the shell cut part of it, since counts would then be wrong.
pub(super) fn parse_diff(output: &str) -> Option<Vec<DiffFile>> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut in_hunk = false;
    for line in output.lines() {
        if line.starts_with("[... ") && line.contains(" omitted") {
            return None;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map_or(rest, |(_, path)| path)
                .to_owned();
            files.push(DiffFile {
                path,
                ..DiffFile::default()
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk {
            if line.starts_with("Binary files ") || line == "GIT binary patch" {
                file.binary = true;
            }
        } else if line.starts_with('+') {
            file.added += 1;
        } else if line.starts_with('-') {
            file.removed += 1;
        }
    }
    (!files.is_empty()).then_some(files)
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

/// The arguments after `git <sub>` in the command, for spotting flags such as `--short`.
fn sub_args<'a>(command: &'a str, sub: &str) -> Vec<&'a str> {
    command
        .split(['&', ';', '|', '\n'])
        .map(|part| part.split_whitespace().collect::<Vec<_>>())
        .find(|words| {
            words
                .first()
                .is_some_and(|p| p.rsplit('/').next() == Some("git"))
                && words.iter().skip(1).find(|w| !w.starts_with('-')) == Some(&sub)
        })
        .map(|words| {
            let at = words.iter().position(|w| *w == sub).unwrap_or(0);
            words[at + 1..].to_vec()
        })
        .unwrap_or_default()
}

fn command(t: &Tool) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(&t.input).ok()?;
    args["command"].as_str().map(str::to_owned)
}

/// Files listed under a status or diff row before the rest are counted.
const LISTED: usize = 5;

/// The git rows for a settled status, diff, commit or push, or `None` for anything else.
pub(super) fn git_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Option<Rows> {
    if ctx.tools_expanded || t.name != "bash" || t.state != ToolState::Completed {
        return None;
    }
    let command = command(t)?;
    // Piped or redirected output is no longer git's report, so status and diff need it whole.
    let whole = !command.contains(['|', '>', '<', '`', '$']);
    let wants_status = runs(&command, "status");
    let wants_diff = runs(&command, "diff");
    if (wants_status || wants_diff) && !whole {
        return None;
    }
    let status = if wants_status {
        let args = sub_args(&command, "status");
        // Only the files of a plain report: `-v` adds a diff and `--porcelain=v2` another format.
        if args
            .iter()
            .any(|a| a.starts_with("-v") || a.starts_with("--porcelain=v2") || a.starts_with("-z"))
        {
            return None;
        }
        let short = args.iter().any(|a| {
            matches!(*a, "-s" | "--short" | "-sb" | "-bs" | "--porcelain")
                || a.starts_with("--porcelain=v1")
        });
        // A short report shares the output with nothing else, or its lines could be misread.
        if short && (wants_diff || runs(&command, "commit") || runs(&command, "push")) {
            return None;
        }
        Some(parse_status(&t.output, short)?)
    } else {
        None
    };
    let diff = if wants_diff {
        let args = sub_args(&command, "diff");
        if args.iter().any(|a| {
            matches!(
                *a,
                "--stat"
                    | "--shortstat"
                    | "--numstat"
                    | "--name-only"
                    | "--name-status"
                    | "--quiet"
                    | "--exit-code"
            )
        }) {
            return None;
        }
        match parse_diff(&t.output) {
            Some(files) => Some(files),
            // An empty diff says there is nothing to show.
            None if super::split_trailer(&t.output).trim().is_empty() => Some(Vec::new()),
            None => return None,
        }
    } else {
        None
    };
    let commit = runs(&command, "commit")
        .then(|| parse_commit(&t.output))
        .flatten();
    let push = runs(&command, "push")
        .then(|| parse_push(&t.output))
        .flatten();
    if status.is_none() && diff.is_none() && commit.is_none() && push.is_none() {
        return None;
    }
    let theme = ctx.theme;
    let ascii = theme.ascii();
    let dot = theme.dot_sep();
    let minus = if ascii { "-" } else { "−" };
    let mut rows = Vec::new();
    if let Some(s) = status {
        rows.extend(status_rows(&s, &dot, w, ctx));
    }
    if let Some(files) = diff {
        rows.extend(diff_rows(&files, &dot, w, ctx));
    }
    if let Some(c) = commit {
        let mut facts = Vec::new();
        if let Some(files) = c.files {
            facts.push(Fact {
                text: text::counted(files, "file"),
                drop: 1,
            });
        }
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
            (if ascii { "*" } else { "●" }, Tok::Success),
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
            (if ascii { "^" } else { "↑" }, Tok::Success),
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

/// The status row (branch, how far it is from upstream, counts) and the first files under it.
/// Conflicts lead in the failure color; a clean tree is one row.
fn status_rows(s: &Status, dot: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let ascii = theme.ascii();
    let mut target = vec![Span::styled(
        s.branch.clone().unwrap_or_default(),
        theme.fg(Tok::Accent),
    )];
    let mut track = Vec::new();
    if s.ahead > 0 {
        track.push(format!("{}{}", if ascii { "+" } else { "↑" }, s.ahead));
    }
    if s.behind > 0 {
        track.push(format!("{}{}", if ascii { "-" } else { "↓" }, s.behind));
    }
    if !track.is_empty() {
        target.push(Span::styled(
            format!(" {}", track.join(" ")),
            theme.fg(Tok::Muted),
        ));
    }
    let fact = |n: usize, word: &str, drop: u8| Fact {
        text: format!("{n} {word}"),
        drop,
    };
    let mut facts = Vec::new();
    if s.conflicts > 0 {
        facts.push(fact(s.conflicts, "conflicted", 0));
    }
    if s.staged > 0 {
        facts.push(fact(s.staged, "staged", 0));
    }
    if s.changed > 0 {
        facts.push(fact(s.changed, "changed", 0));
    }
    if s.untracked > 0 {
        facts.push(fact(s.untracked, "untracked", 1));
    }
    let clean = s.files.is_empty();
    if clean {
        facts.push(Fact {
            text: "clean".into(),
            drop: 0,
        });
    }
    let glyph = if clean {
        (theme.sym(Sym::Check), Tok::Success)
    } else if s.conflicts > 0 {
        (theme.sym(Sym::Cross2), Tok::Error)
    } else {
        (if ascii { "o" } else { "◇" }, Tok::Accent)
    };
    let mut rows = vec![row(glyph, "Status", target, facts, dot, w, ctx)];
    let listed = s.files.iter().take(LISTED).map(|(code, path)| {
        let tok = match code {
            'A' | '?' => Tok::Success,
            'D' => Tok::Error,
            'U' => Tok::Error,
            _ => Tok::Warning,
        };
        (
            vec![
                Span::styled(format!("{code} "), theme.fg(tok)),
                Span::styled(path.clone(), theme.fg(Tok::Muted)),
            ],
            Vec::new(),
        )
    });
    rows.extend(file_rows(listed, s.files.len(), w, ctx));
    rows
}

/// The diff row (file count and line totals) and the first files with their own counts.
fn diff_rows(files: &[DiffFile], dot: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let minus = if theme.ascii() { "-" } else { "−" };
    let counts = |added: u64, removed: u64| {
        vec![
            Span::styled(format!("+{added}"), theme.fg(Tok::Success)),
            Span::raw(" "),
            Span::styled(format!("{minus}{removed}"), theme.fg(Tok::Error)),
        ]
    };
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let glyph = (if theme.ascii() { "~" } else { "±" }, Tok::Accent);
    let facts = if files.is_empty() {
        vec![Fact {
            text: "no changes".into(),
            drop: 0,
        }]
    } else {
        vec![Fact {
            text: format!("+{added} {minus}{removed}"),
            drop: 0,
        }]
    };
    let target = vec![Span::styled(
        text::counted(files.len(), "file"),
        theme.fg(Tok::Muted),
    )];
    let mut rows = vec![row(glyph, "Diff", target, facts, dot, w, ctx)];
    let listed = files.iter().take(LISTED).map(|f| {
        let right = if f.binary {
            vec![Span::styled("binary", theme.fg(Tok::Dim))]
        } else {
            counts(f.added, f.removed)
        };
        (
            vec![Span::styled(f.path.clone(), theme.fg(Tok::Muted))],
            right,
        )
    });
    rows.extend(file_rows(listed, files.len(), w, ctx));
    rows
}

/// Indented file rows under a summary row, each with optional right-aligned spans, then
/// `+N more` when `total` exceeds what was listed. The path gives way first on narrow rows.
fn file_rows(
    listed: impl Iterator<Item = (Vec<Span<'static>>, Vec<Span<'static>>)>,
    total: usize,
    w: usize,
    ctx: &TranscriptCtx,
) -> Rows {
    let theme = ctx.theme;
    let indent = GUTTER + 2;
    let mut rows = Vec::new();
    let mut shown = 0;
    for (left, right) in listed {
        shown += 1;
        let right_w: usize = text::spans_width(&right);
        let room = w.saturating_sub(indent + right_w + 1).max(1);
        let mut spans = vec![Span::raw(" ".repeat(indent))];
        spans.extend(crate::ui::text::truncate_spans(
            left,
            room,
            &ellipsis(theme),
        ));
        if right_w > 0 {
            let used: usize = text::spans_width(&spans);
            spans.push(Span::raw(
                " ".repeat(w.saturating_sub(used + right_w).max(1)),
            ));
            spans.extend(right);
        }
        rows.push(fitted(spans, w, theme));
    }
    if total > shown {
        rows.push(fitted(
            vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(format!("+{} more", total - shown), theme.fg(Tok::Dim)),
            ],
            w,
            theme,
        ));
    }
    rows
}

/// One summary row: mark, verb, target spans and the facts at the right edge.
fn row(
    (glyph, glyph_tok): (&str, Tok),
    verb: &str,
    target: Vec<Span<'static>>,
    facts: Vec<Fact>,
    sep: &str,
    w: usize,
    ctx: &TranscriptCtx,
) -> ratatui::text::Line<'static> {
    let theme = ctx.theme;
    let lead = gutter_span(glyph.to_owned(), theme.fg(glyph_tok));
    let lead_w: usize = text::spans_width(&lead);
    let want: usize = text::spans_width(&target);
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
    head.extend(crate::ui::text::truncate_spans(
        target,
        room.max(1),
        &ellipsis(theme),
    ));
    if !facts_text.is_empty() {
        let used: usize = text::spans_width(&head);
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
