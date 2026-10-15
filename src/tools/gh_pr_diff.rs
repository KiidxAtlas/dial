//! `pr_diff` op of the `github` tool and the `pr://` diff URL grammar (fetch and
//! parse, and the `pr://<n>/diff[/…]` family).
//!
//! Views: `Summary` (file listing with `+adds -dels`), `File(i)` (1-indexed section, verbatim),
//! `All` (full unified diff, verbatim). The unified text comes from `gh pr diff`; when GitHub
//! rejects the aggregate diff as too large, it is reassembled from the paginated per-file API.

use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::gh_common::{GH_TIMEOUT, clip, gh, gh_json, resolve_repo, strings, valid_repo};
use super::{ToolOutput, Workspace, opt_bool, opt_str, opt_u64};

/// Files per page of the per-file endpoint.
const FILES_PAGE_SIZE: u64 = 100;
/// Largest PR the per-file endpoint reassembles.
const FILES_MAX: u64 = 3000;
/// Prefix of every per-file section in a unified diff.
const SECTION_PREFIX: &str = "diff --git ";

/// Which part of a pull request diff is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrDiffView {
    /// File listing with per-file counts (`pr://<N>/diff`).
    Summary,
    /// One file's section, 1-indexed (`pr://<N>/diff/<i>`).
    File(usize),
    /// The full unified diff (`pr://<N>/diff/all`).
    All,
}

/// A parsed `pr://` target. `repo` is `owner/repo` or `host/owner/repo`; `None` means the current
/// checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrTarget {
    pub repo: Option<String>,
    pub number: u64,
    pub view: PrDiffView,
}

/// Parse a `pr://` diff target. Pure: no `gh` calls.
///
/// Accepted forms (each repo-qualified form may carry a `<host>/` prefix, dotted or single-label):
/// `pr://<N>`, `pr://<N>/diff`, `pr://<N>/diff/all`, `pr://<N>/diff/<i>`, and the same four with
/// `<owner>/<repo>/` before the number. Returns `None` for anything else.
pub fn parse_pr_url(target: &str) -> Option<PrTarget> {
    let rest = target.strip_prefix("pr://")?;
    let segs: Vec<&str> = rest.split('/').collect();
    if segs
        .iter()
        .any(|seg| seg.is_empty() || *seg == "." || *seg == "..")
    {
        return None;
    }
    if is_digits(segs[0]) {
        return Some(PrTarget {
            repo: None,
            number: parse_number(segs[0])?,
            view: parse_view(&segs[1..])?,
        });
    }
    // A dotted first segment is always a host (GitHub owners cannot contain dots). A single-label
    // host is recognized only when the number sits in the fourth position.
    let host =
        (segs[0].contains('.') || segs.get(3).is_some_and(|seg| is_digits(seg))).then_some(segs[0]);
    let base = usize::from(host.is_some());
    let owner = *segs.get(base)?;
    let name = *segs.get(base + 1)?;
    let number = parse_number(*segs.get(base + 2)?)?;
    let slug = format!("{owner}/{name}");
    valid_repo(&slug).ok()?;
    let repo = match host {
        Some(host) => {
            if !valid_host(host) {
                return None;
            }
            format!("{host}/{slug}")
        }
        None => slug,
    };
    Some(PrTarget {
        repo: Some(repo),
        number,
        view: parse_view(&segs[base + 3..])?,
    })
}

/// The diff sub-path after the number: nothing or `diff` (summary), `diff/all`, `diff/<i>`.
fn parse_view(tail: &[&str]) -> Option<PrDiffView> {
    match tail {
        [] | ["diff"] => Some(PrDiffView::Summary),
        ["diff", "all"] => Some(PrDiffView::All),
        ["diff", index] => Some(PrDiffView::File(
            usize::try_from(parse_number(index)?).ok()?,
        )),
        _ => None,
    }
}

fn is_digits(seg: &str) -> bool {
    !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit())
}

/// Parses a positive decimal integer for PR numbers and file indices: digits only, greater than zero.
fn parse_number(seg: &str) -> Option<u64> {
    if !is_digits(seg) {
        return None;
    }
    seg.parse::<u64>().ok().filter(|number| *number > 0)
}

fn valid_host(host: &str) -> bool {
    !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The `pr_diff` op. Errors from this module are returned as `Err` and become error results.
pub(super) fn run(
    op: &str,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    match op {
        "pr_diff" => pr_diff(ws, args, cancel),
        other => Err(format!("gh_pr_diff does not handle op {other:?}")),
    }
}

/// `pr` argument: a PR number, or a `pr://` URL whose repo and view it may carry.
struct PrReference {
    repo: Option<String>,
    number: u64,
    view: Option<PrDiffView>,
}

fn pr_reference(args: &Value) -> Result<PrReference, String> {
    let plain = |number| PrReference {
        repo: None,
        number,
        view: None,
    };
    match args.get("pr") {
        None | Some(Value::Null) => {
            Err("github: `pr` is required for pr_diff (PR number or pr:// URL)".to_owned())
        }
        Some(Value::Number(number)) => match number.as_u64().filter(|n| *n > 0) {
            Some(number) => Ok(plain(number)),
            None => Err(format!(
                "github: `pr` must be a positive PR number, got {number}"
            )),
        },
        Some(Value::String(text)) => {
            let text = text.trim();
            if let Some(number) = parse_number(text) {
                return Ok(plain(number));
            }
            let target = parse_pr_url(text).ok_or_else(|| {
                format!("github: `pr` must be a PR number or a pr:// URL (pr://<N>, pr://<owner>/<repo>/<N>, optionally /diff[/all|/<i>]), got `{text}`")
            })?;
            Ok(PrReference {
                repo: target.repo,
                number: target.number,
                view: Some(target.view),
            })
        }
        Some(other) => Err(format!(
            "github: `pr` must be a number or string, got {other}"
        )),
    }
}

fn text_arg(args: &Value, key: &str) -> Result<Option<String>, String> {
    Ok(opt_str(args, key)?
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned))
}

fn pr_diff(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    let pr = pr_reference(args)?;

    let all = opt_bool(args, "all")?;
    let file = opt_u64(args, "file")?;
    let arg_view = match (all, file) {
        (true, Some(_)) => {
            return Err("github: pass `all` or `file` to pr_diff, not both".to_owned());
        }
        (true, None) => Some(PrDiffView::All),
        (false, Some(index)) => Some(PrDiffView::File(
            usize::try_from(index).unwrap_or(usize::MAX),
        )),
        (false, None) => None,
    };
    let view = match (arg_view, pr.view) {
        (Some(_), Some(url)) if url != PrDiffView::Summary => {
            return Err("github: the pr:// URL already selects a diff view; drop `file`/`all` or pass the bare PR number".to_owned());
        }
        (Some(arg), _) => arg,
        (None, Some(url)) => url,
        (None, None) => PrDiffView::Summary,
    };

    let repo = match (text_arg(args, "repo")?, pr.repo) {
        (Some(arg), Some(url)) if !arg.eq_ignore_ascii_case(&url) => {
            return Err(format!(
                "github: `repo` {arg} conflicts with the pr:// URL repo {url}"
            ));
        }
        (Some(arg), _) => Some(arg),
        (None, url) => url,
    };
    let resolved = resolve_repo(ws, repo.as_deref(), cancel)?;
    let repo = match resolved.host {
        Some(host) => format!("{host}/{}", resolved.slug),
        None => resolved.slug,
    };

    let text = fetch_unified(ws, &repo, pr.number, cancel)?;
    let rendered = match view {
        PrDiffView::All => text,
        PrDiffView::Summary => render_summary(&repo, pr.number, &parse_unified(&text)),
        PrDiffView::File(index) => {
            let files = parse_unified(&text);
            if index == 0 || index > files.len() {
                return Err(format!(
                    "pr://{repo}/{number}/diff/{index} is out of range; PR has {count} file{plural}. Use pr://{repo}/{number}/diff to list available indices.",
                    number = pr.number,
                    count = files.len(),
                    plural = plural(files.len()),
                ));
            }
            let file = &files[index - 1];
            text[file.start..file.end].to_owned()
        }
    };
    Ok(ToolOutput::ok(clip(ws, rendered)))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// `gh pr diff` text verbatim; falls back to the per-file API when GitHub rejects the aggregate
/// diff for size.
fn fetch_unified(
    ws: &Workspace,
    repo: &str,
    number: u64,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut args = strings(&["pr", "diff"]);
    args.push(number.to_string());
    args.extend(strings(&["--color", "never", "--repo"]));
    args.push(repo.to_owned());
    match gh(ws, &args, GH_TIMEOUT, cancel) {
        Ok(text) => Ok(text),
        Err(err) if is_too_large_error(&err) => fetch_via_files_api(ws, repo, number, cancel),
        Err(err) => Err(err),
    }
}

/// Whether the error means the aggregate diff is too large: HTTP 406, the "exceeded the maximum number of lines" message, or
/// the `too_large` code.
fn is_too_large_error(message: &str) -> bool {
    contains_word(message, "HTTP 406")
        || message
            .to_ascii_lowercase()
            .contains("exceeded the maximum number of lines")
        || contains_word(message, "too_large")
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(index, _)| {
        let before = haystack[..index].chars().next_back();
        let after = haystack[index + needle.len()..].chars().next();
        !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
    })
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Rebuilds a unified diff from `GET /repos/{slug}/pulls/{n}/files`.
fn fetch_via_files_api(
    ws: &Workspace,
    repo: &str,
    number: u64,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let (host, slug) = split_host(repo);
    let host_args: Vec<String> = host
        .map(|host| vec!["--hostname".to_owned(), host.to_owned()])
        .unwrap_or_default();

    let pull = gh_json(
        ws,
        &api_args(
            &host_args,
            &format!("/repos/{slug}/pulls/{number}"),
            Vec::new(),
        ),
        GH_TIMEOUT,
        cancel,
    )?;
    let changed = pull["changed_files"].as_u64().unwrap_or(0);
    if changed > FILES_MAX {
        return Err(format!(
            "Pull request changes {changed} files, exceeding GitHub's {FILES_MAX}-file limit for the per-file diff API."
        ));
    }

    let mut sections = Vec::new();
    let mut page = 1_u64;
    loop {
        let extra = vec![
            "-F".to_owned(),
            format!("per_page={FILES_PAGE_SIZE}"),
            "-F".to_owned(),
            format!("page={page}"),
        ];
        let response = gh_json(
            ws,
            &api_args(
                &host_args,
                &format!("/repos/{slug}/pulls/{number}/files"),
                extra,
            ),
            GH_TIMEOUT,
            cancel,
        )?;
        let files = response.as_array().ok_or_else(|| {
            "GitHub CLI returned an unexpected pull request files response.".to_owned()
        })?;
        sections.extend(files.iter().filter_map(synthetic_section));
        if (files.len() as u64) < FILES_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    // The trailing newline mirrors `gh pr diff`, so section boundaries match.
    Ok(if sections.is_empty() {
        String::new()
    } else {
        format!("{}\n", sections.join("\n"))
    })
}

fn api_args(host_args: &[String], path: &str, extra: Vec<String>) -> Vec<String> {
    let mut args = strings(&["api"]);
    args.extend(host_args.iter().cloned());
    args.extend(strings(&["--method", "GET"]));
    args.push(path.to_owned());
    args.extend(extra);
    args
}

/// Split `[HOST/]OWNER/REPO` into the optional host and the `OWNER/REPO` slug.
fn split_host(repo: &str) -> (Option<&str>, &str) {
    if repo.matches('/').count() == 2 {
        if let Some((host, slug)) = repo.split_once('/') {
            return (Some(host), slug);
        }
    }
    (None, repo)
}

/// One `diff --git` section from a files-API entry. Entries with no
/// patch (binary or too large) keep an explicit marker line instead of being dropped.
fn synthetic_section(file: &Value) -> Option<String> {
    let new_path = non_empty(file["filename"].as_str())?;
    let status = file["status"].as_str().unwrap_or("modified");
    let previous = non_empty(file["previous_filename"].as_str());
    let old_path = previous.unwrap_or(new_path);
    let old_diff = synthetic_path("a/", old_path);
    let new_diff = synthetic_path("b/", new_path);

    let mut lines = vec![format!("diff --git {old_diff} {new_diff}")];
    if status == "added" {
        lines.push("new file mode 100644".to_owned());
    } else if status == "removed" {
        lines.push("deleted file mode 100644".to_owned());
    } else if status == "renamed" || previous.is_some() {
        lines.push(format!("rename from {}", synthetic_path("", old_path)));
        lines.push(format!("rename to {}", synthetic_path("", new_path)));
    }
    match non_empty(file["patch"].as_str()) {
        Some(patch) => {
            lines.push(if status == "added" {
                "--- /dev/null".to_owned()
            } else {
                format!("--- {old_diff}")
            });
            lines.push(if status == "removed" {
                "+++ /dev/null".to_owned()
            } else {
                format!("+++ {new_diff}")
            });
            lines.push(patch.to_owned());
        }
        None => lines.push(format!(
            "* patch unavailable (binary or too large); additions {}, deletions {}",
            file["additions"].as_u64().unwrap_or(0),
            file["deletions"].as_u64().unwrap_or(0),
        )),
    }
    Some(lines.join("\n"))
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.is_empty())
}

/// Quotes and escapes a path that contains control characters,
/// whitespace, quotes or backslashes, as git would.
fn synthetic_path(prefix: &str, path: &str) -> String {
    let prefixed = format!("{prefix}{path}");
    let needs_quotes = prefixed
        .chars()
        .any(|c| (c as u32) < 0x20 || c.is_whitespace() || c == '"' || c == '\\');
    if !needs_quotes {
        return prefixed;
    }
    let mut escaped = String::new();
    for c in prefixed.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\{:03o}", c as u32)),
            c => escaped.push(c),
        }
    }
    format!("\"{escaped}\"")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeType {
    Modified,
    Added,
    Deleted,
    Renamed,
    Binary,
}

impl ChangeType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Binary => "binary",
        }
    }
}

/// One file of a unified diff: counts plus the byte range of its section in the text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrDiffFile {
    /// Display path; the post-image path unless the file was deleted.
    path: String,
    additions: usize,
    deletions: usize,
    change: ChangeType,
    /// Pre-image path, set only when it differs from `path`.
    old_path: Option<String>,
    /// Byte offset of the section's `diff --git` line.
    start: usize,
    /// Byte offset of the next section, or the end of the text.
    end: usize,
}

/// Splits on `diff --git ` section starts and parses each section.
fn parse_unified(text: &str) -> Vec<PrDiffFile> {
    let mut starts: Vec<usize> = Vec::new();
    if text.starts_with(SECTION_PREFIX) {
        starts.push(0);
    }
    starts.extend(
        text.match_indices("\ndiff --git ")
            .map(|(index, _)| index + 1),
    );
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(text.len());
            parse_section(&text[start..end], start, end)
        })
        .collect()
}

/// Parses one diff section.
fn parse_section(section: &str, start: usize, end: usize) -> PrDiffFile {
    let mut lines = section.split('\n');
    let headers = parse_header_paths(lines.next().unwrap_or(""));
    let mut old_path = headers.old;
    let mut new_path = headers.new;
    let mut change = ChangeType::Modified;
    let mut binary = false;
    let mut additions = 0;
    let mut deletions = 0;
    let mut in_hunk = false;
    let mut unavailable: Option<(usize, usize)> = None;

    for line in lines {
        if line.starts_with("new file mode") {
            change = ChangeType::Added;
        } else if line.starts_with("deleted file mode") {
            change = ChangeType::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            change = ChangeType::Renamed;
            old_path = Some(unquote_path(path));
        } else if let Some(path) = line.strip_prefix("rename to ") {
            new_path = Some(unquote_path(path));
        } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
            binary = true;
        } else if let Some(counts) = unavailable_counts(line) {
            unavailable = Some(counts);
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if !in_hunk && is_file_header_line(line) {
            continue;
        } else if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }

    if let Some((added, removed)) = unavailable {
        additions = added;
        deletions = removed;
    }

    if binary {
        if change == ChangeType::Modified {
            change = ChangeType::Binary;
        }
        additions = 0;
        deletions = 0;
    }

    let path = if change == ChangeType::Deleted {
        old_path.clone().or_else(|| new_path.clone())
    } else {
        new_path.or_else(|| old_path.clone())
    }
    .unwrap_or_else(|| "(unknown)".to_owned());
    let old_path = old_path.filter(|old| !old.is_empty() && *old != path);
    PrDiffFile {
        path,
        additions,
        deletions,
        change,
        old_path,
        start,
        end,
    }
}

/// Counts from the marker `synthetic_section` writes for a file whose patch is unavailable.
fn unavailable_counts(line: &str) -> Option<(usize, usize)> {
    let counts = line
        .strip_prefix("* patch unavailable")?
        .split_once("; additions ")?
        .1;
    let (additions, deletions) = counts.split_once(", deletions ")?;
    Some((additions.parse().ok()?, deletions.parse().ok()?))
}

/// Whether a line is a file header line.
fn is_file_header_line(line: &str) -> bool {
    line == "--- /dev/null"
        || line == "+++ /dev/null"
        || line.starts_with("--- a/")
        || line.starts_with("+++ b/")
        || line.starts_with("--- \"a/")
        || line.starts_with("+++ \"b/")
}

struct HeaderPaths {
    old: Option<String>,
    new: Option<String>,
}

/// Decodes a rename path written by `synthetic_path`: C-quoted when it starts with `"`, else raw.
fn unquote_path(text: &str) -> String {
    if text.starts_with('"') {
        let chars: Vec<char> = text.chars().collect();
        if let Some((path, _)) = parse_quoted(&chars, 0) {
            return path;
        }
    }
    text.to_owned()
}

/// Parses `diff --git a/<old> b/<new>`, with git's C-quoting when quoted.
fn parse_header_paths(header: &str) -> HeaderPaths {
    let none = HeaderPaths {
        old: None,
        new: None,
    };
    let Some(trail) = header.strip_prefix(SECTION_PREFIX) else {
        return none;
    };
    if trail.starts_with('"') {
        let chars: Vec<char> = trail.chars().collect();
        let Some((old, next)) = parse_quoted(&chars, 0) else {
            return none;
        };
        let Some((new, _)) = parse_token(&chars, next) else {
            return none;
        };
        return HeaderPaths {
            old: old.strip_prefix("a/").map(str::to_owned),
            new: new.strip_prefix("b/").map(str::to_owned),
        };
    }
    match trail.find(" b/") {
        Some(b_index) if b_index > 0 && trail.starts_with("a/") => HeaderPaths {
            old: Some(trail[2..b_index].to_owned()),
            new: Some(trail[b_index + 3..].to_owned()),
        },
        _ => none,
    }
}

/// Skips spaces, then reads a quoted or space-terminated token.
fn parse_token(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start;
    while chars.get(i) == Some(&' ') {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    if chars[i] == '"' {
        return parse_quoted(chars, i);
    }
    let mut end = i;
    while end < chars.len() && chars[end] != ' ' {
        end += 1;
    }
    Some((chars[i..end].iter().collect(), end))
}

/// Parses a C-quoted string starting at `start`, returning its decoded value
/// and the index after the closing quote. Octal escapes carry UTF-8 bytes, so
/// non-ASCII names are decoded from bytes rather than one character per byte.
fn parse_quoted(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'"') {
        return None;
    }
    let mut value: Vec<u8> = Vec::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '"' => return Some((String::from_utf8_lossy(&value).into_owned(), i + 1)),
            '\\' => {
                let (decoded, next) = parse_escape(chars, i);
                value.extend_from_slice(&decoded);
                i = next;
            }
            ch => {
                let mut buf = [0; 4];
                value.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                i += 1;
            }
        }
    }
    None
}

/// Decodes the escape whose backslash is at `slash`; returns its bytes and the index after it.
/// Octal escapes are raw bytes, which may be a fragment of a UTF-8 sequence.
fn parse_escape(chars: &[char], slash: usize) -> (Vec<u8>, usize) {
    let Some(&next) = chars.get(slash + 1) else {
        return (b"\\".to_vec(), slash + 1);
    };
    if ('0'..='7').contains(&next) {
        let mut end = slash + 1;
        while end < chars.len() && end < slash + 4 && ('0'..='7').contains(&chars[end]) {
            end += 1;
        }
        let digits: String = chars[slash + 1..end].iter().collect();
        let code = u32::from_str_radix(&digits, 8).unwrap_or(0);
        return (vec![code as u8], end);
    }
    let decoded = match next {
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        other => other,
    };
    let mut buf = [0; 4];
    (decoded.encode_utf8(&mut buf).as_bytes().to_vec(), slash + 2)
}

/// Renders the summary listing (the `pr://<n>/diff` view).
fn render_summary(repo: &str, number: u64, files: &[PrDiffFile]) -> String {
    let header = format!(
        "# Pull Request Diff: {repo}#{number} ({count} file{plural})",
        count = files.len(),
        plural = plural(files.len()),
    );
    let body = if files.is_empty() {
        "_No file changes._".to_owned()
    } else {
        files
            .iter()
            .enumerate()
            .map(|(i, file)| format_file_line(i + 1, file, repo, number))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    format!(
        "{header}\n\n{body}\n\n---\nRead all: `pr://{repo}/{number}/diff/all`. Each file is also available as `pr://{repo}/{number}/diff/<i>`."
    )
}

/// Formats one file line.
fn format_file_line(index: usize, file: &PrDiffFile, repo: &str, number: u64) -> String {
    let stats = if file.change == ChangeType::Binary {
        "(binary)".to_owned()
    } else {
        format!("+{} -{}", file.additions, file.deletions)
    };
    let rename = file
        .old_path
        .as_ref()
        .map(|old| format!("  (renamed from {old})"))
        .unwrap_or_default();
    format!(
        "{index}. {path}  {stats}  [{change}]{rename}\n   pr://{repo}/{number}/diff/{index}",
        path = file.path,
        change = file.change.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use serde_json::json;

    use super::super::gh_common::strings;
    use super::super::gh_common::test_support::FakeGh;
    use super::*;

    /// Five files: modified, added, deleted, renamed, binary.
    const CANNED: &str = "diff --git a/src/lib.rs b/src/lib.rs\nindex 111..222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n fn a() {}\n-fn b() {}\n+fn b() -> u8 { 1 }\n+fn c() {}\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..333\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\ndiff --git a/old.txt b/old.txt\ndeleted file mode 100644\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\ndiff --git a/from.txt b/to.txt\nsimilarity index 90%\nrename from from.txt\nrename to to.txt\n--- a/from.txt\n+++ b/to.txt\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\n";

    const SUMMARY: &str = "# Pull Request Diff: acme/widgets#12 (5 files)\n\n1. src/lib.rs  +2 -1  [modified]\n   pr://acme/widgets/12/diff/1\n\n2. new.txt  +2 -0  [added]\n   pr://acme/widgets/12/diff/2\n\n3. old.txt  +0 -1  [deleted]\n   pr://acme/widgets/12/diff/3\n\n4. to.txt  +1 -1  [renamed]  (renamed from from.txt)\n   pr://acme/widgets/12/diff/4\n\n5. img.png  (binary)  [binary]\n   pr://acme/widgets/12/diff/5\n\n---\nRead all: `pr://acme/widgets/12/diff/all`. Each file is also available as `pr://acme/widgets/12/diff/<i>`.";

    const NEW_TXT_SECTION: &str = "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..333\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n";

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn parse_pr_url_accepts_every_form() {
        let target = |repo: Option<&str>, number, view| PrTarget {
            repo: repo.map(str::to_owned),
            number,
            view,
        };
        let cases = [
            ("pr://12", target(None, 12, PrDiffView::Summary)),
            ("pr://12/diff", target(None, 12, PrDiffView::Summary)),
            ("pr://12/diff/all", target(None, 12, PrDiffView::All)),
            ("pr://12/diff/3", target(None, 12, PrDiffView::File(3))),
            (
                "pr://acme/widgets/12",
                target(Some("acme/widgets"), 12, PrDiffView::Summary),
            ),
            (
                "pr://acme/widgets/12/diff",
                target(Some("acme/widgets"), 12, PrDiffView::Summary),
            ),
            (
                "pr://acme/widgets/12/diff/all",
                target(Some("acme/widgets"), 12, PrDiffView::All),
            ),
            (
                "pr://acme/widgets/12/diff/2",
                target(Some("acme/widgets"), 12, PrDiffView::File(2)),
            ),
            (
                "pr://ghe.example.com/acme/widgets/12/diff/2",
                target(
                    Some("ghe.example.com/acme/widgets"),
                    12,
                    PrDiffView::File(2),
                ),
            ),
            (
                "pr://ghe.example.com/acme/widgets/12",
                target(
                    Some("ghe.example.com/acme/widgets"),
                    12,
                    PrDiffView::Summary,
                ),
            ),
            (
                "pr://ghe/acme/widgets/12/diff/all",
                target(Some("ghe/acme/widgets"), 12, PrDiffView::All),
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(parse_pr_url(url), Some(expected), "{url}");
        }
    }

    #[test]
    fn parse_pr_url_rejects_invalid_forms() {
        let invalid = [
            "",
            "https://github.com/acme/widgets/pull/12",
            "issue://12",
            "pr://",
            "pr://0",
            "pr://0/diff",
            "pr://12/diff/0",
            "pr://12/diff/x",
            "pr://12/diff/all/x",
            "pr://12/diff/",
            "pr://12/diff/all?x=1",
            "pr://12/files",
            "pr://12/acme/widgets",
            "pr://acme",
            "pr://acme/widgets",
            "pr://acme/widgets/0",
            "pr://acme/widgets/abc",
            "pr://acme/widgets/12/files",
            "pr://acme//12",
            "pr://acme/widgets/12/diff/../all",
            "pr://-x/widgets/12",
            "pr://acme/wid gets/12",
            "pr://acme/widgets/12?comments=0",
            "pr://ghe.example.com/acme/widgets",
        ];
        for url in invalid {
            assert_eq!(parse_pr_url(url), None, "{url}");
        }
    }

    #[test]
    fn parse_splits_and_numbers_sections() {
        let files = parse_unified(CANNED);
        let summary: Vec<(&str, usize, usize, ChangeType, Option<&str>)> = files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    f.additions,
                    f.deletions,
                    f.change,
                    f.old_path.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("src/lib.rs", 2, 1, ChangeType::Modified, None),
                ("new.txt", 2, 0, ChangeType::Added, None),
                ("old.txt", 0, 1, ChangeType::Deleted, None),
                ("to.txt", 1, 1, ChangeType::Renamed, Some("from.txt")),
                ("img.png", 0, 0, ChangeType::Binary, None),
            ]
        );
        // Sections tile the text exactly, each starting at its `diff --git` line.
        assert_eq!(files[0].start, 0);
        assert_eq!(files.last().unwrap().end, CANNED.len());
        for pair in files.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert_eq!(&CANNED[files[1].start..files[1].end], NEW_TXT_SECTION);
    }

    #[test]
    fn parse_empty_text_has_no_files() {
        assert!(parse_unified("").is_empty());
    }

    #[test]
    fn parse_quoted_header_paths_are_decoded() {
        let text = "diff --git \"a/x y.txt\" \"b/x y.txt\"\n--- \"a/x y.txt\"\n+++ \"b/x y.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_unified(text);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "x y.txt");
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        assert_eq!(
            parse_escape(&"\\101".chars().collect::<Vec<_>>(), 0),
            (b"A".to_vec(), 4)
        );
    }

    #[test]
    fn summary_renders_counts_and_links() {
        let files = parse_unified(CANNED);
        assert_eq!(render_summary("acme/widgets", 12, &files), SUMMARY);
        assert_eq!(
            render_summary("acme/widgets", 12, &[]),
            "# Pull Request Diff: acme/widgets#12 (0 files)\n\n_No file changes._\n\n---\nRead all: `pr://acme/widgets/12/diff/all`. Each file is also available as `pr://acme/widgets/12/diff/<i>`."
        );
    }

    #[test]
    fn synthetic_sections_match_patch_shapes() {
        let added = json!({"filename": "a.txt", "status": "added", "additions": 1, "deletions": 0, "patch": "@@ -0,0 +1 @@\n+x"});
        assert_eq!(
            synthetic_section(&added).unwrap(),
            "diff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+x"
        );
        let renamed = json!({"filename": "new name.txt", "previous_filename": "old.txt", "status": "renamed", "additions": 0, "deletions": 0});
        assert_eq!(
            synthetic_section(&renamed).unwrap(),
            "diff --git a/old.txt \"b/new name.txt\"\nrename from old.txt\nrename to \"new name.txt\"\n* patch unavailable (binary or too large); additions 0, deletions 0"
        );
        assert_eq!(synthetic_section(&json!({"status": "added"})), None);
        assert_eq!(synthetic_path("a/", "tab\there"), "\"a/tab\\there\"");
    }

    #[test]
    fn quoted_rename_paths_round_trip() {
        let renamed = json!({"filename": "dir/new\nname.txt", "previous_filename": "old \"q\".txt", "status": "renamed", "additions": 0, "deletions": 0});
        let files = parse_unified(&synthetic_section(&renamed).unwrap());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "dir/new\nname.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old \"q\".txt"));
        assert_eq!(files[0].change, ChangeType::Renamed);
    }

    #[test]
    fn too_large_errors_are_detected() {
        assert!(is_too_large_error(
            "gh: HTTP 406: Sorry, the diff exceeded the maximum number of lines (20000)"
        ));
        assert!(is_too_large_error("{\"code\":\"too_large\"}"));
        assert!(!is_too_large_error("gh: HTTP 4060 unrelated"));
        assert!(!is_too_large_error("Could not resolve to a PullRequest"));
    }

    #[test]
    fn run_summary_invokes_gh_pr_diff_and_renders_listing() {
        let fake = FakeGh::new(CANNED, "", 0);
        let (_dir, ws) = workspace();
        let out = run(
            "pr_diff",
            &ws,
            &json!({"repo": "acme/widgets", "pr": 12}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text, SUMMARY);
        assert_eq!(
            fake.last_args(),
            strings(&[
                "pr",
                "diff",
                "12",
                "--color",
                "never",
                "--repo",
                "acme/widgets"
            ])
        );
    }

    #[test]
    fn run_file_slice_from_url_is_verbatim_section() {
        let _fake = FakeGh::new(CANNED, "", 0);
        let (_dir, ws) = workspace();
        let out = run(
            "pr_diff",
            &ws,
            &json!({"pr": "pr://acme/widgets/12/diff/2"}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(out.text, NEW_TXT_SECTION);
    }

    #[test]
    fn run_all_returns_full_diff_verbatim() {
        let _fake = FakeGh::new(CANNED, "", 0);
        let (_dir, ws) = workspace();
        let out = run(
            "pr_diff",
            &ws,
            &json!({"repo": "acme/widgets", "pr": 12, "all": true}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(out.text, CANNED);
    }

    #[test]
    fn run_file_out_of_range_is_an_error() {
        let _fake = FakeGh::new(CANNED, "", 0);
        let (_dir, ws) = workspace();
        let err = run(
            "pr_diff",
            &ws,
            &json!({"repo": "acme/widgets", "pr": 12, "file": 9}),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "pr://acme/widgets/12/diff/9 is out of range; PR has 5 files. Use pr://acme/widgets/12/diff to list available indices."
        );
    }

    #[test]
    fn run_file_zero_is_out_of_range() {
        let _fake = FakeGh::new(CANNED, "", 0);
        let (_dir, ws) = workspace();
        let err = run(
            "pr_diff",
            &ws,
            &json!({"repo": "acme/widgets", "pr": 12, "file": 0}),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("diff/0 is out of range"), "{err}");
    }

    #[test]
    fn run_rejects_conflicting_view_and_repo_before_calling_gh() {
        let (_dir, ws) = workspace();
        let cancel = AtomicBool::new(false);
        let err = run(
            "pr_diff",
            &ws,
            &json!({"pr": "pr://12/diff/2", "file": 3}),
            &cancel,
        )
        .unwrap_err();
        assert!(err.contains("already selects a diff view"), "{err}");
        let err = run(
            "pr_diff",
            &ws,
            &json!({"pr": "pr://acme/widgets/12", "repo": "other/repo"}),
            &cancel,
        )
        .unwrap_err();
        assert!(err.contains("conflicts with the pr:// URL repo"), "{err}");
        let err = run(
            "pr_diff",
            &ws,
            &json!({"pr": 12, "all": true, "file": 1}),
            &cancel,
        )
        .unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn run_rejects_unknown_op_and_bad_pr() {
        let (_dir, ws) = workspace();
        let cancel = AtomicBool::new(false);
        assert!(run("pr_view", &ws, &json!({"pr": 1}), &cancel).is_err());
        assert!(run("pr_diff", &ws, &json!({}), &cancel).is_err());
        assert!(
            run(
                "pr_diff",
                &ws,
                &json!({"pr": "https://github.com/acme/widgets/pull/1"}),
                &cancel
            )
            .is_err()
        );
    }
}
