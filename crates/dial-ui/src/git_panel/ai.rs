//! AI commit-message generation and AI-driven staging for the `/git` panel.

use super::git::apply_patch;
use super::*;
use crate::commit::ai_stage::{StageUnit, UNIT_CHARS, UnitKind, changed_lines, classify_units};
use crate::commit::repo_context::generation_context;
use crate::commit::{GenerationConfig, GenerationInput, format_conventional_commit, generate};
use crate::tools::vcs::git::run_checked;
use std::{
    path::Path,
    sync::mpsc::{self, Receiver},
};

/// Generates a commit message for the staged changes. With nothing staged, stages everything
/// first.
fn generate_in(root: &Path, config: &Config) -> Result<Generated, String> {
    let mut staged_all = false;
    if run_checked(root, &["diff", "--cached", "--name-only"], false)?
        .trim()
        .is_empty()
    {
        run_checked(root, &["add", "-A"], true)?;
        staged_all = true;
    }
    let diff = run_checked(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
        ],
        false,
    )?;
    if diff.trim().is_empty() {
        return Err("Nothing to commit".to_owned());
    }
    let stat = run_checked(
        root,
        &["diff", "--cached", "--no-color", "--stat", "--no-renames"],
        false,
    )?;
    let numstat = run_checked(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--numstat",
            "--no-renames",
        ],
        false,
    )?;
    let context = generation_context(root);
    let input = GenerationInput {
        diff: &diff,
        stat: &stat,
        numstat: &numstat,
        context: &context,
    };
    let result = generate(
        config,
        &GenerationConfig::from_settings(&config.commit),
        input,
    )?;
    Ok(Generated {
        text: format_conventional_commit(&result.commit),
        validation_error: result.validation_error,
        staged_all,
    })
}

/// What staging one unit means: a hunk of a tracked file, or a whole file.
enum Target {
    Hunk { file: usize, hunk: usize },
    Whole { path: String },
}

/// Classifies the unstaged changes against `instruction` and stages the accepted units.
fn ai_stage_in(
    root: &Path,
    config: &Config,
    instruction: &str,
    changes: &[Change],
) -> Result<AiSummary, String> {
    let mut paths: Vec<String> = Vec::new();
    for change in changes {
        if !paths.contains(&change.path) {
            paths.push(change.path.clone());
        }
    }

    let mut units: Vec<StageUnit> = Vec::new();
    let mut targets: Vec<Target> = Vec::new();
    let mut files: Vec<Doc> = Vec::new();
    let tracked: Vec<&Change> = changes
        .iter()
        .filter(|change| change.kind != Kind::Untracked)
        .collect();
    if !tracked.is_empty() {
        let mut args: Vec<&str> = vec![
            "--literal-pathspecs",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            CONTEXT,
            "--",
        ];
        args.extend(tracked.iter().map(|change| change.path.as_str()));
        let raw = run_checked(root, &args, false)?;
        for chunk in split_file_diffs(&raw) {
            let doc = parse_diff(&chunk);
            let Some(path) = diff_path(&doc.head) else {
                continue;
            };
            if is_binary(&doc) {
                units.push(StageUnit {
                    path: path.clone(),
                    kind: UnitKind::Binary,
                    change: String::new(),
                });
                targets.push(Target::Whole { path });
                continue;
            }
            let deleted = tracked
                .iter()
                .any(|change| change.path == path && change.kind == Kind::Deleted);
            if doc.hunks.is_empty() {
                // Mode-only (or otherwise hunkless) change: no hunk to select, so stage the whole file.
                units.push(StageUnit {
                    path: path.clone(),
                    kind: UnitKind::Hunk,
                    change: doc.head.join("\n"),
                });
                targets.push(Target::Whole { path });
                continue;
            }
            let file = files.len();
            for (hunk, parsed) in doc.hunks.iter().enumerate() {
                units.push(StageUnit {
                    path: path.clone(),
                    kind: if deleted {
                        UnitKind::DeletedFile
                    } else {
                        UnitKind::Hunk
                    },
                    change: changed_lines(&parsed.lines),
                });
                targets.push(Target::Hunk { file, hunk });
            }
            files.push(doc);
        }
    }
    for change in changes
        .iter()
        .filter(|change| change.kind == Kind::Untracked)
    {
        let (kind, text) = match untracked_head(root, &change.path) {
            Some(head) => (UnitKind::NewFile, head),
            None => (UnitKind::Binary, String::new()),
        };
        units.push(StageUnit {
            path: change.path.clone(),
            kind,
            change: text,
        });
        targets.push(Target::Whole {
            path: change.path.clone(),
        });
    }

    let decisions = classify_units(
        config,
        &GenerationConfig::from_settings(&config.commit),
        instruction,
        &paths,
        &units,
    )?;

    let mut summary = AiSummary {
        total_files: paths.len(),
        ..AiSummary::default()
    };
    let mut hunks_by_file: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut whole: Vec<String> = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    for (unit, (target, decision)) in units.iter().zip(targets.iter().zip(decisions.iter())) {
        if matches!(unit.kind, UnitKind::Hunk | UnitKind::DeletedFile) {
            summary.total_hunks += 1;
        }
        if !decision.accepted {
            continue;
        }
        if !matched.contains(&unit.path) {
            matched.push(unit.path.clone());
        }
        match target {
            Target::Hunk { file, hunk } => {
                summary.staged_hunks += 1;
                let slot = hunks_by_file.iter().position(|entry| entry.0 == *file);
                match slot {
                    Some(slot) => hunks_by_file[slot].1.push(*hunk),
                    None => hunks_by_file.push((*file, vec![*hunk])),
                }
            }
            Target::Whole { path } => {
                summary.whole_files += 1;
                whole.push(path.clone());
            }
        }
    }

    let mut patch = String::new();
    for (file, hunks) in &hunks_by_file {
        patch.push_str(&file_patch(&files[*file], hunks));
    }
    if !patch.is_empty() {
        // One `git apply` call is atomic: either every selected hunk applies or none does.
        apply_patch(root, &patch, true, false)?;
    }
    if !whole.is_empty() {
        let mut args: Vec<&str> = vec!["--literal-pathspecs", "add", "--"];
        args.extend(whole.iter().map(String::as_str));
        if let Err(error) = run_checked(root, &args, true) {
            if !patch.is_empty() {
                // Roll back the hunks already staged so a failure leaves the index unchanged.
                let _ = apply_patch(root, &patch, true, true);
            }
            return Err(error);
        }
    }
    summary.matched_files = matched.len();
    Ok(summary)
}

/// Splits `git diff` output into one chunk per file, each starting at its `diff --git` line.
fn split_file_diffs(raw: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    for line in raw.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            chunks.push(String::new());
        }
        if let Some(chunk) = chunks.last_mut() {
            chunk.push_str(line);
        }
    }
    chunks
}

/// The new-side path from a `diff --git` header line, decoded from C-quoting when git quoted it.
fn diff_path(head: &[String]) -> Option<String> {
    let line = head.first()?;
    crate::commit::diff::diff_header_path(line)
}

fn is_binary(doc: &Doc) -> bool {
    doc.head
        .iter()
        .any(|line| line.starts_with("Binary files ") || line.starts_with("GIT binary patch"))
}

/// A patch for several hunks of one file: the file header, then each selected hunk in order.
fn file_patch(doc: &Doc, hunks: &[usize]) -> String {
    let mut out = String::new();
    for line in &doc.head {
        out.push_str(line);
        out.push('\n');
    }
    for &index in hunks {
        let Some(hunk) = doc.hunks.get(index) else {
            continue;
        };
        out.push_str(&hunk.header);
        out.push('\n');
        for line in &hunk.lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The first `UNIT_CHARS` bytes of an untracked file, or `None` when it is binary or unreadable.
fn untracked_head(root: &Path, path: &str) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(root.join(path))
        .ok()?
        .take(UNIT_CHARS as u64)
        .read_to_end(&mut head)
        .ok()?;
    if head.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

impl Repo {
    /// Generates a commit message for the staged changes on a worker thread.
    pub(crate) fn spawn_generate(&self) -> Receiver<Result<Generated, String>> {
        let (sender, receiver) = mpsc::channel();
        let (root, config) = (self.root.clone(), self.config.clone());
        std::thread::spawn(move || {
            let _ = sender.send(generate_in(&root, &config));
        });
        receiver
    }

    /// Classifies the unstaged `changes` against `instruction` on a worker thread.
    pub(crate) fn spawn_ai_stage(
        &self,
        instruction: String,
        changes: Vec<Change>,
    ) -> Receiver<Result<AiSummary, String>> {
        let (sender, receiver) = mpsc::channel();
        let (root, config) = (self.root.clone(), self.config.clone());
        std::thread::spawn(move || {
            let _ = sender.send(ai_stage_in(&root, &config, &instruction, &changes));
        });
        receiver
    }
}

#[cfg(test)]
mod tests;
