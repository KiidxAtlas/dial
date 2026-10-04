//! File tools: `read` (plain mode), `write` and the exact-text `edit` used in replace mode.

use super::select::{
    self, DEFAULT_READ_LINES, MAX_READ_LINE, MAX_READ_LINES, MAX_READ_OUTPUT, Selector, empty_read, plan_spans,
};
use super::{EditMode, MAX_LIST, Workspace, hashline, io_message, opt_bool, opt_u64, req_str};
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

const MAX_WRITE: usize = 1024 * 1024;
const MAX_EDIT_FILE: u64 = 4 * 1024 * 1024;

/// Read one line, keeping at most `cap` bytes of it. `None` at EOF; otherwise
/// whether the line was truncated. Memory stays bounded for any input.
fn read_line_capped(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<Option<bool>> {
    buf.clear();
    let mut seen = false;
    let mut truncated = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(seen.then_some(truncated));
        }
        seen = true;
        let newline = chunk.iter().position(|&b| b == b'\n');
        let consumed = newline.map_or(chunk.len(), |i| i + 1);
        let content = &chunk[..newline.unwrap_or(chunk.len())];
        let room = cap.saturating_sub(buf.len());
        truncated |= content.len() > room;
        buf.extend_from_slice(&content[..content.len().min(room)]);
        reader.consume(consumed);
        if newline.is_some() {
            if !truncated && buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(truncated));
        }
    }
}

pub(super) fn list_dir(ws: &Workspace, dir: &Path) -> Result<String, String> {
    let mut names = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| io_message("listing", &ws.rel(dir), &e))?;
    for entry in entries.flatten() {
        let kind = entry.file_type().ok();
        let mut name = entry.file_name().to_string_lossy().into_owned();
        match kind {
            Some(k) if k.is_dir() => name.push('/'),
            Some(k) if k.is_symlink() => name.push('@'),
            _ => {}
        }
        names.push(name);
    }
    names.sort();
    let total = names.len();
    let mut text = format!("{} is a directory ({total} entries; '/' dir, '@' symlink):\n", ws.rel(dir));
    for name in names.iter().take(MAX_LIST) {
        text.push_str(name);
        text.push('\n');
    }
    if total > MAX_LIST {
        text.push_str(&format!("[{} more entries not shown]\n", total - MAX_LIST));
    }
    Ok(text)
}

pub(super) fn read_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let input = req_str(args, "path")?;
    if let Some(result) = super::documents::read(ws, args) {
        return result;
    }
    let offset = opt_u64(args, "offset")?;
    if offset == Some(0) {
        return Err("offset is 1-based and must be at least 1".to_string());
    }
    let limit = opt_u64(args, "limit")?.map_or(DEFAULT_READ_LINES, |l| l as usize);
    if limit == 0 {
        return Err("limit must be at least 1".to_string());
    }
    let limit = limit.min(MAX_READ_LINES);
    let (path_text, sel) = select::split_target(ws, input)?;
    if offset.is_some() && sel.selects_lines() {
        return Err("use either offset or a line selector on the path (`path:50-100`), not both".to_string());
    }
    if crate::llm::images::is_image(Path::new(path_text)) {
        if offset.is_some() || sel.selects_lines() || sel.raw {
            return Err("images do not support line selectors, offsets, or raw text output".into());
        }
        let path = ws.resolve(path_text)?;
        let meta = fs::metadata(&path).map_err(|e| io_message("reading image", input, &e))?;
        if !meta.is_file() || meta.len() > crate::llm::images::MAX_IMAGE_BYTES {
            return Err("image must be a regular file no larger than 5 MiB".into());
        }
        return Ok(format!("Image: {path_text} ({} bytes; attached to the next model request when vision is available)", meta.len()));
    }
    let offset = offset.map_or(1, |o| o.min(usize::MAX as u64) as usize);
    match ws.edit_mode {
        EditMode::Hashline => hashline::read(ws, path_text, &sel, offset, limit),
        EditMode::Replace => read_plain(ws, path_text, &sel, offset, limit),
    }
}

/// Lines in the rest of `reader`, which sits on a line boundary.
fn count_remaining_lines(reader: &mut impl BufRead) -> io::Result<usize> {
    let mut lines = 0;
    let mut last = b'\n';
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let Some(&end) = chunk.last() else { break };
        lines += chunk.iter().filter(|&&b| b == b'\n').count();
        last = end;
        let consumed = chunk.len();
        reader.consume(consumed);
    }
    Ok(lines + usize::from(last != b'\n'))
}

/// Streaming read of the lines a selector picks, in constant memory.
/// `sel.raw` drops the `     N\t` row prefix. Used by replace mode and as the
/// raw fallback for files too large for hashline.
pub(super) fn read_plain(ws: &Workspace, input: &str, sel: &Selector, offset: usize, limit: usize) -> Result<String, String> {
    let path = ws.resolve(input)?;
    let meta = fs::metadata(&path).map_err(|e| io_message("reading", input, &e))?;
    if meta.is_dir() {
        return list_dir(ws, &path);
    }
    let fail = |e: io::Error| io_message("reading", input, &e);
    let open = || fs::File::open(&path).map(BufReader::new).map_err(fail);
    // Only a tail selector needs the length before the first row.
    let known_len = if sel.tail.is_some() { Some(count_remaining_lines(&mut open()?).map_err(fail)?) } else { None };
    let spans = plan_spans(sel, offset, limit, known_len.unwrap_or(usize::MAX));

    let mut reader = open()?;
    let mut buf = Vec::new();
    let mut consumed = 0usize; // lines pulled from the reader
    let mut out = String::new();
    let mut shown: Vec<(usize, usize)> = Vec::new();
    let mut stopped_at = None;
    let mut at_eof = false;
    'spans: for span in &spans {
        while consumed + 1 < span.start {
            if read_line_capped(&mut reader, &mut buf, 1).map_err(fail)?.is_none() {
                at_eof = true;
                break 'spans;
            }
            consumed += 1;
        }
        let mut emitted = 0usize;
        let mut last = 0usize; // last line of this span actually returned
        while consumed < span.end {
            let Some(cut) = read_line_capped(&mut reader, &mut buf, MAX_READ_LINE).map_err(fail)? else {
                at_eof = true;
                break;
            };
            consumed += 1;
            if buf.contains(&0) {
                return Err(format!("{input:?} looks like a binary file (NUL byte at line {consumed})"));
            }
            let text = String::from_utf8_lossy(&buf);
            let mut row = if sel.raw { text.into_owned() } else { format!("{consumed:>6}\t{text}") };
            if cut {
                row.push_str(" …[line truncated]");
            }
            row.push('\n');
            if out.len() + row.len() > MAX_READ_OUTPUT {
                stopped_at = Some(consumed);
                break;
            }
            if emitted == 0 && !shown.is_empty() && !sel.raw {
                out.push_str("…\n");
            }
            out.push_str(&row);
            emitted += 1;
            last = consumed;
        }
        if emitted > 0 {
            shown.push((last + 1 - emitted, last));
        }
        if at_eof || stopped_at.is_some() {
            break;
        }
    }
    let len = match known_len {
        Some(len) => len,
        None if at_eof => consumed,
        None => consumed + count_remaining_lines(&mut reader).map_err(fail)?,
    };
    if shown.is_empty() && stopped_at.is_none() {
        return Ok(empty_read(len));
    }
    let capped = stopped_at.is_some();
    let next = stopped_at.or_else(|| spans.last().filter(|s| s.bounded && s.end < len).map(|s| s.end + 1));
    out.push_str(&select::footer(&ws.rel(&path), sel, &shown, len, next, capped));
    Ok(out)
}

pub(super) fn write_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let input = req_str(args, "path")?;
    let content = req_str(args, "content")?;
    if content.len() > MAX_WRITE {
        return Err(format!(
            "content is {} bytes; the write limit is {MAX_WRITE} bytes (1 MiB)",
            content.len()
        ));
    }
    let path = ws.resolve(input)?;
    if path == ws.root || path.is_dir() {
        return Err(format!("{input:?} is a directory"));
    }
    let existed = path.exists();
    let before = if existed {
        fs::metadata(&path).ok().filter(|m| m.len() <= MAX_EDIT_FILE)
            .and_then(|_| fs::read_to_string(&path).ok())
    } else { Some(String::new()) };
    ws.observe_write(&path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_message("creating directories for", input, &e))?;
    }
    fs::write(&path, content).map_err(|e| io_message("writing", input, &e))?;
    let mut message = format!(
        "{} {} bytes to {}",
        if existed { "overwrote" } else { "created" },
        content.len(),
        ws.rel(&path)
    );
    if ws.edit_mode == EditMode::Hashline {
        message.push('\n');
        message.push_str(&hashline::note_write(ws, &path, content));
    }
    if let Some(before) = before { message.push_str(&super::diff::preview(&before, content)); }
    Ok(message)
}

pub(super) fn edit_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let input = req_str(args, "path")?;
    let old = req_str(args, "old")?;
    let new = req_str(args, "new")?;
    let replace_all = opt_bool(args, "replace_all")?;
    if old.is_empty() {
        return Err("`old` must not be empty; use write to create or replace a whole file".to_string());
    }
    if old == new {
        return Err("`old` and `new` are identical; nothing to change".to_string());
    }
    let path = ws.resolve(input)?;
    let meta = fs::metadata(&path).map_err(|e| io_message("editing", input, &e))?;
    if !meta.is_file() {
        return Err(format!("{input:?} is not a regular file"));
    }
    if meta.len() > MAX_EDIT_FILE {
        return Err(format!("{input:?} is {} bytes; edit supports files up to {MAX_EDIT_FILE} bytes", meta.len()));
    }
    let bytes = fs::read(&path).map_err(|e| io_message("reading", input, &e))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{input:?} is not valid UTF-8"))?;
    // A model writes `\n`; a CRLF file stores `\r\n`. Match and write in the file's own convention.
    let crlf;
    let (old, new) = if !text.contains(old) && text.contains("\r\n") && old.contains('\n') && !old.contains('\r') {
        crlf = (old.replace('\n', "\r\n"), new.replace("\r\n", "\n").replace('\n', "\r\n"));
        (crlf.0.as_str(), crlf.1.as_str())
    } else {
        (old, new)
    };
    let count = text.matches(old).count();
    match count {
        0 => match super::tolerant::find(&text, old, new) {
            super::tolerant::Outcome::Unique(found) => {
                let mut note = format!("`old` differed from the file only in whitespace; matched lines {}-{}", found.lines.0, found.lines.1);
                if let Some(how) = &found.reindent { note.push_str("; "); note.push_str(how); }
                write_edited(ws, &path, input, &text, &found.updated, 1).map(|message| match message.split_once('\n') {
                    Some((first, rest)) => format!("{first} ({note})\n{rest}"),
                    None => format!("{message} ({note})"),
                })
            }
            super::tolerant::Outcome::Ambiguous(lines) => Err(format!(
                "`old` was not found byte-for-byte in {input:?}, and once whitespace is ignored it matches {} places (starting at lines {}); add the lines around the one you mean so it is unique, and copy their exact whitespace",
                lines.len(), lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
            )),
            super::tolerant::Outcome::None => Err(format!("`old` was not found in {input:?}.{}", diagnose_miss(&text, old))),
        },
        1 => write_edited(ws, &path, input, &text, &text.replacen(old, new, 1), 1),
        n if replace_all => write_edited(ws, &path, input, &text, &text.replace(old, new), n),
        n => Err(format!(
            "`old` matches {n} places in {input:?} (starting at lines {}); add the lines around the one you mean to make `old` unique, or set replace_all to true",
            match_lines(&text, old)
        )),
    }
}

/// 1-based line numbers where `needle` starts, for the first few matches.
fn match_lines(text: &str, needle: &str) -> String {
    const SHOWN: usize = 8;
    let mut lines: Vec<String> = text
        .match_indices(needle)
        .take(SHOWN)
        .map(|(at, _)| (text[..at].matches('\n').count() + 1).to_string())
        .collect();
    if text.matches(needle).count() > SHOWN {
        lines.push("…".into());
    }
    lines.join(", ")
}

fn strip_row_number(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| rest[digits..].strip_prefix('\t').or_else(|| rest[digits..].strip_prefix(':'))).flatten()
}

/// Why `old` did not match, as concrete advice. The usual causes for a model are copying the line
/// numbers `read` prints, getting indentation or trailing whitespace wrong, and a block that drifted
/// from the file. Showing the real text lets it fix the call in one step instead of re-reading blindly.
fn diagnose_miss(text: &str, old: &str) -> String {
    const SHOWN: usize = 14;
    let old_lines: Vec<&str> = old.lines().collect();
    let content: Vec<&str> = old_lines.iter().copied().filter(|l| !l.trim().is_empty()).collect();
    if !content.is_empty() && content.iter().all(|l| strip_row_number(l).is_some()) {
        return " `old` starts each line with a line number from `read` output (like `    12\\t`); those are not part of the file. Copy only the text after the tab.".into();
    }
    let file_lines: Vec<&str> = text.lines().collect();
    let trimmed: Vec<&str> = old_lines.iter().map(|l| l.trim()).collect();
    let n = trimmed.len();
    let similar = (n > 0 && n <= file_lines.len())
        .then(|| (0..=file_lines.len() - n).find(|&i| file_lines[i..i + n].iter().map(|l| l.trim()).eq(trimmed.iter().copied())))
        .flatten();
    if let Some(at) = similar {
        let shown: String = (at..(at + n).min(at + SHOWN)).map(|i| format!("{}:{}\n", i + 1, file_lines[i])).collect();
        return format!(
            " The same text exists at lines {}-{} but its whitespace (indentation, tabs, trailing spaces) differs. Actual text:\n{shown}Copy `old` exactly from these lines, leading whitespace included.",
            at + 1,
            at + n
        );
    }
    let Some(anchor) = content.first().map(|l| l.trim()).filter(|l| l.len() >= 4) else {
        return " Matching is byte-exact (tabs vs spaces, line endings). Read the lines you want to change and copy them exactly.".into();
    };
    let near: Vec<String> = file_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(anchor))
        .take(3)
        .map(|(i, l)| format!("  {}:{}", i + 1, l))
        .collect();
    if near.is_empty() {
        return format!(" Not even its first line (`{}`) occurs in the file; the file may have changed. Read it again before editing.", crate::agent::truncate(anchor, 80));
    }
    format!(
        " Its first line occurs in the file but the lines after it differ:\n{}\nRead that region (for example `path:N-M`) and copy the exact text; matching is byte-exact.",
        near.join("\n")
    )
}

fn write_edited(ws: &Workspace, path: &Path, input: &str, before: &str, updated: &str, count: usize) -> Result<String, String> {
    if updated.len() > MAX_EDIT_FILE as usize {
        return Err(format!("edited content would exceed {MAX_EDIT_FILE} bytes"));
    }
    ws.observe_write(path)?;
    fs::write(path, updated).map_err(|e| io_message("writing", input, &e))?;
    Ok(format!("replaced {count} occurrence{} in {input}{}", if count == 1 { "" } else { "s" }, super::diff::preview(before, updated)))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::*;
    use serde_json::json;
    #[test]
    fn read_numbers_lines_and_pages() {
        let (dir, ws) = workspace();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join("f.txt"), &body).unwrap();
        let out = run(&ws, "read", json!({"path": "f.txt", "offset": 3, "limit": 2}));
        assert!(!out.is_error);
        assert!(out.text.contains("     3\tline 3\n     4\tline 4\n"), "{}", out.text);
        assert!(!out.text.contains("line 5\n"));
        assert!(out.text.ends_with("[lines 3-4 of 10; continue with `f.txt:5-`]\n"), "{}", out.text);
        let past = run(&ws, "read", json!({"path": "f.txt", "offset": 99}));
        assert!(past.text.contains("no lines in that range; file has 10 lines"), "{}", past.text);
        assert!(run(&ws, "read", json!({"path": "f.txt", "offset": 0})).is_error);
        fs::write(dir.path().join("empty"), "").unwrap();
        assert_eq!(run(&ws, "read", json!({"path": "empty"})).text, "(empty file)");
        fs::write(dir.path().join("bin"), b"abc\0def").unwrap();
        assert!(run(&ws, "read", json!({"path": "bin"})).text.contains("binary"));
        let missing = run(&ws, "read", json!({"path": "missing"}));
        assert!(missing.is_error && missing.text.contains("no such file"));
        let listing = run(&ws, "read", json!({"path": "."}));
        assert!(listing.text.contains("f.txt") && listing.text.contains("directory"));
    }

    #[test]
    fn read_output_is_capped_at_64_kib() {
        let (dir, ws) = workspace();
        let line = "x".repeat(1000);
        let body: String = (0..500).map(|_| format!("{line}\n")).collect();
        fs::write(dir.path().join("big.txt"), &body).unwrap();
        let out = run(&ws, "read", json!({"path": "big.txt", "limit": 10000}));
        assert!(!out.is_error);
        assert!(out.text.len() <= MAX_READ_OUTPUT + 100, "{}", out.text.len());
        assert!(out.text.contains("output truncated") && out.text.contains("continue with `big.txt:"), "{}", out.text);
        // A single enormous line is truncated, not buffered.
        fs::write(dir.path().join("long.txt"), "y".repeat(5_000_000)).unwrap();
        let long = run(&ws, "read", json!({"path": "long.txt"}));
        assert!(long.text.len() < 4000 && long.text.contains("line truncated"));
    }

    fn numbered_file(dir: &tempfile::TempDir, name: &str, lines: usize) {
        let body: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join(name), body).unwrap();
    }

    #[test]
    fn read_accepts_path_selectors_for_ranges_tail_and_multiple_ranges() {
        let (dir, ws) = workspace();
        numbered_file(&dir, "f.txt", 20);
        let read = |path: &str| run(&ws, "read", json!({ "path": path }));
        assert_eq!(
            read("f.txt:3-4").text,
            "     3\tline 3\n     4\tline 4\n[lines 3-4 of 20]\n"
        );
        assert_eq!(read("f.txt:3+2").text, read("f.txt:3-4").text);
        let tail = read("f.txt:-2").text;
        assert_eq!(tail, "    19\tline 19\n    20\tline 20\n[lines 19-20 of 20]\n");
        let multi = read("f.txt:2-3,18-").text;
        assert_eq!(multi, "     2\tline 2\n     3\tline 3\n…\n    18\tline 18\n    19\tline 19\n    20\tline 20\n[lines 2-3, 18-20 of 20]\n");
        assert_eq!(read("f.txt:raw:19-20").text, "line 19\nline 20\n");
        assert!(read("f.txt:0").is_error);
        assert!(read("f.txt:9-3").is_error);
        assert!(read("f.txt:1-20").text.ends_with("    20\tline 20\n[lines 1-20 of 20]\n"));
    }

    #[test]
    fn read_ranges_clip_at_the_end_of_the_file() {
        let (dir, ws) = workspace();
        numbered_file(&dir, "f.txt", 5);
        let read = |path: &str| run(&ws, "read", json!({ "path": path })).text;
        assert_eq!(read("f.txt:4-99"), "     4\tline 4\n     5\tline 5\n[lines 4-5 of 5]\n");
        assert!(read("f.txt:6-9").contains("no lines in that range; file has 5 lines"));
        // A range past the end is skipped; the valid one is still returned.
        assert_eq!(read("f.txt:2-2,50-60"), "     2\tline 2\n[lines 2 of 5]\n");
        assert!(read("f.txt:-100").contains("[lines 1-5 of 5]"));
        // Open-ended ranges are bounded by `limit` and say how to continue.
        let paged = run(&ws, "read", json!({"path": "f.txt:2-", "limit": 2})).text;
        assert!(paged.ends_with("[lines 2-3 of 5; continue with `f.txt:4-`]\n"), "{paged}");
        // An empty file has no lines for any selector.
        fs::write(dir.path().join("empty"), "").unwrap();
        assert_eq!(read("empty:3-5"), "(empty file)");
        assert_eq!(read("empty:-3"), "(empty file)");
    }

    #[test]
    fn read_without_trailing_newline_counts_the_last_line() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("f.txt"), "a\nb\nc").unwrap();
        let out = run(&ws, "read", json!({"path": "f.txt:-1"})).text;
        assert_eq!(out, "     3\tc\n[lines 3 of 3]\n");
        let head = run(&ws, "read", json!({"path": "f.txt", "limit": 1})).text;
        assert!(head.ends_with("[lines 1 of 3; continue with `f.txt:2-`]\n"), "{head}");
    }

    #[test]
    fn read_rejects_offset_combined_with_a_selector_and_reads_odd_names_literally() {
        let (dir, ws) = workspace();
        numbered_file(&dir, "f.txt", 5);
        let both = run(&ws, "read", json!({"path": "f.txt:2-3", "offset": 2}));
        assert!(both.is_error && both.text.contains("not both"), "{}", both.text);
        fs::write(dir.path().join("weird:5"), "x\n").unwrap();
        assert_eq!(run(&ws, "read", json!({"path": "weird:5"})).text, "     1\tx\n");
    }

    #[test]
    fn write_creates_parents_and_enforces_limit() {
        let (dir, ws) = workspace();
        let out = run(&ws, "write", json!({"path": "a/b/c.txt", "content": "hi"}));
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(), "hi");
        assert!(run(&ws, "write", json!({"path": "a/b/c.txt", "content": "again"})).text.contains("overwrote"));
        let big = "z".repeat(MAX_WRITE + 1);
        let refused = run(&ws, "write", json!({"path": "big", "content": big}));
        assert!(refused.is_error && !dir.path().join("big").exists());
        let exact = "z".repeat(MAX_WRITE);
        assert!(!run(&ws, "write", json!({"path": "big", "content": exact})).is_error);
        assert!(run(&ws, "write", json!({"path": "a", "content": "x"})).is_error);
    }

    #[test]
    fn edit_requires_unique_match_unless_replace_all() {
        let (dir, ws) = workspace();
        let file = dir.path().join("e.txt");
        fs::write(&file, "foo bar foo\n").unwrap();
        let ambiguous = run(&ws, "edit", json!({"path": "e.txt", "old": "foo", "new": "baz"}));
        assert!(ambiguous.is_error && ambiguous.text.contains("matches 2 places"), "{}", ambiguous.text);
        assert_eq!(fs::read_to_string(&file).unwrap(), "foo bar foo\n");
        let missing = run(&ws, "edit", json!({"path": "e.txt", "old": "nope", "new": "x"}));
        assert!(missing.is_error && missing.text.contains("not found"));
        let unique = run(&ws, "edit", json!({"path": "e.txt", "old": "bar", "new": "BAR"}));
        assert!(!unique.is_error);
        assert_eq!(fs::read_to_string(&file).unwrap(), "foo BAR foo\n");
        let all = run(&ws, "edit", json!({"path": "e.txt", "old": "foo", "new": "baz", "replace_all": true}));
        assert!(all.text.contains("2 occurrences"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "baz BAR baz\n");
        assert!(run(&ws, "edit", json!({"path": "e.txt", "old": "", "new": "x"})).is_error);
        assert!(run(&ws, "edit", json!({"path": "e.txt", "old": "baz", "new": "baz"})).is_error);
    }

    fn edit_err(ws: &Workspace, path: &str, old: &str) -> String {
        let out = run(ws, "edit", json!({"path": path, "old": old, "new": "REPLACED"}));
        assert!(out.is_error, "{}", out.text);
        out.text
    }

    #[test]
    fn a_whitespace_only_slip_is_applied_and_reported_when_the_match_is_unique_and_uniform() {
        let (dir, ws) = workspace();
        let file = dir.path().join("a.py");
        fs::write(&file, "def f():\n    x = 1\n    return x\n").unwrap();
        // The model dropped two spaces of indentation from every line of the block.
        let out = run(&ws, "edit", json!({"path": "a.py", "old": "  x = 1\n  return x", "new": "  x = 2\n  return x"}));
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("only in whitespace") && out.text.contains("lines 2-3") && out.text.contains("2 spaces more"), "{}", out.text);
        assert_eq!(fs::read_to_string(&file).unwrap(), "def f():\n    x = 2\n    return x\n");
    }

    #[test]
    fn an_ambiguous_or_uneven_whitespace_slip_is_refused_and_shows_the_real_text() {
        let (dir, ws) = workspace();
        let file = dir.path().join("a.py");
        let original = "def f():\n    x = 1\n    return x\n\ndef g():\n    x = 1\n    return x\n";
        fs::write(&file, original).unwrap();
        let err = edit_err(&ws, "a.py", "x = 1\nreturn x");
        assert!(err.contains("matches 2 places") && err.contains("lines 2, 6"), "{err}");
        fs::write(&file, "def f():\n    x = 1\n    return x\n").unwrap();
        // Two spaces short on one line, four on the next: no single shift, so nothing is guessed.
        let err = edit_err(&ws, "a.py", "def f():\n  x = 1\n return x");
        assert!(err.contains("lines 1-3") && err.contains("whitespace") && err.contains("2:    x = 1"), "{err}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "def f():\n    x = 1\n    return x\n");
    }

    #[test]
    fn a_missed_edit_that_copied_read_line_numbers_says_so() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let err = edit_err(&ws, "a.txt", "     1\talpha\n     2\tbeta");
        assert!(err.contains("line number") && err.contains("after the tab"), "{err}");
    }

    #[test]
    fn a_drifted_block_points_at_where_its_first_line_is() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.rs"), "fn a() {}\nfn target() {\n    new_body();\n}\n").unwrap();
        let err = edit_err(&ws, "a.rs", "fn target() {\n    old_body();\n}");
        assert!(err.contains("first line occurs") && err.contains("2:fn target() {"), "{err}");
        let gone = edit_err(&ws, "a.rs", "fn missing_entirely() {}");
        assert!(gone.contains("Not even its first line"), "{gone}");
    }

    #[test]
    fn an_ambiguous_edit_lists_where_the_matches_are() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "x = 1\nfoo\nbar\nfoo\n").unwrap();
        let err = edit_err(&ws, "a.txt", "foo");
        assert!(err.contains("matches 2 places") && err.contains("lines 2, 4"), "{err}");
    }

    #[test]
    fn a_crlf_file_is_edited_with_lf_text_and_stays_crlf() {
        let (dir, ws) = workspace();
        let file = dir.path().join("w.txt");
        fs::write(&file, "one\r\ntwo\r\nthree\r\n").unwrap();
        let out = run(&ws, "edit", json!({"path": "w.txt", "old": "one\ntwo", "new": "1\n2\n2b"}));
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(fs::read_to_string(&file).unwrap(), "1\r\n2\r\n2b\r\nthree\r\n");
    }
}
