//! Context sources that sit beside the walked tree: the primary checkout of a linked git worktree,
//! `opencode.json` instruction entries and the directories named by
//! `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`.

use std::{
    fs,
    path::{Path, PathBuf},
};

const COPILOT_DIRS_ENV: &str = "COPILOT_CUSTOM_INSTRUCTIONS_DIRS";

/// Primary checkout of a linked worktree, read from git's own metadata. A linked worktree's `.git`
/// is a file `gitdir: <dir>` pointing at `<common>/worktrees/<name>`, whose `commondir` file names
/// the shared `<common>` (a `.git` directory). The primary checkout is that directory's parent.
/// `None` for ordinary checkouts and for any layout that does not match.
pub(super) fn primary_checkout(root: &Path) -> Option<PathBuf> {
    let dotgit = root.join(".git");
    if !dotgit.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dotgit).ok()?;
    let gitdir = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?;
    let gitdir = fs::canonicalize(root.join(gitdir.trim())).ok()?;
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let common_rel = fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = fs::canonicalize(gitdir.join(common_rel.trim())).ok()?;
    if common.file_name()? != ".git" || common != worktrees.parent()? {
        return None;
    }
    let primary = common.parent()?.to_path_buf();
    let canonical_root = fs::canonicalize(root).ok()?;
    (primary != canonical_root && primary.is_dir()).then_some(primary)
}

/// `instructions` entries of the project's `opencode.json`, or of `opencode.jsonc` when that file
/// is missing or unparsable. Entries are returned as written; the caller resolves them against the
/// project root and refuses anything outside it. A file that parses nowhere contributes nothing.
pub(super) fn opencode_instructions(root: &Path) -> Vec<String> {
    let parsed = ["opencode.json", "opencode.jsonc"]
        .into_iter()
        .find_map(|name| {
            let text = fs::read_to_string(root.join(name)).ok()?;
            let text = if name.ends_with(".jsonc") {
                strip_jsonc(&text)
            } else {
                text
            };
            serde_json::from_str::<serde_json::Value>(&text).ok()
        });
    let Some(value) = parsed else {
        return Vec::new();
    };
    value
        .get("instructions")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Makes JSONC text strict JSON: removes `//` and `/* */` comments, then trailing commas that
/// directly precede `}` or `]`. String literals (with escapes) are copied verbatim, so a `//`
/// inside a URL or path survives.
pub(super) fn strip_jsonc(text: &str) -> String {
    strip_trailing_commas(&strip_comments(text))
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    let mut in_string = false;
    let mut pending: Option<char> = None;
    while let Some(c) = pending.take().or_else(|| chars.next()) {
        if in_string {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c != '/' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('/') => {
                // Line comment: drop through the newline, which is kept as separator.
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            Some('*') => {
                // Block comment: drop through `*/` (an unterminated one swallows the rest).
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if prev == '*' && next == '/' {
                        break;
                    }
                    prev = next;
                }
                out.push(' ');
            }
            other => {
                out.push('/');
                pending = other;
            }
        }
    }
    out
}

fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' && closes_next(&chars[i + 1..]) {
            // Trailing comma: dropped.
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// True when the next non-whitespace character is `}` or `]`.
fn closes_next(rest: &[char]) -> bool {
    rest.iter()
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| matches!(c, '}' | ']'))
}

/// Directories named by `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` (comma-separated), resolved against the
/// environment of this process. See [`parse_copilot_dirs`].
pub(in crate::agent) fn copilot_instruction_dirs(root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let raw = std::env::var(COPILOT_DIRS_ENV).unwrap_or_default();
    parse_copilot_dirs(&raw, root, home)
}

/// Parses a comma-separated directory list. `~/` expands to `home`, relative entries resolve
/// against `root`, entries that are not existing directories are dropped, and the order is kept
/// with duplicates removed.
pub(super) fn parse_copilot_dirs(raw: &str, root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let candidate = match entry.strip_prefix("~/") {
            Some(rest) => match home {
                Some(home) => home.join(rest),
                None => continue,
            },
            None => root.join(entry),
        };
        let Ok(real) = fs::canonicalize(&candidate) else {
            continue;
        };
        if real.is_dir() && !out.contains(&real) {
            out.push(real);
        }
    }
    out
}
