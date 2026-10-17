//! Ambient project context (AGENTS.md & friends), `@file` references and slash-command templates.
//!
//! Everything read here is untrusted data: it is only ever read and handed to the model as text,
//! never executed, and every size is capped. The one exception that spawns a process is `@diff` /
//! `@staged`, which runs `git diff` with an explicit argument vector (no shell) and with the
//! repo-configurable hooks that can execute programs (external diff, textconv, fsmonitor) disabled.
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use crate::config::paths::Layout;
use crate::runtime::arguments::split_words;

const FILE_CAP: usize = 32 * 1024;
const TOTAL_CAP: usize = 128 * 1024;
const MAX_WALK_LEVELS: usize = 8;
const REF_FILE_CAP: usize = 64 * 1024;
const REF_TOTAL_CAP: usize = 256 * 1024;
const MAX_REFS: usize = 16;
const DIR_LISTING_CAP: usize = 200;
const GIT_OUTPUT_CAP: usize = 64 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_FILE_CAP: usize = 32 * 1024;
const TRUNCATION_NOTICE: &str = "[... truncated: context size limit reached ...]";

#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub display: String,
    pub content: String,
}

// ---------------------------------------------------------------- reading helpers

/// A regular file's text, at most `cap` bytes. `None` for missing, non-regular, unreadable or binary
/// (NUL in the first 8 KiB) files. The flag says the content was cut.
fn read_text(path: &Path, cap: usize) -> Option<(String, bool)> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    let truncated = bytes.len() > cap;
    bytes.truncate(cap);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated && text.ends_with('\u{FFFD}') {
        text.pop(); // a multibyte character cut by the cap
    }
    Some((text, truncated))
}

pub(super) fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    fs::canonicalize(home).ok()
}

/// Directories whose instruction files apply to `root`, nearest first. The walk climbs to the nearest
/// directory containing `.git`, or to `home` when the root lives under it; with neither it stays in
/// `root`. It never goes above `home` or more than `MAX_WALK_LEVELS` levels.
pub(super) fn walk_dirs<'a>(root: &'a Path, home: Option<&Path>) -> Vec<&'a Path> {
    let under_home = home.is_some_and(|h| root.starts_with(h));
    let mut dirs: Vec<&Path> = Vec::new();
    let mut found_boundary = false;
    for dir in root.ancestors().take(MAX_WALK_LEVELS) {
        dirs.push(dir);
        if dir.join(".git").exists() {
            found_boundary = true;
            break;
        }
        if home.is_some_and(|h| dir == h) {
            found_boundary = true;
            break;
        }
    }
    if !found_boundary && !under_home {
        dirs.truncate(1);
    }
    dirs
}

// ---------------------------------------------------------------- ambient context

/// Project instruction files from the directory tree around the project plus user-level
/// `AGENTS.md`, outermost first. See [`discover_with_foreign`].
/// Portable GitHub/Cursor/native rules are selected separately by `rules::Catalog` at request
/// and pre-tool boundaries. Never concatenate their path/agent-scoped bodies into ambient context.
/// `overlap_dedup` mirrors `experimental.context_overlap_dedup`: when set, paragraphs repeated in a
/// later file are also removed from earlier files (see [`dedupe_overlapping_paragraphs`]).
pub fn discover(
    layout: &Layout,
    overlap_dedup: bool,
    foreign_sources: &[String],
) -> Vec<ContextFile> {
    let foreign: Vec<&str> = foreign_sources.iter().map(String::as_str).collect();
    discover_with_foreign(layout, home_dir().as_deref(), overlap_dedup, &foreign)
}

const PER_DIR_FILES: [&str; 4] = ["AGENTS.md", "CLAUDE.md", ".cursorrules", ".dial/AGENTS.md"];

/// Read only from the working directory; ancestor directories are not consulted.
const ROOT_ONLY_FILES: [&str; 1] = [".gemini/GEMINI.md"];

/// Foreign user-level instruction files, highest precedence first (priorities: codex 70, gemini 60).
const FOREIGN_USER_FILES: [(&str, &str); 2] = [
    ("codex", ".codex/AGENTS.md"),
    ("gemini", ".gemini/GEMINI.md"),
];

/// The walk starts at the project root and climbs to the nearest directory containing `.git`. With no
/// git root it climbs to `home` when the project lives under it, and otherwise stays in the project
/// root: instruction files in arbitrary ancestors (`/tmp`, `/`) are not trusted. It never goes above
/// `home` or more than 8 levels. Nearest files win the size budget and duplicates; the result is
/// ordered outermost first so the most specific guidance comes last (and wins).
#[cfg(test)]
fn discover_with_home(
    layout: &Layout,
    home: Option<&Path>,
    overlap_dedup: bool,
) -> Vec<ContextFile> {
    discover_with_foreign(layout, home, overlap_dedup, &[])
}

/// Same walk as [`discover_with_home`]. `foreign_user` lists opted-in foreign providers (`codex`,
/// `gemini`) whose user-level files may stand in for the user `AGENTS.md`. Unknown names match no
/// provider and are ignored. Production passes `config.foreign_sources`; every foreign user provider
/// is off unless listed there.
fn discover_with_foreign(
    layout: &Layout,
    home: Option<&Path>,
    overlap_dedup: bool,
    foreign_user: &[&str],
) -> Vec<ContextFile> {
    let root = layout.project_root.as_path();
    let dirs = walk_dirs(root, home);
    let boundary = dirs.last().copied().unwrap_or(root);

    let mut budget = TOTAL_CAP;
    let mut seen: HashSet<String> = HashSet::new();
    let mut groups: Vec<Vec<ContextFile>> = Vec::new();
    for dir in &dirs {
        let mut group = Vec::new();
        for name in PER_DIR_FILES {
            if let Some(file) = load_walked(
                &dir.join(name),
                boundary,
                layout,
                home,
                &mut budget,
                &mut seen,
            ) {
                group.push(file);
            }
        }
        groups.push(group);
    }
    // `dirs` starts at the project root, so the first group belongs to the working directory.
    for name in ROOT_ONLY_FILES {
        if let (Some(file), Some(group)) = (
            load_walked(
                &root.join(name),
                boundary,
                layout,
                home,
                &mut budget,
                &mut seen,
            ),
            groups.first_mut(),
        ) {
            group.push(file);
        }
    }
    let user_path = layout.user_dir.join("AGENTS.md");
    let user = if user_path.exists() {
        load_context_file(
            &user_path,
            format!("{} (user)", display_for(layout, home, &user_path)),
            &mut budget,
            &mut seen,
        )
    } else {
        foreign_user_file(layout, home, foreign_user, &mut budget, &mut seen)
    };

    let mut out: Vec<ContextFile> = user.into_iter().collect();
    out.extend(groups.into_iter().rev().flatten());
    let out = dedupe_contained_context(out);
    if overlap_dedup {
        dedupe_overlapping_paragraphs(out)
    } else {
        out
    }
}

/// Keep precedence order and original text; only remove an earlier file when its
/// complete paragraph sequence occurs in a later, more specific file. This is a
/// conservative context-containment check. Never fuzzy-match instructions.
fn dedupe_contained_context(files: Vec<ContextFile>) -> Vec<ContextFile> {
    let blocks: Vec<Vec<&str>> = files
        .iter()
        .map(|file| {
            file.content
                .split("\n\n")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .collect();
    let keep: Vec<bool> = blocks
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            rule.is_empty()
                || !blocks[i + 1..].iter().any(|candidate| {
                    candidate.len() >= rule.len()
                        && candidate
                            .windows(rule.len())
                            .any(|window| window == rule.as_slice())
                })
        })
        .collect();
    files
        .into_iter()
        .zip(keep)
        .filter_map(|(file, keep)| keep.then_some(file))
        .collect()
}

/// Exact paragraph projection for `experimental.context_overlap_dedup`, applied after containment.
/// Paragraphs are split on blank lines and compared trimmed. An earlier file loses a paragraph only
/// when the same paragraph is a whole paragraph of a later (more specific) file. Entries, labels and
/// order are kept; retained paragraphs keep their original text. A paragraph carrying the truncation
/// notice is never removed. Work is one pass over the loaded text.
fn dedupe_overlapping_paragraphs(files: Vec<ContextFile>) -> Vec<ContextFile> {
    let mut projected = vec![String::new(); files.len()];
    let mut later: HashSet<&str> = HashSet::new();
    for (index, file) in files.iter().enumerate().rev() {
        let mut kept: Vec<&str> = Vec::new();
        for paragraph in file.content.split("\n\n") {
            let key = paragraph.trim();
            if key.is_empty() || paragraph.contains(TRUNCATION_NOTICE) || !later.contains(key) {
                kept.push(paragraph);
            }
        }
        projected[index] = kept.join("\n\n");
        later.extend(
            file.content
                .split("\n\n")
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        );
    }
    files
        .into_iter()
        .zip(projected)
        .map(|(mut file, content)| {
            file.content = content;
            file
        })
        .collect()
}

fn load_context_file(
    path: &Path,
    display: String,
    budget: &mut usize,
    seen: &mut HashSet<String>,
) -> Option<ContextFile> {
    let (mut content, cut) = read_text(path, FILE_CAP)?;
    if content.trim().is_empty() || *budget == 0 || !seen.insert(content.clone()) {
        return None;
    }
    let mut cut = cut;
    if content.len() > *budget {
        let mut end = *budget;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        cut = true;
    }
    *budget -= content.len();
    if cut {
        content.push_str(&format!("\n{TRUNCATION_NOTICE}"));
    }
    Some(ContextFile {
        path: path.to_path_buf(),
        display,
        content,
    })
}

/// Reads one walked instruction file. A symlink may point anywhere, so it is followed only when it
/// stays inside the walked tree.
fn load_walked(
    path: &Path,
    boundary: &Path,
    layout: &Layout,
    home: Option<&Path>,
    budget: &mut usize,
    seen: &mut HashSet<String>,
) -> Option<ContextFile> {
    match fs::canonicalize(path) {
        Ok(real) if real.starts_with(boundary) => {}
        _ => return None,
    }
    load_context_file(path, display_for(layout, home, path), budget, seen)
}

/// The user instructions from the first listed, opted-in foreign provider file that exists. Like
/// the user scope, the first file present decides even when it loads empty. Files resolving
/// outside `home` are refused.
fn foreign_user_file(
    layout: &Layout,
    home: Option<&Path>,
    enabled: &[&str],
    budget: &mut usize,
    seen: &mut HashSet<String>,
) -> Option<ContextFile> {
    let home = home?;
    let real_home = fs::canonicalize(home).ok()?;
    for (provider, rel) in FOREIGN_USER_FILES {
        let path = home.join(rel);
        if !enabled.contains(&provider) || !path.exists() {
            continue;
        }
        match fs::canonicalize(&path) {
            Ok(real) if real.starts_with(&real_home) => {}
            _ => continue,
        }
        let display = format!("{} (user)", display_for(layout, Some(home), &path));
        return load_context_file(&path, display, budget, seen);
    }
    None
}

fn display_for(layout: &Layout, home: Option<&Path>, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(&layout.project_root) {
        return rel.display().to_string();
    }
    if let Some(rel) = home.and_then(|h| path.strip_prefix(h).ok()) {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

/// `<user_dir>/SOUL.md`: the agent's identity and tone.
pub fn soul(layout: &Layout) -> Option<String> {
    let (text, cut) = read_text(&layout.user_dir.join("SOUL.md"), FILE_CAP)?;
    let text = text.trim();
    (!text.is_empty()).then(|| {
        if cut {
            format!("{text}\n[... truncated ...]")
        } else {
            text.to_string()
        }
    })
}

/// (`.dial/SYSTEM.md` replacing the base system prompt, `.dial/APPEND_SYSTEM.md` appended to it).
pub fn system_overrides(layout: &Layout) -> (Option<String>, Option<String>) {
    let load = |name: &str| {
        let (text, cut) = read_text(&layout.project_dir.join(name), FILE_CAP)?;
        let text = text.trim();
        (!text.is_empty()).then(|| {
            if cut {
                format!("{text}\n[... truncated ...]")
            } else {
                text.to_string()
            }
        })
    };
    (load("SYSTEM.md"), load("APPEND_SYSTEM.md"))
}

// ---------------------------------------------------------------- @references

/// Expands `@path`, `@dir/`, `@diff` and `@staged` references: the text is returned unchanged with
/// the referenced content appended as `<file>` / `<dir>` / `<diff>` blocks. Paths are confined to
/// `root` (absolute paths, `..`, and symlinks leading out are refused). Unresolvable references stay
/// as typed; a warning is produced only when the token looks like a path, so `a@b.com` and
/// `@someone` pass silently.
pub fn expand_references(root: &Path, text: &str, interrupt: &AtomicBool) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut total = 0usize;
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

    for token in reference_tokens(text) {
        if !seen.insert(token) {
            continue;
        }
        if seen.len() > MAX_REFS {
            warnings.push(format!(
                "more than {MAX_REFS} @references; ignoring the rest"
            ));
            break;
        }
        let block = match token {
            _ if crate::llm::images::is_image(Path::new(token)) => None,
            "diff" => git_block(&canonical_root, false, &mut warnings, interrupt),
            "staged" => git_block(&canonical_root, true, &mut warnings, interrupt),
            _ => path_block(&canonical_root, token, &mut warnings),
        };
        if let Some(block) = block {
            if total + block.len() > REF_TOTAL_CAP {
                warnings.push(format!(
                    "@{token} skipped: references exceed {} KiB in total",
                    REF_TOTAL_CAP / 1024
                ));
                continue;
            }
            total += block.len();
            blocks.push(block);
        }
    }
    if blocks.is_empty() {
        return (text.to_string(), warnings);
    }
    (format!("{text}\n\n{}", blocks.join("\n\n")), warnings)
}

/// Expand text references and retain compact image attachments for provider encoding.
pub fn expand_message(
    root: &Path,
    text: &str,
    interrupt: &AtomicBool,
) -> (serde_json::Value, Vec<String>) {
    let (content, mut warnings) = expand_references(root, text, interrupt);
    let mut images = Vec::new();
    let mut seen = HashSet::new();
    for token in reference_tokens(text)
        .into_iter()
        .chain(pasted_image_tokens(text))
        .filter(|token| seen.insert(*token))
        .take(MAX_REFS)
    {
        if !crate::llm::images::is_image(Path::new(token)) {
            continue;
        }
        match crate::llm::images::user_attachment(root, Path::new(token)) {
            Ok(image) => images.push(image),
            Err(error) => warnings.push(format!("@{token} skipped: {error}")),
        }
    }
    let mut message = serde_json::json!({"role":"user","content":content});
    if !images.is_empty() {
        message["dial_images"] = serde_json::Value::Array(images);
    }
    (message, warnings)
}

/// Terminal drag/drop and clipboard paths need no @ prefix. Only standalone local image paths
/// in the human's message are eligible; ordinary prose, URLs and non-image files stay untouched.
fn pasted_image_tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || "([{,;".contains(c));
        if rest.is_empty() {
            break;
        }
        let (token, consumed) = if let Some(quote @ ('\'' | '"' | '`')) = rest.chars().next() {
            match rest[1..].find(quote) {
                Some(end) => (&rest[1..end + 1], end + 2),
                None => break,
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], end)
        };
        let token = token.trim_end_matches([',', ';', '.', ')', ']', '}']);
        if (token.starts_with('/') || token.starts_with("~/") || token.starts_with("./"))
            && crate::llm::images::is_image(Path::new(token))
        {
            out.push(token);
        }
        rest = &rest[consumed..];
    }
    out
}

/// Tokens following an `@` that starts a word (so `user@host` is skipped), minus trailing punctuation.
fn reference_tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut consumed_until = 0;
    for (i, _) in text.match_indices('@') {
        if i < consumed_until {
            continue;
        }
        let starts_word = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || "([{\"'`,;:".contains(c));
        if !starts_word {
            continue;
        }
        let rest = &text[i + 1..];
        if let Some(quote @ ('"' | '\'')) = rest.chars().next() {
            if let Some(end) = rest[1..].find(quote) {
                consumed_until = i + 3 + end;
                let token = &rest[1..1 + end];
                if !token.is_empty() {
                    out.push(token);
                }
            }
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '\0')
            .unwrap_or(rest.len());
        let token = rest[..end]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '`']);
        if !token.is_empty() {
            out.push(token);
        }
    }
    out
}

fn looks_like_path(token: &str) -> bool {
    token.contains('/') || token.contains('.') || token.starts_with('~')
}

fn xml_attr(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn path_block(root: &Path, token: &str, warnings: &mut Vec<String>) -> Option<String> {
    let rel = Path::new(token);
    let path_like = looks_like_path(token);
    let escapes = rel.is_absolute()
        || token.starts_with('~')
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)));
    if escapes {
        if path_like {
            warnings.push(format!(
                "@{token} ignored: references must stay inside the project"
            ));
        }
        return None;
    }
    let Ok(real) = fs::canonicalize(root.join(rel)) else {
        if path_like {
            warnings.push(format!("@{token} not found"));
        }
        return None;
    };
    if !real.starts_with(root) {
        warnings.push(format!("@{token} ignored: it resolves outside the project"));
        return None;
    }
    let shown = xml_attr(token);
    if real.is_dir() {
        return Some(format!(
            "<dir path=\"{shown}\">\n{}</dir>",
            dir_listing(&real)
        ));
    }
    match read_text(&real, REF_FILE_CAP) {
        Some((body, cut)) => {
            let body = body.replace("</file>", "<\\/file>");
            let marker = if cut {
                format!("\n[... truncated at {} KiB ...]", REF_FILE_CAP / 1024)
            } else {
                String::new()
            };
            Some(format!("<file path=\"{shown}\">\n{body}{marker}\n</file>"))
        }
        None => {
            warnings.push(format!("@{token} skipped: not a readable text file"));
            None
        }
    }
}

fn dir_listing(dir: &Path) -> String {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".git")
            .map(|e| {
                let mut name: String = e
                    .file_name()
                    .to_string_lossy()
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    name.push('/');
                }
                name
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    let extra = names.len().saturating_sub(DIR_LISTING_CAP);
    names.truncate(DIR_LISTING_CAP);
    let mut out = names.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    if extra > 0 {
        out.push_str(&format!("[... {extra} more entries ...]\n"));
    }
    out
}

fn git_block(
    root: &Path,
    staged: bool,
    warnings: &mut Vec<String>,
    interrupt: &AtomicBool,
) -> Option<String> {
    let label = if staged { "staged" } else { "diff" };
    match run_git_diff(root, staged, interrupt) {
        Ok((out, _)) if out.trim().is_empty() => {
            warnings.push(format!("@{label}: no changes"));
            None
        }
        Ok((out, cut)) => {
            let marker = if cut {
                format!("\n[... truncated at {} KiB ...]", GIT_OUTPUT_CAP / 1024)
            } else {
                String::new()
            };
            let out = out.replace("</diff>", "<\\/diff>");
            Some(format!("<diff kind=\"{label}\">\n{out}{marker}\n</diff>"))
        }
        Err(why) => {
            warnings.push(format!("@{label}: {why}"));
            None
        }
    }
}

/// `git diff [--staged]` with bounded output and time. Returns (text, truncated).
fn run_git_diff(
    root: &Path,
    staged: bool,
    interrupt: &AtomicBool,
) -> Result<(String, bool), String> {
    let mut cmd = Command::new("git");
    // Repo config is untrusted: switch off every hook `git diff` honours that runs a program.
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.pager=cat",
        "--no-pager",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
    ]);
    if staged {
        cmd.arg("--staged");
    }
    cmd.current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("could not run git: {e}"))?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not capture git output".into());
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout)
            .take(GIT_OUTPUT_CAP as u64 + 1)
            .read_to_end(&mut buf);
        drop(stdout); // closing the pipe early makes a still-writing git die of SIGPIPE
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    // Short slices so an interrupt is observed promptly rather than after the whole timeout.
    let captured = loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(buf) => break Some(buf),
            Err(mpsc::RecvTimeoutError::Timeout)
                if Instant::now() < deadline && !interrupt.load(Ordering::SeqCst) => {}
            Err(_) => break None,
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None)
                if Instant::now() < deadline
                    && captured.is_some()
                    && !interrupt.load(Ordering::SeqCst) =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if interrupt.load(Ordering::SeqCst) {
        return Err("git diff interrupted".into());
    }
    let mut bytes = captured.ok_or_else(|| "git diff timed out".to_string())?;
    let cut = bytes.len() > GIT_OUTPUT_CAP;
    bytes.truncate(GIT_OUTPUT_CAP);
    // Output capped early means we killed git ourselves; that is success, not failure.
    if !cut && !status.is_some_and(|s| s.success()) {
        return Err("git diff failed (not a git repository?)".into());
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if cut && text.ends_with('\u{FFFD}') {
        text.pop();
    }
    Ok((text, cut))
}

// ---------------------------------------------------------------- slash commands

#[derive(Debug, Clone)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub template: String,
    #[cfg(test)]
    pub source: PathBuf,
}

/// Custom slash commands from `<project_dir>/commands/*.md` and `<user_dir>/commands/*.md`; a project
/// command shadows a user command of the same name. Sorted by name.
pub fn discover_commands(layout: &Layout) -> Vec<CommandDef> {
    let mut out: Vec<CommandDef> = Vec::new();
    for dir in [
        layout.project_dir.join("commands"),
        layout.user_dir.join("commands"),
    ] {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|n| valid_command_name(n))
            else {
                continue;
            };
            if out.iter().any(|c| c.name == name) {
                continue;
            }
            let Some((raw, _)) = read_text(&path, COMMAND_FILE_CAP) else {
                continue;
            };
            let (description, template) = split_frontmatter(&raw);
            if template.trim().is_empty() {
                continue;
            }
            let description = description.unwrap_or_else(|| {
                let first = template
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                match first.char_indices().nth(80) {
                    Some((i, _)) => format!("{}…", &first[..i]),
                    None => first.to_string(),
                }
            });
            out.push(CommandDef {
                name: name.to_string(),
                description,
                template,
                #[cfg(test)]
                source: path,
            });
        }
    }
    // Shipped defaults; a project or user command with the same name wins.
    for (name, description, template) in DEFAULT_COMMANDS {
        if !out.iter().any(|c| c.name == *name) {
            out.push(CommandDef {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                template: (*template).to_owned(),
                #[cfg(test)]
                source: PathBuf::from("(built-in)"),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

const DEFAULT_COMMANDS: &[(&str, &str, &str)] = &[
    (
        "review",
        "read-only review of uncommitted, base, ref, commit or authenticated GitHub PR snapshots",
        "Review $ARGUMENTS using the immutable read-only review workflow. Identify concrete introduced defects, rank numeric priorities P0 through P3, state confidence and inclusive target-relative line ranges, and give an explicit verdict. Do not invent findings or claim verification that did not run. Do not modify files, run commands, commit or push.",
    ),
    (
        "commit-work",
        "stage the relevant changes and commit them",
        "Commit the current work. Use the git tool: check status and diff, stage only files that belong to this change (never secrets or build output), write a concise message in the style of `log`, then commit. Do not push. $ARGUMENTS",
    ),
    (
        "pr",
        "push the branch and open a pull request",
        "Open a pull request for the current branch. Use the git tool: check status and log against the base branch, make sure everything intended is committed, push, then pr_create with a clear title and a body that summarises what changed and why. Do not force-push. $ARGUMENTS",
    ),
];

fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Splits an optional `---\ndescription: ...\n---\n` header off the template.
fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (None, raw.to_string());
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let header = &rest[..offset];
            let body = &rest[offset + line.len()..];
            let description = header.lines().find_map(|l| {
                let (key, value) = l.split_once(':')?;
                (key.trim() == "description")
                    .then(|| value.trim().trim_matches(['"', '\'']).to_string())
            });
            return (description.filter(|d| !d.is_empty()), body.to_string());
        }
        offset += line.len();
    }
    (None, raw.to_string()) // unterminated header: treat the whole file as the template
}

/// Substitutes `$ARGUMENTS` / `$@` (the raw argument string) and `$1`..`$9` (shell-style words;
/// missing ones become empty) in one pass, so arguments containing `$1` are not re-expanded. A
/// template with no placeholder gets the arguments appended.
pub fn expand_command(def: &CommandDef, args: &str) -> String {
    let args = args.trim();
    let words = split_words(args);
    let mut out = String::with_capacity(def.template.len() + args.len());
    let mut used = false;
    let mut rest = def.template.as_str();
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(r) = after
            .strip_prefix("ARGUMENTS")
            .or_else(|| after.strip_prefix('@'))
        {
            out.push_str(args);
            used = true;
            rest = r;
        } else if let Some(d) = after
            .chars()
            .next()
            .and_then(|c| c.to_digit(10))
            .filter(|d| *d >= 1)
        {
            out.push_str(words.get(d as usize - 1).map(String::as_str).unwrap_or(""));
            used = true;
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    if !used && !args.is_empty() {
        out.push_str("\n\n");
        out.push_str(args);
    }
    out
}
#[cfg(test)]
mod tests;
