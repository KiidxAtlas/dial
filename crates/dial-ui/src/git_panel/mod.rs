//! Git access for the `/git` panel. The panel's UI renders and dispatches keys; every git command,
//! patch application, AI commit-message and staging run, and the working-tree watch lives here.
//! The UI reaches git only through [`Repo`], which owns the project root and the config for AI work.

mod ai;
mod git;
mod jobs;
mod nested;
mod watch;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod testkit;

pub(crate) use jobs::JobNotice;
pub(crate) use watch::Watch;

/// Unified-diff context lines for the panel's diffs (matches the diff view).
const CONTEXT: &str = "-U3";

use crate::config::types::Config;
use std::path::{Component, Path, PathBuf};

/// Git handle for one project root. Cheap to clone; background jobs take a clone.
#[derive(Clone)]
pub struct Repo {
    root: PathBuf,
    config: Config,
    /// A nested repository the user chose (`/git repo <folder>`), relative to `root`.
    selected: Option<String>,
}

impl Repo {
    pub fn new(root: &Path, config: &Config) -> Self {
        Self {
            root: root.to_path_buf(),
            config: config.clone(),
            selected: None,
        }
    }

    /// Work on the nested repository at `folder` (relative to the project), not the project itself.
    pub fn select(mut self, folder: &str) -> Self {
        self.selected = Some(folder.to_owned());
        self
    }

    /// Resolve porcelain path coordinates on a worker, never from the input thread.
    ///
    /// A project that is not a repository uses its only nested repository. Several nested
    /// repositories are never guessed between: the error names them so the user can choose.
    pub(crate) fn at_repository_root(mut self) -> Result<Self, String> {
        self.root = match &self.selected {
            Some(folder) => selected_root(&self.root, folder)?,
            None => match top_of(&self.root) {
                Ok(top) => top,
                Err(error) => match nested::repositories(&self.root).as_slice() {
                    [] => return Err(error),
                    [only] => only.clone(),
                    many => return Err(choose_message(&self.root, many)),
                },
            },
        };
        Ok(self)
    }

    /// Watches the worktree and git state. Fails when git or the OS watcher cannot start.
    pub(crate) fn watch(&self) -> Result<Watch, String> {
        Watch::start(&self.root)
    }
}

/// The repository `dir` is the top of, when Git says so.
fn top_of(dir: &Path) -> Result<PathBuf, String> {
    let top = crate::tools::vcs::git::run_checked(dir, &["rev-parse", "--show-toplevel"], false)?;
    Ok(PathBuf::from(top.trim_end_matches(['\n', '\r'])))
}

/// The nested repository at `folder`, which must be a repository top inside `project`.
fn selected_root(project: &Path, folder: &str) -> Result<PathBuf, String> {
    let relative = Path::new(folder);
    if folder.is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "repo must name a folder inside the project, not {folder:?}"
        ));
    }
    let project = project.canonicalize().map_err(|e| e.to_string())?;
    let dir = project
        .join(relative)
        .canonicalize()
        .map_err(|_| format!("{folder} is not a folder in the project"))?;
    if !dir.starts_with(&project) {
        return Err(format!("{folder} is outside the project"));
    }
    match top_of(&dir) {
        Ok(top) if top.canonicalize().is_ok_and(|top| top == dir) => Ok(dir),
        Ok(_) => Err(format!(
            "{folder} is inside another repository, not a repository of its own"
        )),
        Err(_) => Err(format!("{folder} is not a Git repository")),
    }
}

fn choose_message(project: &Path, repos: &[PathBuf]) -> String {
    let names: Vec<String> = repos
        .iter()
        .map(|repo| {
            repo.strip_prefix(project)
                .unwrap_or(repo)
                .display()
                .to_string()
        })
        .collect();
    format!(
        "This folder holds several Git repositories: {}. Open one with /git repo <folder>.",
        names.join(", ")
    )
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

/// A generated commit message and validation feedback.
pub(super) struct Generated {
    pub(super) text: String,
    pub(super) validation_error: Option<String>,
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
