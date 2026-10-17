//! Git access for the `/git` panel. The panel's UI renders and dispatches keys; every git command,
//! patch application, AI commit-message and staging run, and the working-tree watch lives here.
//! The UI reaches git only through [`Repo`], which owns the project root and the config for AI work.

mod ai;
mod git;
mod watch;

pub(crate) use watch::Watch;

/// Unified-diff context lines for the panel's diffs (matches the diff view).
const CONTEXT: &str = "-U3";

use crate::config::types::Config;
use std::path::{Path, PathBuf};

/// Git handle for one project root. Cheap to clone; background jobs take a clone.
#[derive(Clone)]
pub struct Repo {
    root: PathBuf,
    config: Config,
}

impl Repo {
    pub fn new(root: &Path, config: &Config) -> Self {
        // Porcelain status paths are relative to the repository top, and diff/add/restore pathspecs
        // resolve against the cwd, so every command must run from the top (the overlay root can be a
        // subdirectory when dial was started there).
        let root = crate::tools::git::run_checked(root, &["rev-parse", "--show-toplevel"], false)
            .map(|top| PathBuf::from(top.trim_end_matches(['\n', '\r'])))
            .unwrap_or_else(|_| root.to_path_buf());
        Self {
            root,
            config: config.clone(),
        }
    }

    /// Watches the worktree and git state. Fails when git or the OS watcher cannot start.
    pub(crate) fn watch(&self) -> Result<Watch, String> {
        Watch::start(&self.root)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Area {
    Unstaged,
    Staged,
    Commit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Change {
    pub(super) path: String,
    pub(super) orig: Option<String>,
    pub(super) kind: Kind,
    pub(super) area: Area,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Hunk {
    pub(super) header: String,
    pub(super) lines: Vec<String>,
}

/// A parsed diff: header lines before the first hunk, then the hunks themselves.
#[derive(Clone, Debug, Default)]
pub(super) struct Doc {
    pub(super) head: Vec<String>,
    pub(super) hunks: Vec<Hunk>,
    pub(super) additions: usize,
    pub(super) deletions: usize,
}

pub(super) struct Commit {
    pub(super) sha: String,
    pub(super) short: String,
    pub(super) subject: String,
    pub(super) body: String,
    pub(super) author: String,
    pub(super) date: String,
}

pub(super) fn change_paths(change: &Change) -> Vec<&str> {
    let mut paths = vec![change.path.as_str()];
    if let Some(orig) = change
        .orig
        .as_deref()
        .filter(|_| change.kind == Kind::Renamed)
    {
        paths.push(orig);
    }
    paths
}

pub(super) fn with_paths<'a>(lead: &[&'a str], paths: &[&'a str]) -> Vec<&'a str> {
    lead.iter().chain(paths.iter()).copied().collect()
}

pub(super) fn sort_changes(changes: &mut [Change]) {
    changes.sort_by_key(|change| match change.area {
        Area::Staged => 0,
        Area::Unstaged | Area::Commit => 1,
    });
}

fn conflicted(x: char, y: char) -> bool {
    matches!(
        (x, y),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

fn kind_of(code: char) -> Kind {
    match code {
        'A' => Kind::Added,
        'D' => Kind::Deleted,
        'R' | 'C' => Kind::Renamed,
        _ => Kind::Modified,
    }
}

pub(super) fn parse_diff(text: &str) -> Doc {
    let mut doc = Doc::default();
    // Split on `\n` only: `str::lines` would strip the `\r` of CRLF files, and hunk patches built from these lines must stay byte-exact.
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        body.split('\n').collect()
    };
    for line in lines {
        if line.starts_with("@@") {
            doc.hunks.push(Hunk {
                header: line.to_owned(),
                lines: Vec::new(),
            });
        } else if let Some(hunk) = doc.hunks.last_mut() {
            match line.as_bytes().first() {
                Some(b'+') => doc.additions += 1,
                Some(b'-') => doc.deletions += 1,
                _ => {}
            }
            hunk.lines.push(line.to_owned());
        } else {
            doc.head.push(line.to_owned());
        }
    }
    doc
}

fn start_of(range: &str) -> u32 {
    range
        .split(',')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
}

pub(super) fn hunk_start(header: &str) -> (u32, u32) {
    let (mut old, mut new) = (1, 1);
    for token in header.split_whitespace().skip(1).take(2) {
        if let Some(range) = token.strip_prefix('-') {
            old = start_of(range);
        } else if let Some(range) = token.strip_prefix('+') {
            new = start_of(range);
        }
    }
    (old, new)
}

/// A patch for one hunk: the file header followed by the hunk.
pub(super) fn hunk_patch(doc: &Doc, index: usize) -> Option<String> {
    let hunk = doc.hunks.get(index)?;
    let mut out = String::new();
    for line in &doc.head {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&hunk.header);
    out.push('\n');
    for line in &hunk.lines {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// A generated commit message and whether generation first staged everything.
pub(super) struct Generated {
    pub(super) text: String,
    pub(super) validation_error: Option<String>,
    pub(super) staged_all: bool,
}

/// Outcome of one AI staging run.
#[derive(Debug, Default)]
pub(super) struct AiSummary {
    pub(super) total_files: usize,
    pub(super) matched_files: usize,
    pub(super) total_hunks: usize,
    pub(super) staged_hunks: usize,
    pub(super) whole_files: usize,
}

impl AiSummary {
    pub(crate) fn message(&self) -> String {
        if self.matched_files == 0 {
            return "AI staging matched no changes".to_owned();
        }
        format!(
            "AI staged {} of {} hunks and {} whole files ({} of {} changed files matched)",
            self.staged_hunks,
            self.total_hunks,
            self.whole_files,
            self.matched_files,
            self.total_files
        )
    }
}
