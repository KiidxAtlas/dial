//! Tool surface of the agentic commit loop: schemas, handlers, and per-run state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::{Value as Json, json};

use super::lock_files::is_lock_file;
use super::state::{
    ChangelogProposal, ChangelogTargetProposal, CommitAgentState, CommitProposal,
    ConventionalAnalysis, ConventionalDetail, FileChange, GitOverviewSnapshot, NumstatEntry,
    SplitCommitGroup, SplitCommitPlan,
};
use super::template::{Value, render};
use super::topo_sort::compute_dependency_order;
use super::validation::{
    MAX_DETAIL_ITEMS, SUMMARY_MAX_CHARS, cap_details, is_test_path, normalize_summary,
    parse_category, validate_analysis, validate_summary_rules, validate_type_consistency,
};
use crate::commit::ai_stage::map_bounded;
use crate::commit::changelog::parse::normalize_items;
use crate::commit::commit_types::{COMMIT_TYPE_ORDER, canonical_commit_type, coerce_commit_type};
use crate::commit::config::GenerationConfig;
use crate::commit::diff::{FileStatus, parse_file_diffs, parse_file_hunks, parse_prompt_diff};
use crate::commit::execute::{git_output, git_output_indexed};
use crate::commit::run::{CommitBackend, CommitError};
use crate::commit::scope::extract_scope_candidates;
use crate::config::types::Config;
use crate::llm::Chat;

const ANALYZE_TEMPLATE: &str = include_str!("prompts/analyze-file.md");
const ANALYZE_MAX_TOKENS: u32 = 2048;
const ANALYZE_TEMPERATURE: f64 = 0.2;

/// Character budget for `git_file_diff` output (a 30k-token target, about 120k characters).
const DIFF_MAX_CHARS: usize = 120_000;
/// Diffs longer than this many lines are elided in the middle.
const TRUNCATE_THRESHOLD_LINES: usize = 30;
const KEEP_HEAD_LINES: usize = 15;
const KEEP_TAIL_LINES: usize = 10;

const RECENT_MIN: u64 = 1;
const RECENT_MAX: u64 = 50;
const RECENT_DEFAULT: u64 = 8;
const TOP_LIST: usize = 5;

const BINARY_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".ico", ".woff", ".woff2", ".ttf", ".eot", ".pdf", ".zip",
    ".tar", ".gz", ".exe", ".dll", ".so", ".dylib",
];
const LOW_PRIORITY_EXTENSIONS: &[&str] = &[
    ".md", ".txt", ".json", ".yaml", ".yml", ".toml", ".xml", ".csv",
];
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "Gemfile",
    "build.gradle",
    "pom.xml",
];
const SCRIPT_EXTENSIONS: &[&str] = &[".sh", ".bash", ".zsh", ".sql"];
const HIGH_PRIORITY_EXTENSIONS: &[&str] = &[
    ".rs", ".go", ".py", ".js", ".ts", ".tsx", ".jsx", ".java", ".c", ".cpp", ".h", ".hpp",
];
const TEST_MARKERS: &[&str] = &[
    "/test/",
    "/tests/",
    "/__tests__/",
    "_test.",
    ".test.",
    ".spec.",
    "_spec.",
];

/// Everything a tool handler needs for one agentic run.
pub(crate) struct ToolEnv<'a> {
    pub(crate) root: &'a Path,
    pub(crate) config: &'a Config,
    pub(crate) backend: &'a dyn CommitBackend,
    /// Staged paths, relative to the repository root.
    pub(crate) staged: Vec<String>,
    /// `git diff --cached --numstat` rows for the staged changes.
    pub(crate) numstat: Vec<NumstatEntry>,
    /// Changelog files that the flow may update.
    pub(crate) changelog_targets: Vec<String>,
    /// Index the run reads and writes: the repository's own, or a private copy for a preview.
    pub(crate) index: Option<PathBuf>,
    /// Set by the caller to stop `analyze_files` from admitting further model calls.
    pub(crate) cancel: &'a AtomicBool,
    pub(crate) state: CommitAgentState,
}

/// OpenAI function schemas for the eight commit tools.
pub(crate) fn tool_schemas() -> Vec<Json> {
    let category_items = json!({"type": "string", "enum": ["Breaking Changes", "Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]});
    let detail = json!({
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "One sentence ending with a period, at most 120 characters."},
            "changelog_category": category_items,
            "user_visible": {"type": "boolean"}
        },
        "required": ["text"]
    });
    let string_list = json!({"type": "array", "items": {"type": "string"}});
    vec![
        function(
            "git_overview",
            "List the staged files with their diff stat, numstat, and scope candidates.",
            json!({
                "type": "object",
                "properties": {
                    "staged": {"type": "boolean", "description": "Use the staged diff (default true)."},
                    "include_untracked": {"type": "boolean", "description": "Also list untracked files (default false)."}
                }
            }),
        ),
        function(
            "git_file_diff",
            "Read the diffs of the given files, ordered by importance and truncated to fit a budget.",
            json!({
                "type": "object",
                "properties": {
                    "files": string_list,
                    "staged": {"type": "boolean", "description": "Use the staged diff (default true)."}
                },
                "required": ["files"]
            }),
        ),
        function(
            "git_hunk",
            "Read hunks of one file's diff, optionally filtered by 1-based hunk number.",
            json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "hunks": {"type": "array", "items": {"type": "integer", "minimum": 1}},
                    "staged": {"type": "boolean", "description": "Use the staged diff (default true)."}
                },
                "required": ["file"]
            }),
        ),
        function(
            "recent_commits",
            "Show recent commit subjects with style statistics, for matching the repository's conventions.",
            json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "minimum": RECENT_MIN, "maximum": RECENT_MAX}
                }
            }),
        ),
        function(
            "analyze_files",
            "Analyze the changed files one by one and describe what changed in each.",
            json!({
                "type": "object",
                "properties": {
                    "files": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "goal": {"type": "string", "description": "What the analysis should focus on."}
                },
                "required": ["files"]
            }),
        ),
        function(
            "propose_commit",
            "Propose a single commit covering all staged changes.",
            json!({
                "type": "object",
                "properties": {
                    "type": {"type": "string", "enum": COMMIT_TYPE_ORDER},
                    "scope": {"type": ["string", "null"]},
                    "summary": {"type": "string", "description": "Past-tense summary, at most 72 characters, no trailing period."},
                    "details": {"type": "array", "items": detail},
                    "issue_refs": string_list
                },
                "required": ["type", "scope", "summary"]
            }),
        ),
        function(
            "split_commit",
            "Propose several commits that together cover all staged changes, with dependencies between them.",
            json!({
                "type": "object",
                "properties": {
                    "commits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "changes": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "path": {"type": "string"},
                                            "kind": {"type": "string", "enum": ["all", "indices", "lines"]},
                                            "indices": {"type": "array", "items": {"type": "integer", "minimum": 1}},
                                            "start": {"type": "integer", "minimum": 1},
                                            "end": {"type": "integer", "minimum": 1}
                                        },
                                        "required": ["path", "kind"]
                                    }
                                },
                                "type": {"type": "string", "enum": COMMIT_TYPE_ORDER},
                                "scope": {"type": ["string", "null"]},
                                "summary": {"type": "string"},
                                "details": {"type": "array", "items": detail},
                                "issue_refs": string_list,
                                "rationale": {"type": "string"},
                                "dependencies": {
                                    "type": "array",
                                    "items": {"type": "integer", "minimum": 0},
                                    "description": "0-based indices of commits that must land first."
                                }
                            },
                            "required": ["changes", "type", "scope", "summary"]
                        }
                    }
                },
                "required": ["commits"]
            }),
        ),
        function(
            "propose_changelog",
            "Propose Unreleased changelog entries for each changelog file the flow asked about.",
            json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "entries": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": "string"}}},
                                "deletions": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": "string"}}}
                            },
                            "required": ["path", "entries"]
                        }
                    }
                },
                "required": ["entries"]
            }),
        ),
    ]
}

fn function(name: &str, description: &str, parameters: Json) -> Json {
    json!({"type": "function", "function": {"name": name, "description": description, "parameters": parameters}})
}

/// Run one tool call. Handler errors are returned as text so the model can correct itself.
pub(crate) fn execute(env: &mut ToolEnv<'_>, name: &str, args: &Json) -> String {
    let result = match name {
        "git_overview" => git_overview(env, args),
        "git_file_diff" => git_file_diff(env, args),
        "git_hunk" => git_hunk(env, args),
        "recent_commits" => recent_commits(env.root, args),
        "analyze_files" => analyze_files(env, args),
        "propose_commit" => propose_commit(env, args),
        "split_commit" => split_commit(env, args),
        "propose_changelog" => propose_changelog(env, args),
        other => Err(format!("Unknown tool: {other}")),
    };
    match result {
        Ok(text) => text,
        Err(error) => format!("Error: {error}"),
    }
}

fn message(error: CommitError) -> String {
    match error {
        CommitError::Model(text) | CommitError::Git(text) | CommitError::Validation(text) => text,
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    git_output(root, args, "Git command failed").map_err(message)
}

/// Git read of the run's staged state. It uses the run's index, so a preview never reads the
/// user's own index.
fn git_index(env: &ToolEnv<'_>, args: &[&str]) -> Result<String, String> {
    git_output_indexed(env.root, env.index.as_deref(), args, "Git command failed").map_err(message)
}

/// `git diff [--cached] <extra>` arguments.
fn diff_args<'a>(staged: bool, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args: Vec<&'a str> = vec!["diff"];
    if staged {
        args.push("--cached");
    }
    args.extend_from_slice(extra);
    args
}

fn opt_bool(args: &Json, key: &str, default: bool) -> bool {
    args.get(key).and_then(Json::as_bool).unwrap_or(default)
}

fn str_list(args: &Json, key: &str) -> Result<Vec<String>, String> {
    let items = args
        .get(key)
        .and_then(Json::as_array)
        .ok_or_else(|| format!("{key} must be an array of strings"))?;
    items
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must be an array of strings"))
        })
        .collect()
}

fn string_list_opt(args: &Json, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn trimmed_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Extension including its leading dot, or `""`.
fn extension(name: &str) -> &str {
    name.rfind('.').map_or("", |index| &name[index..])
}

/// Parse `git diff --numstat` rows (`added\tdeleted\tpath`). Binary rows carry `None` counts.
pub(crate) fn parse_numstat(raw: &str) -> Vec<NumstatEntry> {
    raw.split('\0')
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let additions = parts.next()?;
            let deletions = parts.next()?;
            let path = parts.next()?;
            Some(NumstatEntry {
                path: path.to_owned(),
                additions: additions.parse().ok(),
                deletions: deletions.parse().ok(),
            })
        })
        .collect()
}

/// Number of hunks `path` has in the unified diff `diff_text`.
pub(crate) fn hunk_count(diff_text: &str, path: &str) -> usize {
    parse_file_diffs(diff_text)
        .iter()
        .find(|file| file.filename == path)
        .map_or(0, |file| parse_file_hunks(file).len())
}

/// Added and deleted files are committed whole: a partial hunk selection would apply half a file.
fn whole_file_only(diff_text: &str, path: &str) -> bool {
    parse_prompt_diff(diff_text)
        .iter()
        .find(|file| file.filename == path)
        .is_some_and(|file| matches!(file.status, FileStatus::Added | FileStatus::Deleted))
}

/// Priority for diff truncation: higher first (ranking used by `git_file_diff`).
fn file_priority(path: &str) -> i32 {
    let lower = path.to_ascii_lowercase();
    let base = basename(&lower);
    let ext = extension(base);
    if BINARY_EXTENSIONS.contains(&ext) {
        return -100;
    }
    let anchored = format!("/{lower}");
    if TEST_MARKERS.iter().any(|marker| anchored.contains(marker)) {
        return 10;
    }
    let manifest = MANIFESTS
        .iter()
        .any(|name| name.eq_ignore_ascii_case(basename(path)));
    if LOW_PRIORITY_EXTENSIONS.contains(&ext) && !manifest {
        return 20;
    }
    if manifest {
        return 70;
    }
    if SCRIPT_EXTENSIONS.contains(&ext) {
        return 80;
    }
    if HIGH_PRIORITY_EXTENSIONS.contains(&ext) {
        return 100;
    }
    50
}

/// Short type label shown to the analysis model.
fn file_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let base = basename(&lower);
    let ext = extension(base);
    if BINARY_EXTENSIONS.contains(&ext) {
        "binary"
    } else if is_test_path(&lower) {
        "test"
    } else if ext == ".md" || ext == ".txt" {
        "documentation"
    } else if [".json", ".yaml", ".yml", ".toml"].contains(&ext) {
        "configuration"
    } else if MANIFESTS.iter().any(|name| name.eq_ignore_ascii_case(base)) {
        "dependency manifest"
    } else if SCRIPT_EXTENSIONS.contains(&ext) {
        "script"
    } else if HIGH_PRIORITY_EXTENSIONS.contains(&ext) {
        "implementation"
    } else {
        "source file"
    }
}

/// Keep the head and tail of a long diff, eliding the middle.
fn truncate_lines(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= TRUNCATE_THRESHOLD_LINES {
        return content.to_owned();
    }
    let head = lines[..KEEP_HEAD_LINES].join("\n");
    let tail = lines[lines.len() - KEEP_TAIL_LINES..].join("\n");
    let elided = lines.len() - KEEP_HEAD_LINES - KEEP_TAIL_LINES;
    format!("{head}\n[…{elided}ln elided…]\n{tail}")
}

/// Join sections in order, cutting the last one that exceeds the character budget.
fn assemble_sections(sections: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;
    for section in sections {
        let len = section.chars().count();
        if used + len <= DIFF_MAX_CHARS {
            used += len;
            parts.push(section.clone());
            continue;
        }
        let left = DIFF_MAX_CHARS.saturating_sub(used);
        let head: String = section.chars().take(left).collect();
        parts.push(format!("{head}\n[…{}ch elided…]", len - left));
        break;
    }
    if parts.is_empty() {
        return "(no output)".to_owned();
    }
    parts.join("\n\n")
}

fn lines_changed(env: &ToolEnv<'_>, path: &str) -> u64 {
    env.numstat
        .iter()
        .find(|entry| entry.path == path)
        .map_or(0, |entry| {
            entry.additions.unwrap_or(0) + entry.deletions.unwrap_or(0)
        })
}

/// Cached per-file diff of the staged or working-tree changes.
fn file_diff(env: &mut ToolEnv<'_>, file: &str, staged: bool) -> Result<String, String> {
    let key = format!("{file}:{staged}");
    if let Some(cached) = env.state.diff_cache.get(&key) {
        return Ok(cached.clone());
    }
    let pathspec = format!(":(literal){file}");
    let args = diff_args(
        staged,
        &[
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "--",
            pathspec.as_str(),
        ],
    );
    let diff = git_index(env, &args)?;
    env.state.diff_cache.insert(key, diff.clone());
    Ok(diff)
}

fn git_overview(env: &mut ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let staged = opt_bool(args, "staged", true);
    let include_untracked = opt_bool(args, "include_untracked", false);
    let names = git_index(
        env,
        &diff_args(staged, &["--name-only", "--no-renames", "--no-color", "-z"]),
    )?;
    let stat = git_index(env, &diff_args(staged, &["--stat", "--no-color"]))?;
    let numstat_raw = git_index(
        env,
        &diff_args(staged, &["--numstat", "--no-renames", "-z"]),
    )?;

    let files: Vec<String> = names
        .split('\0')
        .filter(|line| !line.is_empty() && !is_lock_file(line))
        .map(str::to_owned)
        .collect();
    let excluded_files: Vec<String> = names
        .split('\0')
        .filter(|line| !line.is_empty() && is_lock_file(line))
        .map(str::to_owned)
        .collect();
    let numstat: Vec<NumstatEntry> = parse_numstat(&numstat_raw)
        .into_iter()
        .filter(|entry| !is_lock_file(&entry.path))
        .collect();
    let kept_numstat_raw: String = numstat_raw
        .split('\0')
        .filter(|line| {
            line.splitn(3, '\t')
                .nth(2)
                .is_none_or(|path| !is_lock_file(path))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let scope = extract_scope_candidates(&kept_numstat_raw, &GenerationConfig::default());
    let untracked_files: Vec<String> = if include_untracked {
        git_index(env, &["ls-files", "--others", "--exclude-standard", "-z"])?
            .split('\0')
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };

    let out = json!({
        "files": files,
        "stat": stat,
        "numstat": numstat
            .iter()
            .map(|entry| json!({"path": entry.path, "additions": entry.additions, "deletions": entry.deletions}))
            .collect::<Vec<_>>(),
        "scopeCandidates": scope.scope_candidates,
        "isWideScope": scope.is_wide,
        "untrackedFiles": untracked_files,
        "excludedFiles": excluded_files,
    });
    env.state.overview = Some(GitOverviewSnapshot {
        files,
        stat,
        numstat,
        scope_candidates: scope.scope_candidates,
        is_wide_scope: scope.is_wide,
        untracked_files,
        excluded_files,
    });
    Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
}

fn git_file_diff(env: &mut ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let files = str_list(args, "files")?;
    if files.is_empty() {
        return Err("files must contain at least one path".to_owned());
    }
    let staged = opt_bool(args, "staged", true);
    let mut sections: Vec<(i32, String, String)> = Vec::new();
    for file in &files {
        let diff = file_diff(env, file, staged)?;
        sections.push((file_priority(file), file.clone(), truncate_lines(&diff)));
    }
    sections.sort_by_key(|section| std::cmp::Reverse(section.0));
    let rendered: Vec<String> = sections
        .iter()
        .map(|(_, file, content)| format!("=== {file} ===\n{content}"))
        .collect();
    Ok(assemble_sections(&rendered))
}

fn git_hunk(env: &mut ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let file = args
        .get("file")
        .and_then(Json::as_str)
        .ok_or("file is required")?
        .to_owned();
    let selected: Option<Vec<u64>> = match args.get("hunks").and_then(Json::as_array) {
        Some(list) => Some(
            list.iter()
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|number| *number >= 1)
                        .ok_or("hunks must be positive integers")
                })
                .collect::<Result<Vec<u64>, _>>()?,
        ),
        None => None,
    };
    let staged = opt_bool(args, "staged", true);
    let diff = file_diff(env, &file, staged)?;
    let Some(parsed) = parse_file_diffs(&diff).into_iter().next() else {
        return Ok("(no matching hunks)".to_owned());
    };
    if parsed.is_binary {
        return Ok("Binary file diff; no hunks available.".to_owned());
    }
    let hunks: Vec<String> = parse_file_hunks(&parsed)
        .into_iter()
        .filter(|hunk| {
            selected
                .as_ref()
                .is_none_or(|wanted| wanted.contains(&((hunk.index + 1) as u64)))
        })
        .map(|hunk| hunk.content)
        .collect();
    if hunks.is_empty() {
        return Ok("(no matching hunks)".to_owned());
    }
    Ok(hunks.join("\n\n"))
}

/// One parsed `git log` subject.
struct Subject {
    subject: String,
    commit_type: Option<String>,
    scope: Option<String>,
    summary: String,
}

/// Parse `type(scope): summary`; anything else keeps the trimmed subject as the summary.
fn parse_subject(subject: &str) -> Subject {
    let trimmed = subject.trim();
    if let Some((head, rest)) = trimmed.split_once(':')
        && rest.starts_with(char::is_whitespace)
    {
        let head = head.trim();
        let (ty, scope) = match head.find('(') {
            Some(open) if head.ends_with(')') => {
                (&head[..open], Some(&head[open + 1..head.len() - 1]))
            }
            _ => (head, None),
        };
        if !ty.is_empty() && ty.chars().all(|c| c.is_ascii_alphabetic()) {
            return Subject {
                subject: trimmed.to_owned(),
                commit_type: Some(ty.to_owned()),
                scope: scope
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                summary: rest.trim().to_owned(),
            };
        }
    }
    Subject {
        subject: trimmed.to_owned(),
        commit_type: None,
        scope: None,
        summary: trimmed.to_owned(),
    }
}

fn percent(part: usize, total: usize) -> u64 {
    if total == 0 {
        return 0;
    }
    (part as f64 * 100.0 / total as f64).round() as u64
}

fn top_counts(counts: BTreeMap<String, u64>) -> Vec<(String, u64)> {
    let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(TOP_LIST);
    ranked
}

fn recent_commits(root: &Path, args: &Json) -> Result<String, String> {
    let count = args
        .get("count")
        .and_then(Json::as_u64)
        .unwrap_or(RECENT_DEFAULT);
    if !(RECENT_MIN..=RECENT_MAX).contains(&count) {
        return Err(format!(
            "count must be between {RECENT_MIN} and {RECENT_MAX}"
        ));
    }
    let count_arg = count.to_string();
    let raw = git(root, &["log", "-n", count_arg.as_str(), "--format=%s"])?;
    let subjects: Vec<Subject> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_subject)
        .collect();
    let total = subjects.len();

    let scoped = subjects.iter().filter(|s| s.scope.is_some()).count();
    let mut verbs: BTreeMap<String, u64> = BTreeMap::new();
    let mut scopes: BTreeMap<String, u64> = BTreeMap::new();
    for subject in &subjects {
        if let Some(verb) = subject.summary.split_whitespace().next() {
            *verbs.entry(verb.to_ascii_lowercase()).or_default() += 1;
        }
        if let Some(scope) = &subject.scope {
            *scopes.entry(scope.clone()).or_default() += 1;
        }
    }
    let lengths: Vec<usize> = subjects.iter().map(|s| s.summary.chars().count()).collect();
    let min = lengths.iter().copied().min().unwrap_or(0);
    let max = lengths.iter().copied().max().unwrap_or(0);
    let average = if total == 0 {
        0.0
    } else {
        (lengths.iter().sum::<usize>() as f64 / total as f64 * 10.0).round() / 10.0
    };
    let lowercase = subjects
        .iter()
        .filter(|s| s.summary.chars().next().is_some_and(char::is_lowercase))
        .count();

    let out = json!({
        "commits": subjects
            .iter()
            .map(|s| json!({"subject": s.subject, "type": s.commit_type, "scope": s.scope, "summary": s.summary}))
            .collect::<Vec<_>>(),
        "stats": {
            "scopeUsagePercent": percent(scoped, total),
            "commonVerbs": top_counts(verbs)
                .into_iter()
                .map(|(verb, count)| json!({"verb": verb, "count": count}))
                .collect::<Vec<_>>(),
            "summaryLength": {"min": min, "max": max, "average": average},
            "lowercaseSummaryPercent": percent(lowercase, total),
            "topScopes": top_counts(scopes)
                .into_iter()
                .map(|(scope, count)| json!({"scope": scope, "count": count}))
                .collect::<Vec<_>>(),
        }
    });
    Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
}

/// Largest number of files named in the shared overview of `analyze_files`.
const ANALYZE_OVERVIEW_FILES: usize = 40;
/// Character budget for all prompts of one `analyze_files` call. Files that do not fit are
/// reported as skipped instead of being sent.
const ANALYZE_PROMPT_BUDGET_CHARS: usize = 400_000;

/// The change's file list, shared by every prompt of one call. It names at most
/// `ANALYZE_OVERVIEW_FILES` files, so each prompt carries a bounded overview rather than the
/// whole list.
fn analysis_overview(env: &ToolEnv<'_>, files: &[String]) -> String {
    let mut lines: Vec<String> = files
        .iter()
        .take(ANALYZE_OVERVIEW_FILES)
        .map(|file| {
            format!(
                "- {file} ({} lines): {}",
                lines_changed(env, file),
                file_type(file)
            )
        })
        .collect();
    if files.len() > ANALYZE_OVERVIEW_FILES {
        lines.push(format!(
            "- … and {} more files",
            files.len() - ANALYZE_OVERVIEW_FILES
        ));
    }
    format!("FILES IN THIS CHANGE:\n{}", lines.join("\n"))
}

fn analyze_files(env: &ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let requested = str_list(args, "files")?;
    // A path requested twice is analysed once, at its first position.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let files: Vec<String> = requested
        .into_iter()
        .filter(|file| seen.insert(file.clone()))
        .collect();
    if files.is_empty() {
        return Err("files must contain at least one path".to_owned());
    }
    let goal = trimmed_opt(args.get("goal").and_then(Json::as_str)).unwrap_or_default();
    let overview = analysis_overview(env, &files);
    // Prompts are rendered before any call, against one aggregate budget. Files that do not fit
    // are reported as skipped; the rest run on the bounded pool and keep input order.
    let mut used = 0usize;
    let mut jobs: Vec<(&str, Result<String, String>)> = Vec::with_capacity(files.len());
    for file in &files {
        let prompt = render(
            ANALYZE_TEMPLATE,
            &Value::map([
                ("file", Value::text(file)),
                ("goal", Value::text(&goal)),
                ("related_files", Value::text(&overview)),
            ]),
        )?;
        let size = prompt.chars().count();
        if used + size > ANALYZE_PROMPT_BUDGET_CHARS {
            jobs.push((
                file.as_str(),
                Err(format!(
                    "Analysis skipped for {file}: the prompt budget for this call is used up"
                )),
            ));
            continue;
        }
        used += size;
        jobs.push((file.as_str(), Ok(prompt)));
    }
    let backend = env.backend;
    let config = env.config;
    let entries = map_bounded(&jobs, env.cancel, |(file, job)| match job {
        Ok(prompt) => analyze_guarded(backend, config, file, prompt),
        Err(skipped) => Some(skipped.clone()),
    });
    // Items the pool never admitted (cancellation) keep their slot as `None`.
    let results: Vec<String> = jobs
        .iter()
        .zip(entries)
        .filter_map(|(job, entry)| match entry {
            Some(text) => text,
            None => Some(format!("Analysis cancelled for {}", job.0)),
        })
        .collect();
    if results.is_empty() {
        return Ok("(no output)".to_owned());
    }
    Ok(results.join("\n\n"))
}

/// One guarded model call for `analyze_files`. A panic inside the call becomes a failure note for
/// that file instead of aborting the whole analysis.
fn analyze_guarded(
    backend: &dyn CommitBackend,
    config: &Config,
    file: &str,
    prompt: &str,
) -> Option<String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        analyze_one(backend, config, file, prompt)
    }))
    .unwrap_or_else(|_| {
        Some(format!(
            "Analysis failed for {file}: analysis thread panicked"
        ))
    })
}

/// One model call for `analyze_files`. `None` when the model returns no text.
fn analyze_one(
    backend: &dyn CommitBackend,
    config: &Config,
    file: &str,
    prompt: &str,
) -> Option<String> {
    let chat = Chat {
        messages: vec![json!({"role": "user", "content": prompt})],
        tools: Vec::new(),
        temperature: ANALYZE_TEMPERATURE,
        max_tokens: ANALYZE_MAX_TOKENS,
        seed: None,
    };
    match backend.complete(config, &chat) {
        Ok(completion) => {
            let text = completion.reply.text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        Err(error) => Some(format!("Analysis failed for {file}: {error}")),
    }
}

/// Parse `details` into validated details. Unknown categories are reported as errors.
fn parse_details(args: &Json) -> (Vec<ConventionalDetail>, Vec<String>) {
    let mut errors = Vec::new();
    let mut details = Vec::new();
    let Some(items) = args.get("details").and_then(Json::as_array) else {
        return (details, errors);
    };
    for item in items {
        let text = item
            .get("text")
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if text.is_empty() {
            continue;
        }
        let changelog_category = match item
            .get("changelog_category")
            .and_then(Json::as_str)
            .map(str::trim)
        {
            Some(raw) if !raw.is_empty() => match parse_category(raw) {
                Ok(category) => Some(category),
                Err(error) => {
                    errors.push(error);
                    None
                }
            },
            _ => None,
        };
        let user_visible = item.get("user_visible").and_then(Json::as_bool);
        details.push(ConventionalDetail {
            text,
            changelog_category,
            user_visible,
        });
    }
    (details, errors)
}

fn propose_commit(env: &mut ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let raw_type = args
        .get("type")
        .and_then(Json::as_str)
        .ok_or("type is required")?;
    let mut errors = Vec::new();
    let commit_type = match canonical_commit_type(raw_type) {
        Some(known) => known,
        None => {
            errors.push(format!("Invalid commit type: {raw_type}"));
            coerce_commit_type(raw_type)
        }
    };
    let scope = trimmed_opt(args.get("scope").and_then(Json::as_str));
    let raw_summary = args
        .get("summary")
        .and_then(Json::as_str)
        .ok_or("summary is required")?;
    let summary = normalize_summary(raw_summary, &commit_type, scope.as_deref());
    let (details, detail_errors) = parse_details(args);
    let issue_refs = string_list_opt(args, "issue_refs");
    let (details, cap_warnings) = cap_details(&details);

    let summary_checked = validate_summary_rules(&summary);
    errors.extend(detail_errors);
    errors.extend(summary_checked.errors);
    errors.extend(validate_analysis(scope.as_deref(), &details));
    let consistency = validate_type_consistency(
        &commit_type,
        &env.staged,
        env.state.diff_text.as_deref().unwrap_or(""),
        &summary,
        &details,
    );
    errors.extend(consistency.errors);

    let mut warnings = cap_warnings;
    warnings.extend(summary_checked.warnings);
    warnings.extend(consistency.warnings);

    let valid = errors.is_empty();
    if valid {
        env.state.proposal = Some(CommitProposal {
            analysis: ConventionalAnalysis {
                commit_type: commit_type.clone(),
                scope: scope.clone(),
                details: details.clone(),
                issue_refs: issue_refs.clone(),
            },
            summary: summary.clone(),
            warnings: warnings.clone(),
        });
    }

    let mut out = json!({
        "valid": valid,
        "errors": errors,
        "warnings": warnings,
        "constraints": {"maxSummaryChars": SUMMARY_MAX_CHARS, "maxDetailItems": MAX_DETAIL_ITEMS},
    });
    if valid {
        out["proposal"] = json!({
            "type": commit_type,
            "scope": scope,
            "summary": summary,
            "details": details.iter().map(|detail| detail.text.clone()).collect::<Vec<String>>(),
            "issueRefs": issue_refs,
        });
    }
    Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
}

/// Shape-only parse of one change; semantic checks (staging, hunks, lines) happen in [`split_commit`].
fn parse_change(value: &Json) -> Result<FileChange, String> {
    let path = value
        .get("path")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or("each change needs a path")?
        .to_owned();
    match value.get("kind").and_then(Json::as_str).unwrap_or("all") {
        "all" => Ok(FileChange::All { path }),
        "indices" => {
            let indices = value
                .get("indices")
                .and_then(Json::as_array)
                .ok_or_else(|| format!("indices required for {path}"))?
                .iter()
                .map(|item| {
                    item.as_u64()
                        .ok_or_else(|| format!("hunk indices must be integers for {path}"))
                })
                .collect::<Result<Vec<u64>, String>>()?;
            Ok(FileChange::Indices { path, indices })
        }
        "lines" => {
            let start = value
                .get("start")
                .and_then(Json::as_u64)
                .ok_or_else(|| format!("start required for {path}"))?;
            let end = value
                .get("end")
                .and_then(Json::as_u64)
                .ok_or_else(|| format!("end required for {path}"))?;
            Ok(FileChange::Lines { path, start, end })
        }
        other => Err(format!("unknown change kind for {path}: {other}")),
    }
}

/// Line count of the staged version of `path`, `None` when it cannot be read.
fn staged_line_count(env: &ToolEnv<'_>, path: &str) -> Option<u64> {
    let spec = format!(":{path}");
    git_index(env, &["show", spec.as_str()])
        .ok()
        .map(|content| content.lines().count() as u64)
}

fn parse_dependencies(
    raw: &Json,
    own: usize,
    total: usize,
    prefix: &str,
    errors: &mut Vec<String>,
) -> Vec<usize> {
    let Some(list) = raw.get("dependencies").and_then(Json::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for value in list {
        let Some(dependency) = value.as_u64().map(|number| number as usize) else {
            errors.push(format!("{prefix}dependency index must be an integer"));
            continue;
        };
        if dependency == own {
            errors.push(format!("{prefix}cannot depend on itself"));
        } else if dependency >= total {
            errors.push(format!(
                "{prefix}dependency index out of range ({dependency})"
            ));
        } else {
            out.push(dependency);
        }
    }
    out
}

/// A file may appear in several commits only when every occurrence selects disjoint hunks or disjoint lines.
fn can_share_file(occurrences: &[(usize, FileChange)]) -> bool {
    if occurrences
        .iter()
        .all(|(_, change)| matches!(change, FileChange::Indices { .. }))
    {
        let mut seen = BTreeSet::new();
        return occurrences.iter().all(|(_, change)| match change {
            FileChange::Indices { indices, .. } => indices.iter().all(|index| seen.insert(*index)),
            _ => false,
        });
    }
    if occurrences
        .iter()
        .all(|(_, change)| matches!(change, FileChange::Lines { .. }))
    {
        let mut ranges: Vec<(u64, u64)> = occurrences
            .iter()
            .filter_map(|(_, change)| match change {
                FileChange::Lines { start, end, .. } => Some((*start, *end)),
                _ => None,
            })
            .collect();
        ranges.sort_unstable();
        return ranges.windows(2).all(|pair| pair[1].0 > pair[0].1);
    }
    false
}

fn split_commit(env: &mut ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let commits = args
        .get("commits")
        .and_then(Json::as_array)
        .ok_or("commits must be an array")?;
    let total = commits.len();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut groups: Vec<SplitCommitGroup> = Vec::new();
    let diff_text = env.state.diff_text.as_deref().unwrap_or("");

    for (index, raw) in commits.iter().enumerate() {
        let prefix = format!("Commit {}: ", index + 1);
        let raw_type = raw.get("type").and_then(Json::as_str).unwrap_or("");
        let commit_type = match canonical_commit_type(raw_type) {
            Some(known) => known,
            None => {
                errors.push(format!("{prefix}Invalid commit type: {raw_type}"));
                coerce_commit_type(raw_type)
            }
        };
        let scope = trimmed_opt(raw.get("scope").and_then(Json::as_str));
        let raw_summary = raw.get("summary").and_then(Json::as_str).unwrap_or("");
        let summary = normalize_summary(raw_summary, &commit_type, scope.as_deref());

        let summary_checked = validate_summary_rules(&summary);
        errors.extend(
            summary_checked
                .errors
                .iter()
                .map(|error| format!("{prefix}{error}")),
        );
        warnings.extend(
            summary_checked
                .warnings
                .iter()
                .map(|warning| format!("{prefix}{warning}")),
        );

        let (details, detail_errors) = parse_details(raw);
        errors.extend(detail_errors.iter().map(|error| format!("{prefix}{error}")));
        let (details, cap_warnings) = cap_details(&details);
        warnings.extend(
            cap_warnings
                .iter()
                .map(|warning| format!("{prefix}{warning}")),
        );
        errors.extend(
            validate_analysis(scope.as_deref(), &details)
                .iter()
                .map(|error| format!("{prefix}{error}")),
        );

        let mut changes: Vec<FileChange> = Vec::new();
        match raw.get("changes").and_then(Json::as_array) {
            Some(values) if !values.is_empty() => {
                for value in values {
                    match parse_change(value) {
                        Ok(change) => changes.push(change),
                        Err(error) => errors.push(format!("{prefix}{error}")),
                    }
                }
            }
            _ => errors.push(format!("{prefix}no files specified")),
        }
        for change in &changes {
            let path = change.path();
            if !env.staged.iter().any(|staged| staged == path)
                && !env.changelog_targets.iter().any(|t| t == path)
            {
                errors.push(format!("File not staged: {path}"));
            }
            match change {
                FileChange::All { .. } => {}
                FileChange::Indices { path, indices } => {
                    let count = hunk_count(diff_text, path) as u64;
                    if indices.is_empty()
                        || indices.iter().any(|index| *index == 0 || *index > count)
                    {
                        errors.push(format!("{prefix}invalid hunk indices for {path}"));
                    } else if whole_file_only(diff_text, path)
                        && indices.iter().copied().collect::<BTreeSet<u64>>().len() as u64 != count
                    {
                        errors.push(format!(
                            "{prefix}{path} is added or deleted and must be committed whole"
                        ));
                    }
                }
                FileChange::Lines { path, start, end } => {
                    let valid = *start >= 1
                        && start <= end
                        && staged_line_count(env, path).is_some_and(|lines| *end <= lines);
                    if !valid {
                        errors.push(format!("{prefix}invalid line range for {path}"));
                    }
                }
            }
        }

        let files: Vec<String> = changes
            .iter()
            .map(|change| change.path().to_owned())
            .collect();
        let consistency =
            validate_type_consistency(&commit_type, &files, diff_text, &summary, &details);
        errors.extend(
            consistency
                .errors
                .iter()
                .map(|error| format!("{prefix}{error}")),
        );
        warnings.extend(
            consistency
                .warnings
                .iter()
                .map(|warning| format!("{prefix}{warning}")),
        );

        let dependencies = parse_dependencies(raw, index, total, &prefix, &mut errors);
        groups.push(SplitCommitGroup {
            changes,
            commit_type,
            scope,
            summary,
            details,
            issue_refs: string_list_opt(raw, "issue_refs"),
            rationale: trimmed_opt(raw.get("rationale").and_then(Json::as_str)),
            dependencies,
        });
    }

    let mut occurrences: BTreeMap<String, Vec<(usize, FileChange)>> = BTreeMap::new();
    for (index, group) in groups.iter().enumerate() {
        for change in &group.changes {
            occurrences
                .entry(change.path().to_owned())
                .or_default()
                .push((index, change.clone()));
        }
    }
    for (path, occurrence) in &occurrences {
        let commits_seen: BTreeSet<usize> = occurrence.iter().map(|(index, _)| *index).collect();
        let repeated = commits_seen.iter().find(|commit| {
            occurrence
                .iter()
                .filter(|(index, _)| index == *commit)
                .count()
                > 1
        });
        if let Some(commit) = repeated {
            errors.push(format!(
                "File listed multiple times in commit {}: {path}",
                groups[*commit].summary
            ));
        } else if occurrence.len() > 1 {
            if !can_share_file(occurrence) {
                errors.push(format!("File appears in multiple commits: {path}"));
            } else {
                let total_hunks = hunk_count(diff_text, path) as u64;
                let covered: BTreeSet<u64> = occurrence
                    .iter()
                    .flat_map(|(_, change)| match change {
                        FileChange::Indices { indices, .. } => indices.clone(),
                        _ => Vec::new(),
                    })
                    .collect();
                if (1..=total_hunks).any(|hunk| !covered.contains(&hunk)) {
                    warnings.push(format!(
                        "Some hunks of {path} are in no commit and stay unstaged."
                    ));
                }
            }
        }
    }
    for file in &env.staged {
        if !is_lock_file(file) && !occurrences.contains_key(file) {
            errors.push(format!("Staged file missing from split plan: {file}"));
        }
    }
    if let Err(error) = compute_dependency_order(&groups) {
        errors.push(error);
    }

    let valid = errors.is_empty();
    if valid {
        env.state.split_proposal = Some(SplitCommitPlan {
            commits: groups.clone(),
            warnings: warnings.clone(),
        });
    }
    let out = json!({
        "valid": valid,
        "errors": errors,
        "warnings": warnings,
        "commits": groups.len(),
        "constraints": {"maxSummaryChars": SUMMARY_MAX_CHARS, "maxDetailItems": MAX_DETAIL_ITEMS},
    });
    Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
}

/// Parse a `{category: [items]}` map. Unknown categories and non-string items are reported in `errors`.
fn category_map(
    value: Option<&Json>,
    path: &str,
    deletion: bool,
    errors: &mut Vec<String>,
) -> Vec<(String, Vec<String>)> {
    let Some(map) = value.and_then(Json::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (category, items) in map {
        if parse_category(category).is_err() {
            errors.push(if deletion {
                format!("Unknown deletion category for {path}: {category}")
            } else {
                format!("Unknown changelog category for {path}: {category}")
            });
            continue;
        }
        let strings: Option<Vec<String>> = items.as_array().and_then(|list| {
            list.iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect()
        });
        let Some(strings) = strings else {
            errors.push(if deletion {
                format!("Invalid deletion entries for {path}: {category}")
            } else {
                format!("Invalid changelog entries for {path}: {category}")
            });
            continue;
        };
        let cleaned = if deletion {
            strings
                .iter()
                .map(|item| item.trim().to_owned())
                .filter(|item| !item.is_empty())
                .collect()
        } else {
            normalize_items(&strings)
        };
        if cleaned.is_empty() {
            if !strings.is_empty() {
                errors.push(if deletion {
                    format!("Invalid deletion entries for {path}: {category}")
                } else {
                    format!("Invalid changelog entries for {path}: {category}")
                });
            }
            continue;
        }
        out.push((category.clone(), cleaned));
    }
    out
}

fn propose_changelog(env: &mut ToolEnv<'_>, args: &Json) -> Result<String, String> {
    let targets = args
        .get("entries")
        .and_then(Json::as_array)
        .ok_or("entries must be an array")?;
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut proposals: Vec<ChangelogTargetProposal> = Vec::new();

    for target in targets {
        let Some(path) = target
            .get("path")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            errors.push("Changelog entry is missing path".to_owned());
            continue;
        };
        if !env
            .changelog_targets
            .iter()
            .any(|expected| expected == path)
        {
            errors.push(format!("Changelog not expected: {path}"));
            continue;
        }
        if proposals.iter().any(|proposal| proposal.path == path) {
            errors.push(format!("Duplicate changelog entry for {path}"));
            continue;
        }
        let entries = category_map(target.get("entries"), path, false, &mut errors);
        let deletions = category_map(target.get("deletions"), path, true, &mut errors);
        if entries.is_empty() {
            warnings.push(format!("No changelog entries provided for {path}."));
        }
        proposals.push(ChangelogTargetProposal {
            path: path.to_owned(),
            entries,
            deletions,
        });
    }
    for expected in &env.changelog_targets {
        if !proposals.iter().any(|proposal| &proposal.path == expected) {
            errors.push(format!("Missing changelog entries for {expected}"));
        }
    }

    let valid = errors.is_empty();
    let mut text = if valid {
        "Changelog entries accepted.".to_owned()
    } else {
        "Changelog validation failed.".to_owned()
    };
    if !errors.is_empty() {
        text.push_str("\n\nErrors:\n- ");
        text.push_str(&errors.join("\n- "));
    }
    if !warnings.is_empty() {
        text.push_str("\n\nWarnings:\n- ");
        text.push_str(&warnings.join("\n- "));
    }
    if valid {
        env.state.changelog_proposal = Some(ChangelogProposal { entries: proposals });
    }
    Ok(text)
}

#[cfg(test)]
mod tests;
