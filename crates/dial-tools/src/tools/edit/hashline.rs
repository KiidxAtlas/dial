//! Hashline: line-anchored reads and patch-style edits.
//!
//! `read` shows `[path#TAG]` followed by `LINE:TEXT` rows. TAG is four
//! uppercase hex characters derived from the SHA-256 of the LF-normalized whole
//! file. `edit` takes one or more `[path#TAG]` sections of operations whose line
//! numbers all refer to that tagged snapshot (never to earlier operations of
//! the same call). The session [`EditStore`] remembers snapshots (tag ->
//! content), which line ranges the model has actually been shown ("seen"), and
//! the named cut/paste registers.
//!
//! Safety properties:
//! * every anchor must lie inside lines that a read (or an earlier edit/write
//!   preview) displayed for that snapshot;
//! * a stale tag is recovered only when a line diff between the snapshot and
//!   the current file proves that every anchor sits on unchanged, contiguous
//!   lines; otherwise the edit is refused with the current context;
//! * a multi-section patch is parsed and validated completely before the first
//!   write, and the write observer sees every target before any file changes;
//! * all paths go through `Workspace::resolve` (no `..` / symlink escapes).
//!
//! Block anchors (`N*`) use the syntax tree when the language is supported; a
//! conservative bracket/indentation heuristic covers plain text and unsupported
//! grammars. Markdown headings span through deeper subsections up to the next
//! heading of equal or higher level. Stale-tag recovery first trims unchanged
//! prefixes and suffixes, then applies a bounded diff to the unmatched middle.

use crate::tools::Workspace;
use crate::tools::fs::select::{
    self, MAX_READ_LINE, READ_FOOTER_RESERVE, Selector, empty_read, plan_spans,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_INPUT: usize = 256 * 1024;
pub const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 5000;
const MAX_DIFF_D: usize = 1000;
const SNAPSHOTS_PER_FILE: usize = 6;
const MAX_FILES: usize = 256;
const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;
const PREVIEW_CONTEXT: usize = 2;
const PREVIEW_MAX_ROWS: usize = 60;
const PREVIEW_LINE_CAP: usize = 240;
const MISMATCH_CONTEXT: usize = 3;
const MISMATCH_MAX_ROWS: usize = 60;

// ------------------------------------------------------------------ text helpers

/// Normalize a raw file, removing only its first metadata BOM.
pub fn normalize(text: &str) -> String {
    normalize_body(text.strip_prefix('\u{feff}').unwrap_or(text)).into_owned()
}
/// Editable text may itself begin with a literal BOM; do not strip it twice.
pub(crate) fn normalize_body(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains("\r\n") {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// Four uppercase hex characters from the SHA-256 of LF-normalized content.
pub fn tag_of(lf: &str) -> String {
    let digest = Sha256::digest(lf.as_bytes());
    format!("{:02X}{:02X}", digest[0], digest[1])
}

/// Lines of LF content. A final newline does not start another line.
pub fn split_lines(lf: &str) -> Vec<&str> {
    if lf.is_empty() {
        return Vec::new();
    }
    lf.strip_suffix('\n').unwrap_or(lf).split('\n').collect()
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        out.push('\n');
    }
    out
}

fn truncate_chars(text: &str, cap: usize) -> (&str, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ seen ranges

/// Sorted, merged, 1-based inclusive line ranges.
#[derive(Debug, Clone, Default)]
struct Seen(Vec<(usize, usize)>);

impl Seen {
    fn from_ranges(ranges: &[(usize, usize)]) -> Seen {
        let mut seen = Seen::default();
        for &(a, b) in ranges {
            seen.0.push((a, b));
        }
        seen.normalize();
        seen
    }

    fn normalize(&mut self) {
        self.0.retain(|&(a, b)| a >= 1 && a <= b);
        self.0.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(self.0.len());
        for &(s, e) in &self.0 {
            match out.last_mut() {
                Some(last) if s <= last.1 + 1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        self.0 = out;
    }

    fn add(&mut self, a: usize, b: usize) {
        self.0.push((a, b));
        self.normalize();
    }

    /// First sub-range of `a..=b` that is not covered.
    fn first_unseen(&self, a: usize, b: usize) -> Option<(usize, usize)> {
        let mut pos = a;
        for &(s, e) in &self.0 {
            if e < pos {
                continue;
            }
            if s > pos {
                return Some((pos, (s - 1).min(b)));
            }
            pos = e + 1;
            if pos > b {
                return None;
            }
        }
        (pos <= b).then_some((pos, b))
    }

    fn unseen_ranges(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut pos = a;
        while pos <= b {
            let Some((lo, hi)) = self.first_unseen(pos, b) else {
                break;
            };
            ranges.push((lo, hi));
            pos = hi + 1;
        }
        ranges
    }

    fn flags(&self, len: usize) -> Vec<bool> {
        let mut flags = vec![false; len];
        for &(a, b) in &self.0 {
            for flag in flags.iter_mut().take(b.min(len)).skip(a - 1) {
                *flag = true;
            }
        }
        flags
    }
}

pub(crate) fn ranges_from_flags(flags: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &flag) in flags.iter().enumerate() {
        match (flag, start) {
            (true, None) => start = Some(i + 1),
            (false, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, flags.len()));
    }
    out
}

// ------------------------------------------------------------------ session store

#[derive(Clone, Debug)]
struct Snapshot {
    tag: String,
    text: Arc<str>,
    seen: Seen,
    raw_revision: Option<[u8; 32]>,
}

/// Session state shared by `read`, `write` and `edit` of one workspace.
#[derive(Clone, Default)]
pub struct EditStore {
    files: HashMap<PathBuf, Vec<Snapshot>>,
    /// Least recently used first.
    lru: Vec<PathBuf>,
    named: HashMap<String, Vec<String>>,
    /// (hash of the patch, consecutive occurrences) of the last no-op edit.
    noop: Option<(u64, u32)>,
}

impl std::fmt::Debug for EditStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditStore")
            .field("files", &self.files.len())
            .field("registers", &self.named.len())
            .finish()
    }
}

impl EditStore {
    /// Remember `lf` (LF-normalized) for `path` and union `seen` into the
    /// ranges displayed for it. Returns the tag.
    pub fn record(&mut self, path: &Path, lf: &str, seen: &[(usize, usize)]) -> String {
        self.record_version(path, lf, None, seen)
    }
    pub(crate) fn record_version(
        &mut self,
        path: &Path,
        lf: &str,
        raw_revision: Option<[u8; 32]>,
        seen: &[(usize, usize)],
    ) -> String {
        let tag = tag_of(lf);
        let snaps = self.files.entry(path.to_path_buf()).or_default();
        if let Some(pos) = snaps
            .iter()
            .position(|s| s.tag == tag && *s.text == *lf && s.raw_revision == raw_revision)
        {
            let mut snap = snaps.remove(pos);
            for &(a, b) in seen {
                snap.seen.0.push((a, b));
            }
            snap.seen.normalize();
            snaps.push(snap);
        } else {
            snaps.push(Snapshot {
                tag: tag.clone(),
                text: Arc::from(lf),
                seen: Seen::from_ranges(seen),
                raw_revision,
            });
            if snaps.len() > SNAPSHOTS_PER_FILE {
                snaps.remove(0);
            }
        }
        self.lru.retain(|p| p != path);
        self.lru.push(path.to_path_buf());
        self.evict();
        tag
    }

    /// Record `lf` with every line marked seen (the model authored or was shown it all).
    pub fn record_all(&mut self, path: &Path, lf: &str) -> String {
        let lines = split_lines(lf).len();
        let ranges = if lines == 0 {
            Vec::new()
        } else {
            vec![(1, lines)]
        };
        self.record(path, lf, &ranges)
    }

    fn bytes(&self) -> usize {
        self.files.values().flatten().map(|s| s.text.len()).sum()
    }

    fn evict(&mut self) {
        while self.lru.len() > MAX_FILES || (self.bytes() > MAX_STORE_BYTES && self.lru.len() > 1) {
            let oldest = self.lru.remove(0);
            self.files.remove(&oldest);
        }
        // A single huge file keeps only its newest snapshot.
        while self.bytes() > MAX_STORE_BYTES {
            let Some(snaps) = self.files.values_mut().find(|s| s.len() > 1) else {
                break;
            };
            snaps.remove(0);
        }
    }

    fn snapshot(&self, path: &Path, tag: &str) -> Option<&Snapshot> {
        self.files.get(path)?.iter().rev().find(|s| s.tag == tag)
    }

    pub(crate) fn check_ranges(
        &self,
        path: &Path,
        lf: &str,
        ranges: &[(usize, usize)],
    ) -> Result<(), String> {
        self.check_version_ranges(path, lf, None, ranges)
    }
    pub(crate) fn check_version_ranges(
        &self,
        path: &Path,
        lf: &str,
        revision: Option<[u8; 32]>,
        ranges: &[(usize, usize)],
    ) -> Result<(), String> {
        let snapshot=self.files.get(path).and_then(|snaps|snaps.iter().rev().find(|snap|snap.text.as_ref()==lf&&snap.raw_revision==revision))
            .ok_or_else(||format!("{} has no current source/read witness (notebook metadata must match too); read its affected lines first",path.display()))?;
        for &(a, b) in ranges {
            if let Some((first, last)) = snapshot.seen.first_unseen(a, b) {
                return Err(format!(
                    "{} lines {first}-{last} were not displayed in full; read them before editing",
                    path.display()
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn witness_flags(&self, path: &Path, lf: &str) -> Vec<bool> {
        self.witness_flags_version(path, lf, None)
    }
    pub(crate) fn witness_flags_version(
        &self,
        path: &Path,
        lf: &str,
        revision: Option<[u8; 32]>,
    ) -> Vec<bool> {
        self.files
            .get(path)
            .and_then(|snaps| {
                snaps
                    .iter()
                    .rev()
                    .find(|s| s.text.as_ref() == lf && s.raw_revision == revision)
            })
            .map_or_else(
                || vec![false; split_lines(lf).len()],
                |s| s.seen.flags(split_lines(lf).len()),
            )
    }
    pub(crate) fn record_flags(&mut self, path: &Path, text: &str, flags: &[bool]) {
        self.record(path, &normalize(text), &ranges_from_flags(flags));
    }
    pub(crate) fn record_transition(&mut self, path: &Path, before: &str, after: &str) {
        let old = normalize(before);
        let prior = self.witness_flags(path, &old);
        self.record_flags(
            path,
            after,
            &transition_flags(&old, &normalize(after), &prior),
        );
    }

    pub fn forget(&mut self, path: &Path) {
        self.files.remove(path);
        self.lru.retain(|p| p != path);
    }
}

pub(crate) fn transition_flags(before: &str, after: &str, prior: &[bool]) -> Vec<bool> {
    let old_count = split_lines(before).len();
    let new_count = split_lines(after).len();
    if prior.len() == old_count && prior.iter().all(|seen| *seen) {
        return vec![true; new_count];
    }
    if before == after {
        return (0..new_count)
            .map(|n| prior.get(n).copied().unwrap_or(false))
            .collect();
    }
    let old: Vec<String> = split_lines(before).into_iter().map(str::to_owned).collect();
    let new: Vec<String> = split_lines(after).into_iter().map(str::to_owned).collect();
    let mut flags = vec![true; new.len()];
    if let Some(map) = line_map(&old, &new) {
        for (bi, ci) in map.into_iter().enumerate() {
            if let Some(ci) = ci {
                flags[ci] = prior.get(bi).copied().unwrap_or(false);
            }
        }
    } else {
        flags.fill(false);
    }
    flags
}

// ------------------------------------------------------------------ patch grammar

#[derive(Debug, Clone, PartialEq)]
enum Anchor {
    Range(usize, usize),
    Block(usize),
    Before(usize),
    After(usize),
    AfterBlock(usize),
    Tail,
}

#[derive(Debug, Clone, PartialEq)]
enum Src {
    Body(Vec<String>),
    Register(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
enum OpKind {
    Put {
        anchor: Anchor,
        src: Src,
    },
    Cut {
        anchor: Anchor,
        name: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct Op {
    line_no: usize,
    kind: OpKind,
    /// Body rows written without the `+` prefix (or as a bare blank line) and taken as content.
    bare: usize,
}

#[derive(Debug, Clone)]
struct Section {
    line_no: usize,
    path: String,
    tag: String,
    ops: Vec<Op>,
    rem: bool,
    mv: Option<String>,
}

fn valid_register_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_line_number(text: &str, what: &str) -> Result<usize, String> {
    let n: usize = text
        .trim()
        .parse()
        .map_err(|_| format!("{what}: expected a line number, got {text:?}"))?;
    if n == 0 {
        return Err(format!("{what}: lines are 1-based"));
    }
    Ok(n)
}

fn parse_anchor(target: &str) -> Result<Anchor, String> {
    let target = target.trim();
    if let Some(rest) = target.strip_prefix('<') {
        return Ok(Anchor::Before(parse_line_number(rest, "`<N`")?));
    }
    if let Some(rest) = target.strip_prefix('>') {
        if rest.trim() == "$" {
            return Ok(Anchor::Tail);
        }
        if let Some(n) = rest.trim().strip_suffix('*') {
            return Ok(Anchor::AfterBlock(parse_line_number(n, "`>N*`")?));
        }
        return Ok(Anchor::After(parse_line_number(rest, "`>N`")?));
    }
    if target.bytes().all(|c| c.is_ascii_digit()) && !target.is_empty() {
        let n = parse_line_number(target, "line")?;
        return Ok(Anchor::Range(n, n));
    }
    if let Some((a, b)) = target.split_once(".=").or_else(|| target.split_once(',')) {
        let (a, b) = (
            parse_line_number(a, "range start")?,
            parse_line_number(b, "range end").map_err(|error| {
                format!("{error}; ranges require both endpoints: use N.=M (N.=N for one line), for example PUT {a}.={a}: to replace line {a}. Use PUT >{a}: to insert after it. Correct the target before retrying; rereading will not fix patch syntax.")
            })?,
        );
        if b < a {
            return Err(format!(
                "reversed range {a}.={b}: the end must not be before the start"
            ));
        }
        return Ok(Anchor::Range(a, b));
    }
    if let Some(n) = target.strip_suffix('*') {
        return Ok(Anchor::Block(parse_line_number(n, "`N*`")?));
    }
    Err(format!(
        "unknown target {target:?}; use N.=M, N*, <N, >N, >N* or >$ (a single line is N.=N). Example: `PUT 12.=12:` then `+` rows with the new text"
    ))
}

fn parse_put(rest: &str) -> Result<OpKind, String> {
    let rest = rest.trim();
    if let Some(target) = rest.strip_suffix(':') {
        return Ok(OpKind::Put {
            anchor: parse_anchor(target)?,
            src: Src::Body(Vec::new()),
        });
    }
    let (target, name) = match rest.split_once('@') {
        Some((target, name)) => {
            let name = name.trim();
            if !valid_register_name(name) {
                return Err(format!(
                    "invalid register name {name:?}; use ASCII letters, digits, `_` or `-`"
                ));
            }
            (target, Some(name.to_owned()))
        }
        None => (rest, None),
    };
    let anchor = parse_anchor(target)?;
    if name.is_none() && matches!(anchor, Anchor::Range(..) | Anchor::Block(_)) {
        return Err(
            "replacing a range or block from a register needs `@name`; to write new text end the line with `:` and add `+` rows"
                .to_owned(),
        );
    }
    Ok(OpKind::Put {
        anchor,
        src: Src::Register(name),
    })
}

fn parse_cut(rest: &str) -> Result<OpKind, String> {
    let (target, name) = match rest.split_once('@') {
        Some((target, name)) => {
            let name = name.trim();
            if !valid_register_name(name) {
                return Err(format!(
                    "invalid register name {name:?}; use ASCII letters, digits, `_` or `-`"
                ));
            }
            (target, Some(name.to_owned()))
        }
        None => (rest, None),
    };
    let anchor = parse_anchor(target)?;
    if !matches!(anchor, Anchor::Range(..) | Anchor::Block(_)) {
        return Err("CUT takes N.=M or N*".to_owned());
    }
    Ok(OpKind::Cut { anchor, name })
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
}

fn parse_header(t: &str, line_no: usize) -> Result<Section, String> {
    let Some(inner) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
        return Err(format!(
            "line {line_no}: malformed section header {t:?}; expected [PATH#TAG]"
        ));
    };
    let Some((path, tag)) = inner.rsplit_once('#') else {
        return Err(format!(
            "line {line_no}: section header {t:?} has no #TAG; copy the full [PATH#TAG] header from the latest read/edit/write of that file"
        ));
    };
    let tag = tag.trim();
    if tag.len() != 4 || !tag.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "line {line_no}: tag {tag:?} is not four hex characters; copy it from the [PATH#TAG] header"
        ));
    }
    let path = path.trim();
    let path = if path.starts_with('"') {
        serde_json::from_str::<String>(path)
            .map_err(|e| format!("line {line_no}: invalid quoted path: {e}"))?
    } else {
        path.to_owned()
    };
    if path.is_empty() {
        return Err(format!("line {line_no}: section header has an empty path"));
    }
    Ok(Section {
        line_no,
        path,
        tag: tag.to_ascii_uppercase(),
        ops: Vec::new(),
        rem: false,
        mv: None,
    })
}

fn close_section(section: Section, out: &mut Vec<Section>) -> Result<(), String> {
    if section.ops.is_empty() && !section.rem && section.mv.is_none() {
        return Err(format!(
            "line {}: section for {:?} has no operations",
            section.line_no, section.path
        ));
    }
    out.push(section);
    Ok(())
}

/// Submitted literal source fragments for pre-execution structural guidance. This uses the
/// actual patch grammar without opening files or resolving mutable source registers.
pub fn proposed_sources(input: &str) -> Result<Vec<(String, String)>, String> {
    let mut sources = Vec::new();
    for section in parse_patch(input)? {
        for op in section.ops {
            if let OpKind::Put { src, .. } = op.kind {
                match src {
                    Src::Body(body) => sources.push((section.path.clone(), body.join("\n"))),
                    Src::Register(_) => return Err("AST rules cannot inspect register-derived source before execution; submit literal PUT rows".into()),
                }
            }
        }
    }
    Ok(sources)
}

fn parse_patch(input: &str) -> Result<Vec<Section>, String> {
    if input.len() > MAX_INPUT {
        return Err(format!(
            "patch is {} bytes; the limit is {MAX_INPUT}",
            input.len()
        ));
    }
    let lines: Vec<&str> = input.lines().collect();
    let (mut start, mut end) = (0, lines.len());
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if start < end && lines[start].trim() == "*** Begin Patch" {
        start += 1;
    }
    if end > start && lines[end - 1].trim() == "*** End Patch" {
        end -= 1;
    }
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;
    let mut collecting = false;
    let rows = &lines[start..end];
    // A row that is not an operation, header or diff marker: inside a PUT body it is content even when
    // the model forgot its `+` (the commonest hashline mistake; rejecting it costs a whole round).
    let bare_row = |t: &str| !t.is_empty() && !is_op_line(t) && !t.starts_with(['-', '*', '+']);
    for (offset, raw) in rows.iter().enumerate() {
        let line_no = start + offset + 1;
        if collecting {
            let t = raw.trim();
            // A blank line continues the body when more body rows follow it; otherwise it ends the body.
            let continues = t.is_empty()
                && rows[offset + 1..]
                    .iter()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .is_some_and(|next| next.starts_with('+') || bare_row(next));
            let (row, bare) = match raw.strip_prefix('+') {
                Some(text) => (Some(text), false),
                None if bare_row(t) => (Some(*raw), true),
                None if continues => (Some(""), true),
                None => (None, false),
            };
            if let Some(text) = row {
                if let Some(Op {
                    kind:
                        OpKind::Put {
                            src: Src::Body(body),
                            ..
                        },
                    bare: count,
                    ..
                }) = current.as_mut().and_then(|s| s.ops.last_mut())
                {
                    body.push(text.to_owned());
                    *count += usize::from(bare);
                }
                continue;
            }
            collecting = false;
            if body_is_empty(current.as_ref()) {
                return Err(format!(
                    "line {line_no}: a `PUT ...:` operation needs at least one `+` row (a blank line is a lone `+`)"
                ));
            }
        }
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('[') {
            if let Some(section) = current.take() {
                close_section(section, &mut sections)?;
            }
            current = Some(parse_header(t, line_no)?);
            continue;
        }
        let Some(section) = current.as_mut() else {
            return Err(format!(
                "line {line_no}: expected a [PATH#TAG] section header before {t:?}"
            ));
        };
        if let Some(rest) = t.strip_prefix("PUT ") {
            let kind = parse_put(rest).map_err(|e| format!("line {line_no}: {e}"))?;
            collecting = matches!(
                &kind,
                OpKind::Put {
                    src: Src::Body(_),
                    ..
                }
            );
            section.ops.push(Op {
                line_no,
                kind,
                bare: 0,
            });
        } else if let Some(rest) = t.strip_prefix("CUT ") {
            let kind = parse_cut(rest).map_err(|e| format!("line {line_no}: {e}"))?;
            section.ops.push(Op {
                line_no,
                kind,
                bare: 0,
            });
        } else if t == "REM" {
            if section.rem {
                return Err(format!("line {line_no}: duplicate REM"));
            }
            section.rem = true;
        } else if let Some(dest) = t.strip_prefix("MV ") {
            let dest = unquote(dest);
            if dest.is_empty() || section.mv.is_some() {
                return Err(format!(
                    "line {line_no}: MV needs exactly one destination path"
                ));
            }
            section.mv = Some(dest.to_owned());
        } else if t.starts_with('+') {
            return Err(format!(
                "line {line_no}: `+` row outside a `PUT ...:` body (blank lines inside a body must be a lone `+`)"
            ));
        } else if t.starts_with('-') || t.starts_with("@@") || t.starts_with("***") {
            return Err(format!(
                "line {line_no}: {t:?} looks like a unified diff; use hashline operations (PUT/CUT/REM/MV) with final-content `+` rows"
            ));
        } else {
            return Err(format!(
                "line {line_no}: unrecognized operation {t:?}; expected PUT, CUT, REM or MV. Example: `PUT 12.=12:` then `+` rows"
            ));
        }
    }
    if collecting && body_is_empty(current.as_ref()) {
        return Err("the last `PUT ...:` operation needs at least one `+` row".to_owned());
    }
    if let Some(section) = current.take() {
        close_section(section, &mut sections)?;
    }
    if sections.is_empty() {
        return Err("empty patch: expected at least one [PATH#TAG] section".to_owned());
    }
    Ok(sections)
}

fn body_is_empty(section: Option<&Section>) -> bool {
    matches!(
        section.and_then(|s| s.ops.last()),
        Some(Op { kind: OpKind::Put { src: Src::Body(body), .. }, .. }) if body.is_empty()
    )
}

fn is_op_line(t: &str) -> bool {
    t.starts_with('[')
        || t.starts_with("PUT ")
        || t.starts_with("CUT ")
        || t == "REM"
        || t.starts_with("MV ")
}

// ------------------------------------------------------------------ block heuristic

const HASH_COMMENT_EXTS: &[&str] = &[
    "py", "sh", "bash", "zsh", "rb", "toml", "yaml", "yml", "pl", "r", "mk", "cfg", "ini", "conf",
    "ex", "exs", "nim",
];
const CHAR_LITERAL_EXTS: &[&str] = &[
    "rs", "c", "h", "cc", "cpp", "hpp", "cs", "java", "go", "kt", "swift", "scala", "zig",
];

struct Scanner {
    balance: i64,
    in_block_comment: bool,
    hash_comments: bool,
    char_literals: bool,
}

impl Scanner {
    fn new(ext: &str) -> Scanner {
        Scanner {
            balance: 0,
            in_block_comment: false,
            hash_comments: HASH_COMMENT_EXTS.contains(&ext),
            char_literals: CHAR_LITERAL_EXTS.contains(&ext),
        }
    }

    /// Feed one line. Returns the line with strings/comments blanked is not
    /// needed; only the running balance and the "ends with colon" flag matter.
    fn line(&mut self, text: &str) -> bool {
        let cs: Vec<char> = text.chars().collect();
        let mut i = 0;
        let mut last_significant = None;
        while i < cs.len() {
            let c = cs[i];
            let next = cs.get(i + 1).copied();
            if self.in_block_comment {
                if c == '*' && next == Some('/') {
                    self.in_block_comment = false;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            match c {
                '/' if !self.hash_comments && next == Some('/') => break,
                '/' if !self.hash_comments && next == Some('*') => {
                    self.in_block_comment = true;
                    i += 2;
                    continue;
                }
                '#' if self.hash_comments => break,
                '"' | '`' => {
                    i = skip_string(&cs, i, c);
                    last_significant = Some(c);
                    continue;
                }
                '\'' => {
                    if self.char_literals {
                        i = skip_char_literal(&cs, i);
                    } else {
                        i = skip_string(&cs, i, '\'');
                    }
                    last_significant = Some(c);
                    continue;
                }
                '(' | '[' | '{' => self.balance += 1,
                ')' | ']' | '}' => self.balance -= 1,
                _ => {}
            }
            if !c.is_whitespace() {
                last_significant = Some(c);
            }
            i += 1;
        }
        last_significant == Some(':')
    }
}

fn skip_string(cs: &[char], start: usize, quote: char) -> usize {
    let mut i = start + 1;
    while i < cs.len() {
        match cs[i] {
            '\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    cs.len()
}

fn skip_char_literal(cs: &[char], start: usize) -> usize {
    // 'x' or '\n' / '\u{..}' ; anything else (a lifetime like 'a) is one char.
    if cs.get(start + 1) == Some(&'\\') {
        let end = (start + 12).min(cs.len());
        if let Some(offset) = cs
            .get(start + 2..end)
            .and_then(|window| window.iter().position(|&c| c == '\''))
        {
            return start + 2 + offset + 1;
        }
    } else if cs.get(start + 2) == Some(&'\'') {
        return start + 3;
    }
    start + 1
}

fn indent_of(line: &str) -> usize {
    let mut width = 0;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += 4,
            _ => break,
        }
    }
    width
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_prefix_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('@') || t.starts_with("#[") || t.starts_with("#![") || t.starts_with("///")
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
            return Some(hashes);
        }
    }
    None
}

/// Last line (1-based, inclusive) of the construct that starts on line `n`.
pub fn block_end<S: AsRef<str>>(lines: &[S], n: usize, ext: &str) -> Result<usize, String> {
    let len = lines.len();
    if n == 0 || n > len {
        return Err(format!("line {n} is outside the file ({len} lines)"));
    }
    let at = |i: usize| lines[i - 1].as_ref();
    if is_blank(at(n)) {
        return Err(format!(
            "line {n} is blank; anchor the first line of the construct (or use an explicit range)"
        ));
    }
    if matches!(ext, "md" | "markdown" | "mdx")
        && let Some(level) = heading_level(at(n))
    {
        return markdown_end(lines, n, level);
    }
    let mut open = n;
    while open < len && is_prefix_line(at(open)) {
        open += 1;
    }
    if is_prefix_line(at(open)) {
        return Err(format!(
            "line {n} is a decorator/attribute with no declaration after it; use an explicit range"
        ));
    }
    let mut scanner = Scanner::new(ext);
    scanner.line(at(open));
    let mut end;
    if scanner.balance < 0 {
        return Err(format!(
            "line {n} starts with a closing delimiter; anchor the line that OPENS the block (or use an explicit range)"
        ));
    }
    if scanner.balance > 0 {
        end = scan_until_balanced(lines, &mut scanner, open, n)?;
        if ends_with_colon_at(lines, end, ext) {
            end = end.max(indent_end(lines, end, indent_of(at(open))));
        }
    } else {
        let allman = open < len
            && at(open + 1).trim_start().starts_with('{')
            && indent_of(at(open + 1)) <= indent_of(at(open));
        if allman {
            end = scan_until_balanced(lines, &mut scanner, open + 1, n).map(|e| e.max(open + 1))?;
        } else {
            end = indent_end(lines, open, indent_of(at(open)));
        }
    }
    if end <= n {
        return Err(format!(
            "line {n} is a single-line construct; use the explicit range form `{n}.={n}` instead of `{n}*`"
        ));
    }
    Ok(end)
}

fn ends_with_colon_at<S: AsRef<str>>(lines: &[S], end: usize, ext: &str) -> bool {
    let mut scanner = Scanner::new(ext);
    scanner.line(lines[end - 1].as_ref())
}

/// Continue feeding lines from `from` (the scanner already consumed `from` or,
/// for Allman style, will consume it now) until the balance returns to zero.
fn scan_until_balanced<S: AsRef<str>>(
    lines: &[S],
    scanner: &mut Scanner,
    from: usize,
    anchor: usize,
) -> Result<usize, String> {
    let len = lines.len();
    let mut i = from;
    if scanner.balance == 0 {
        // Allman: the opener line itself has not been scanned yet.
        scanner.line(lines[i - 1].as_ref());
        if scanner.balance <= 0 {
            return Ok(i);
        }
    }
    while i < len {
        i += 1;
        scanner.line(lines[i - 1].as_ref());
        if scanner.balance < 0 {
            return Err(format!(
                "line {anchor}: brackets are unbalanced near line {i}; use an explicit range"
            ));
        }
        if scanner.balance == 0 {
            return Ok(i);
        }
    }
    Err(format!(
        "line {anchor}: the bracket opened there is never closed; use an explicit range"
    ))
}

/// Last non-blank line after `open` that belongs to an indented body; `open` if none.
fn indent_end<S: AsRef<str>>(lines: &[S], open: usize, base_indent: usize) -> usize {
    let mut end = open;
    for i in open + 1..=lines.len() {
        let line = lines[i - 1].as_ref();
        if is_blank(line) {
            continue;
        }
        if indent_of(line) > base_indent {
            end = i;
        } else {
            break;
        }
    }
    end
}

fn markdown_end<S: AsRef<str>>(lines: &[S], n: usize, level: usize) -> Result<usize, String> {
    let mut in_fence = false;
    let mut end = lines.len();
    for i in n + 1..=lines.len() {
        let line = lines[i - 1].as_ref();
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && heading_level(line).is_some_and(|l| l <= level) {
            end = i - 1;
            break;
        }
    }
    while end > n && is_blank(lines[end - 1].as_ref()) {
        end -= 1;
    }
    if end <= n {
        return Err(format!(
            "heading on line {n} has no body; use the explicit range form `{n}.={n}` instead of `{n}*`"
        ));
    }
    Ok(end)
}

// ------------------------------------------------------------------ line diff

/// Matched (a_index, b_index) pairs of a shortest edit script, or `None` when
/// the scripts exceed `max_d` edits.
fn myers(a: &[u32], b: &[u32], max_d: usize) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 || m == 0 {
        return Some(Vec::new());
    }
    let max = ((n + m) as usize).min(max_d);
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max as isize {
        trace.push(v[(off - d - 1) as usize..=(off + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (off + k) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m, d));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize, last_d: isize) -> Vec<(usize, usize)> {
    let (mut x, mut y) = (n, m);
    let mut pairs = Vec::new();
    for d in (0..=last_d).rev() {
        let w = &trace[d as usize];
        let get = |k: isize| w[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        if d > 0 {
            x = prev_x;
            y = prev_y;
        }
    }
    pairs.reverse();
    pairs
}

fn intern_line<'a>(ids: &mut HashMap<&'a str, u32>, text: &'a str) -> u32 {
    let next = ids.len() as u32;
    *ids.entry(text).or_insert(next)
}

/// For every base line, the index of the identical current line it maps to.
/// `None` means the unmatched middle is too different to map safely.
fn line_map(base: &[String], cur: &[String]) -> Option<Vec<Option<usize>>> {
    // Large files remain recoverable when the changed middle is small: common prefix/suffix
    // detection is linear and Myers sees only the unmatched region below.
    let mut prefix = 0;
    while prefix < base.len() && prefix < cur.len() && base[prefix] == cur[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < base.len() - prefix
        && suffix < cur.len() - prefix
        && base[base.len() - 1 - suffix] == cur[cur.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut map = vec![None; base.len()];
    for (i, slot) in map.iter_mut().enumerate().take(prefix) {
        *slot = Some(i);
    }
    for s in 0..suffix {
        map[base.len() - 1 - s] = Some(cur.len() - 1 - s);
    }
    let (bm, cm) = (
        &base[prefix..base.len() - suffix],
        &cur[prefix..cur.len() - suffix],
    );
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let a: Vec<u32> = bm.iter().map(|t| intern_line(&mut ids, t)).collect();
    let b: Vec<u32> = cm.iter().map(|t| intern_line(&mut ids, t)).collect();
    if a.len() + b.len() > MAX_DIFF_LINES {
        return None;
    }
    for (ai, bi) in myers(&a, &b, MAX_DIFF_D)? {
        map[prefix + ai] = Some(prefix + bi);
    }
    Some(map)
}

/// Changed regions as half-open 0-based `(base_start, base_end, cur_start, cur_end)`.
fn hunks(map: &[Option<usize>], cur_len: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let (mut prev_b, mut prev_c) = (0usize, 0usize); // next unmatched positions
    for (bi, slot) in map.iter().enumerate() {
        if let Some(ci) = *slot {
            if bi > prev_b || ci > prev_c {
                out.push((prev_b, bi, prev_c, ci));
            }
            prev_b = bi + 1;
            prev_c = ci + 1;
        }
    }
    if map.len() > prev_b || cur_len > prev_c {
        out.push((prev_b, map.len(), prev_c, cur_len));
    }
    out
}

// ------------------------------------------------------------------ edits

#[derive(Debug, Clone)]
struct Edit {
    /// Replace: first line (1-based). Insert: gap (after line `lo`, 0 = head).
    lo: usize,
    /// Replace: last line (inclusive). Insert: unused.
    hi: usize,
    insert: bool,
    lines: Vec<String>,
}

/// Translate base-snapshot coordinates onto the current file, or return the
/// base ranges (1-based inclusive) that changed under an anchor.
fn translate(
    edits: &mut [Edit],
    map: &[Option<usize>],
    cur_len: usize,
) -> Result<(), Vec<(usize, usize)>> {
    let base_len = map.len();
    let mut blocked = Vec::new();
    for edit in edits.iter_mut() {
        if edit.insert {
            let g = edit.lo;
            let ok = if base_len == 0 {
                cur_len == 0
            } else if g == 0 {
                map[0] == Some(0)
            } else if g == base_len {
                cur_len > 0 && map[base_len - 1] == Some(cur_len - 1)
            } else {
                matches!((map[g - 1], map[g]), (Some(x), Some(y)) if y == x + 1)
            };
            if ok {
                edit.lo = if g == 0 {
                    0
                } else {
                    map[g - 1].map_or(0, |x| x + 1)
                };
            } else {
                blocked.push((g.max(1), (g + 1).min(base_len.max(1))));
            }
        } else {
            let (a, b) = (edit.lo, edit.hi);
            let contiguous = (a..=b).all(|i| map[i - 1].is_some())
                && (a..b).all(|i| map[i] == map[i - 1].map(|x| x + 1));
            match (contiguous, map[a - 1], map[b - 1]) {
                (true, Some(x), Some(y)) => {
                    edit.lo = x + 1;
                    edit.hi = y + 1;
                }
                _ => blocked.push((a, b)),
            }
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(blocked)
    }
}

fn check_overlap(edits: &[Edit]) -> Result<(), String> {
    let mut replaces: Vec<&Edit> = edits.iter().filter(|e| !e.insert).collect();
    replaces.sort_by_key(|e| e.lo);
    for pair in replaces.windows(2) {
        if pair[1].lo <= pair[0].hi {
            return Err(format!(
                "overlapping edits: lines {}-{} and {}-{} intersect; combine them into one operation",
                pair[0].lo, pair[0].hi, pair[1].lo, pair[1].hi
            ));
        }
    }
    for insert in edits.iter().filter(|e| e.insert) {
        let g = insert.lo;
        // The gap after line g lies strictly inside a replaced range a..=b when a <= g < b.
        let idx = replaces.partition_point(|r| r.lo <= g);
        if idx > 0 && replaces[idx - 1].hi > g {
            return Err(format!(
                "an insertion after line {g} falls inside the replaced lines {}-{}; put it before or after that range",
                replaces[idx - 1].lo,
                replaces[idx - 1].hi
            ));
        }
    }
    Ok(())
}

struct Applied {
    lines: Vec<String>,
    seen: Vec<bool>,
    /// Changed regions in new numbering: `(first, last)`; `last == first - 1` for a pure deletion.
    regions: Vec<(usize, usize)>,
    added: usize,
    removed: usize,
}

fn apply_edits(cur: &[String], cur_seen: &[bool], edits: &[Edit]) -> Applied {
    let mut inserts: BTreeMap<usize, Vec<&Edit>> = BTreeMap::new();
    let mut replaces: BTreeMap<usize, &Edit> = BTreeMap::new();
    for edit in edits {
        if edit.insert {
            inserts.entry(edit.lo).or_default().push(edit);
        } else {
            replaces.insert(edit.lo, edit);
        }
    }
    let mut out = Applied {
        lines: Vec::new(),
        seen: Vec::new(),
        regions: Vec::new(),
        added: 0,
        removed: 0,
    };
    let emit = |out: &mut Applied, body: &[String]| {
        let first = out.lines.len() + 1;
        out.lines.extend(body.iter().cloned());
        out.seen.extend(std::iter::repeat_n(true, body.len()));
        out.added += body.len();
        out.regions.push((first, first + body.len() - 1));
    };
    let mut i = 1;
    loop {
        if let Some(list) = inserts.get(&(i - 1)) {
            for edit in list {
                emit(&mut out, &edit.lines);
            }
        }
        if i > cur.len() {
            break;
        }
        if let Some(edit) = replaces.get(&i) {
            out.removed += edit.hi - edit.lo + 1;
            emit(&mut out, &edit.lines);
            i = edit.hi + 1;
        } else {
            out.lines.push(cur[i - 1].clone());
            out.seen.push(cur_seen[i - 1]);
            i += 1;
        }
    }
    out
}

// ------------------------------------------------------------------ preview

fn preview_rows<S: AsRef<str>>(
    lines: &[S],
    regions: &[(usize, usize)],
    context: usize,
    max_rows: usize,
) -> (String, Vec<(usize, usize)>) {
    let len = lines.len();
    let mut wanted: Vec<(usize, usize)> = Vec::new();
    for &(first, last) in regions {
        if len == 0 {
            continue;
        }
        let lo = first.saturating_sub(context).max(1);
        let hi = (last + context).min(len);
        if lo > hi {
            continue;
        }
        match wanted.last_mut() {
            Some(prev) if lo <= prev.1 + 1 => prev.1 = prev.1.max(hi),
            _ => wanted.push((lo, hi)),
        }
    }
    let mut text = String::new();
    let mut rows = 0;
    let mut shown = Vec::new();
    let mut omitted = 0;
    for (index, &(lo, hi)) in wanted.iter().enumerate() {
        if rows >= max_rows {
            omitted += hi - lo + 1;
            continue;
        }
        if index > 0 {
            text.push_str("…\n");
        }
        let take_hi = hi.min(lo + (max_rows - rows) - 1);
        for n in lo..=take_hi {
            let (shown_text, cut) = truncate_chars(lines[n - 1].as_ref(), PREVIEW_LINE_CAP);
            text.push_str(&format!(
                "{n}:{shown_text}{}\n",
                if cut { " …[line truncated]" } else { "" }
            ));
            if !cut {
                shown.push((n, n));
            }
        }
        rows += take_hi - lo + 1;
        omitted += hi - take_hi;
    }
    if omitted > 0 {
        text.push_str(&format!(
            "[preview truncated; {omitted} more changed/context lines not shown]\n"
        ));
    }
    (text, shown)
}

// ------------------------------------------------------------------ orchestration

struct Job {
    abs: PathBuf,
    rel: String,
    ext: String,
    tag: String,
    cur_lf: String,
    cur: Vec<String>,
    base: Vec<String>,
    seen: Seen,
    enforce_seen: bool,
    /// All unseen ranges blocked during anchor preflight, for [`show_unseen`].
    unseen: Vec<(usize, usize)>,
    original: super::edit_transaction::Original,
    syntactic_blocks: Option<std::collections::BTreeMap<usize, Result<usize, String>>>,
    carrier: Option<super::edit_source::Carrier>,
    raw_revision: Option<[u8; 32]>,
    stale: bool,
    crlf: bool,
    mixed: bool,
    edits: Vec<Edit>,
    rem: bool,
    mv: Option<(PathBuf, String)>,
    warnings: Vec<String>,
}

impl Job {
    fn block_end(&self, line: usize) -> Result<usize, String> {
        match &self.syntactic_blocks {
            Some(blocks) => blocks
                .get(&line)
                .cloned()
                .unwrap_or_else(|| Err("block anchor was not prepared".into())),
            None => block_end(&self.base, line, &self.ext),
        }
    }
    fn check_seen(&mut self, a: usize, b: usize) -> Result<(), String> {
        if !self.enforce_seen {
            return Ok(());
        }
        let ranges = self.seen.unseen_ranges(a, b);
        let first = ranges.first().copied();
        self.unseen.extend(ranges);
        match first {
            None => Ok(()),
            Some((x, y)) => Err(format!(
                "{}#{}: lines {} have not been shown by a read; read `{}:{x}-{y}` first (anchors may only touch lines you have seen)",
                self.rel,
                self.tag,
                if x == y {
                    x.to_string()
                } else {
                    format!("{x}-{y}")
                },
                self.rel
            )),
        }
    }
}

/// Text of an existing workspace file, read through the guarded transaction reader.
pub(crate) fn load_text(ws: &Workspace, abs: &Path, display: &str) -> Result<String, String> {
    let original = super::edit_transaction::read(ws, abs)
        .map_err(|e| format!("reading {display:?}: {e}"))?
        .ok_or_else(|| format!("{display:?}: no such file"))?;
    if original.text.len() as u64 > MAX_FILE {
        return Err(format!(
            "{display:?} is {} bytes; hashline supports files up to {MAX_FILE} bytes",
            original.text.len()
        ));
    }
    if original.text.contains('\0') {
        return Err(format!("{display:?} looks like a binary file"));
    }
    Ok(original.text)
}

fn line_endings(raw: &str) -> (bool, bool) {
    let (crlf, lf) = super::edit_native::text::count_line_endings(raw);
    (
        super::edit_native::text::dominant_line_ending(raw)
            == super::edit_native::text::LineEnding::CrLf,
        crlf > 0 && lf > 0,
    )
}

fn load_job(
    ws: &Workspace,
    store: &EditStore,
    sec: &Section,
    abs: PathBuf,
    block_lines: &[usize],
) -> Result<Job, String> {
    let rel = super::edit_source::display_path(ws.rel(&abs));
    let original =
        super::edit_transaction::read(ws, &abs)?.ok_or_else(|| format!("{rel:?}: no such file"))?;
    let raw = &original.text;
    super::editing::generated_guard(ws, &abs, Some(raw))?;
    let source = super::edit_source::Source::load(&abs, Some(raw))?;
    super::editing::generated_guard(ws, &abs, Some(&source.text))?;
    let (cur_lf, raw_revision, carrier) = source.into_parts();
    let (crlf, mixed) = line_endings(raw);
    let cur_tag = tag_of(&cur_lf);
    let fallback = Snapshot {
        tag: cur_tag.clone(),
        text: Arc::from(cur_lf.as_str()),
        seen: Seen::default(),
        raw_revision,
    };
    let snap = match store.snapshot(&abs, &sec.tag) {
        Some(snapshot) => snapshot,
        None if !ws.edit_settings.enforce_seen_lines && cur_tag == sec.tag => &fallback,
        None => {
            return Err(if cur_tag == sec.tag {
                format!(
                    "{rel}#{}: no read of this file is recorded; read its affected lines first",
                    sec.tag
                )
            } else {
                format!(
                    "{rel}#{}: unknown snapshot tag (current {cur_tag}); read again",
                    sec.tag
                )
            });
        }
    };
    if raw_revision.is_some() && snap.raw_revision != raw_revision {
        return Err(format!(
            "{rel}: notebook raw JSON changed or only a raw view was read; read the editable projection again"
        ));
    }
    let stale = snap.text.as_ref() != cur_lf;
    let cur: Vec<String> = split_lines(&cur_lf)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let base: Vec<String> = if stale {
        split_lines(&snap.text)
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        cur.clone()
    };
    let ext = ext_of(&abs);
    let syntactic_blocks =
        if block_lines.is_empty() || matches!(ext.as_str(), "md" | "markdown" | "mdx") {
            None
        } else {
            super::syntax::block_ends(&abs, if stale { &snap.text } else { &cur_lf }, block_lines)
        };
    Ok(Job {
        ext,
        abs,
        rel,
        tag: sec.tag.clone(),
        cur_lf,
        cur,
        base,
        seen: snap.seen.clone(),
        enforce_seen: ws.edit_settings.enforce_seen_lines,
        unseen: Vec::new(),
        original,
        syntactic_blocks,
        carrier: Some(carrier),
        raw_revision,
        stale,
        crlf,
        mixed,
        edits: Vec::new(),
        rem: false,
        mv: None,
        warnings: Vec::new(),
    })
}

#[derive(Default)]
struct Registers {
    anon: Option<Vec<String>>,
    named: HashMap<String, Vec<String>>,
    /// (name, content, job index) in authored order; published once the job's write lands.
    pending: Vec<(String, Vec<String>, usize)>,
}

impl Registers {
    fn get(&self, name: &Option<String>) -> Result<Vec<String>, String> {
        match name {
            Some(n) => self.named.get(n).cloned().ok_or_else(|| {
                let mut known: Vec<&String> = self.named.keys().collect();
                known.sort();
                format!("unknown register @{n}; it must be filled by an earlier `CUT ... @{n}` (known: {known:?})")
            }),
            None => self
                .anon
                .clone()
                .ok_or_else(|| "the anonymous register is empty; run a `CUT` without @name earlier in this call".to_owned()),
        }
    }
}

enum Resolved {
    Replace(usize, usize),
    Insert(usize),
}

fn resolve_anchor(job: &mut Job, anchor: &Anchor) -> Result<Resolved, String> {
    let len = job.base.len();
    let out_of_range = |job: &Job, n: usize| {
        format!(
            "{}#{}: line {n} is beyond the end of the file ({len} lines)",
            job.rel, job.tag
        )
    };
    Ok(match *anchor {
        Anchor::Range(a, b) => {
            if b > len {
                return Err(out_of_range(job, b));
            }
            job.check_seen(a, b)?;
            Resolved::Replace(a, b)
        }
        Anchor::Block(n) => {
            if n > len {
                return Err(out_of_range(job, n));
            }
            let end = job
                .block_end(n)
                .map_err(|e| format!("{}#{}: {e}", job.rel, job.tag))?;
            job.check_seen(n, end)?;
            Resolved::Replace(n, end)
        }
        Anchor::Before(n) => {
            if len == 0 {
                if n != 1 {
                    return Err(out_of_range(job, n));
                }
                return Ok(Resolved::Insert(0));
            }
            if n > len {
                return Err(out_of_range(job, n));
            }
            job.check_seen(n, n)?;
            Resolved::Insert(n - 1)
        }
        Anchor::After(n) => {
            if n > len {
                return Err(out_of_range(job, n));
            }
            job.check_seen(n, n)?;
            Resolved::Insert(n)
        }
        Anchor::AfterBlock(n) => {
            if n > len {
                return Err(out_of_range(job, n));
            }
            match job.block_end(n) {
                Ok(end) => {
                    job.check_seen(n, end)?;
                    Resolved::Insert(end)
                }
                Err(reason) => {
                    job.warnings.push(format!("`>{n}*` did not resolve to a block ({reason}); inserting after line {n} only"));
                    job.check_seen(n, n)?;
                    Resolved::Insert(n)
                }
            }
        }
        Anchor::Tail => {
            if len > 0 {
                job.check_seen(len, len)?;
            }
            Resolved::Insert(len)
        }
    })
}

struct Prepared {
    abs: PathBuf,
    original: super::edit_transaction::Original,
    rel: String,
    rem: bool,
    mv: Option<(PathBuf, String)>,
    changed: bool,
    new_lf: String,
    new_text: String,
    projected: bool,
    carrier_changed: bool,
    seen_after: Vec<(usize, usize)>,
    preview: String,
    diff: String,
    added: usize,
    removed: usize,
    /// Lines a replace removed that its new text does not contain; see [`dropped_report`].
    dropped: String,
    warnings: Vec<String>,
}

/// Most rows the `removed` report lists, and the longest line it shows.
const DROPPED_MAX_ROWS: usize = 12;
const DROPPED_LINE_CAP: usize = 160;

/// Tell the model which existing lines its replacements deleted. `PUT N.=M` replaces N through M
/// inclusive, and an off-by-one silently destroys a neighbouring line; seeing the loss in the very
/// result of the edit lets the model repair it in one step instead of discovering it at the next build.
/// Lines whose text survives in the replacement (moved or re-indented) are not reported.
fn dropped_report(cur: &[String], edits: &[Edit]) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut total = 0;
    for edit in edits.iter().filter(|e| !e.insert) {
        for n in edit.lo..=edit.hi.min(cur.len()) {
            let old = &cur[n - 1];
            if old.trim().is_empty() || edit.lines.iter().any(|new| new.trim() == old.trim()) {
                continue;
            }
            total += 1;
            if rows.len() < DROPPED_MAX_ROWS {
                let (shown, cut) = truncate_chars(old, DROPPED_LINE_CAP);
                rows.push(format!("  {n}: {shown}{}\n", if cut { " …" } else { "" }));
            }
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "removed lines not present in your new text (old numbering; `PUT N.=M` replaces N through M inclusive - if you only meant to add lines, use `PUT >N:`/`PUT <N:`):\n",
    );
    rows.iter().for_each(|row| out.push_str(row));
    if total > rows.len() {
        out.push_str(&format!("  …and {} more\n", total - rows.len()));
    }
    out
}

fn mismatch_error(
    job: &Job,
    store: &mut EditStore,
    blocked: &[(usize, usize)],
    map: &[Option<usize>],
) -> String {
    let all = hunks(map, job.cur.len());
    let touching: Vec<_> = all
        .iter()
        .filter(|&&(bs, be, _, _)| blocked.iter().any(|&(a, b)| bs <= b && a - 1 <= be))
        .copied()
        .collect();
    let chosen: Vec<_> = if touching.is_empty() {
        all.into_iter().take(3).collect()
    } else {
        touching.into_iter().take(3).collect()
    };
    let regions: Vec<(usize, usize)> = chosen.iter().map(|&(_, _, cs, ce)| (cs + 1, ce)).collect();
    let (rows, shown) = preview_rows(&job.cur, &regions, MISMATCH_CONTEXT, MISMATCH_MAX_ROWS);
    let fresh = store.record(&job.abs, &job.cur_lf, &shown);
    let spans: Vec<String> = blocked
        .iter()
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect();
    format!(
        "{rel}#{tag}: the file changed since that snapshot and the edit touches changed lines (snapshot lines {spans}), so it cannot be re-anchored safely.\n\
         Current content near the change (fresh tag):\n[{rel}#{fresh}]\n{rows}Re-issue the edit against [{rel}#{fresh}] using these line numbers.",
        rel = job.rel,
        tag = job.tag,
        spans = spans.join(", "),
    )
}

/// Show all unread anchor ranges together, without allowing the rejected patch to write.
/// Only complete, displayed rows become witnesses; oversized previews require an explicit read.
fn show_unseen(job: &mut Job, store: &mut EditStore, error: String) -> String {
    let ranges = Seen::from_ranges(&std::mem::take(&mut job.unseen)).0;
    if ranges.is_empty() {
        return error;
    }
    let Some(raw_revision) = store.snapshot(&job.abs, &job.tag).map(|s| s.raw_revision) else {
        return error;
    };
    if job.stale {
        return error;
    }
    let (rows, shown) = preview_rows(&job.cur, &ranges, 0, MISMATCH_MAX_ROWS);
    store.record_version(&job.abs, &job.cur_lf, raw_revision, &shown);
    let spans = ranges
        .iter()
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let displayed = Seen::from_ranges(&shown);
    let complete = ranges
        .iter()
        .all(|&(a, b)| displayed.first_unseen(a, b).is_none());
    let next = if complete {
        format!(
            "All unread anchor lines are shown below and now count as read; check them, then re-issue the edit against [{rel}#{tag}].",
            rel = job.rel,
            tag = job.tag
        )
    } else {
        format!(
            "Only complete lines shown below now count as read. Before retrying, read the remaining affected ranges: `{rel}:{spans}` (ranges use snapshot numbering).",
            rel = job.rel
        )
    };
    format!(
        "{rel}#{tag}: lines {spans} have not been shown by a read, so nothing was written. {next}\n[{rel}#{tag}]\n{rows}",
        rel = job.rel,
        tag = job.tag,
    )
}

/// Where to find the current content before retrying: its tag and the range to read. Uses the
/// same content tag that a read of the whole file returns, so the model needs no guessing.
fn reread_hint(job: &Job) -> String {
    let fresh = tag_of(&job.cur_lf);
    let lines = job.cur.len().max(1);
    format!(
        "Current content is [{rel}#{fresh}] ({lines} lines): read `{rel}:1-{lines}` and then retry against [{rel}#{fresh}].",
        rel = job.rel,
    )
}

fn finalize(job: &mut Job, store: &mut EditStore) -> Result<Prepared, String> {
    if job.rem {
        if !job.edits.is_empty() || job.mv.is_some() {
            return Err(format!(
                "{}: REM cannot be combined with other operations",
                job.rel
            ));
        }
        if job.stale {
            return Err(format!(
                "{}#{}: the file changed since that snapshot, so nothing was deleted. {}",
                job.rel,
                job.tag,
                reread_hint(job)
            ));
        }
        if !job.base.is_empty() {
            job.check_seen(1, job.base.len())?;
        }
        return Ok(Prepared {
            abs: job.abs.clone(),
            original: job.original.clone(),
            rel: job.rel.clone(),
            rem: true,
            mv: None,
            changed: true,
            new_lf: String::new(),
            new_text: String::new(),
            projected: job.raw_revision.is_some(),
            carrier_changed: false,
            seen_after: Vec::new(),
            preview: String::new(),
            diff: super::diff::preview(&job.cur_lf, ""),
            added: 0,
            removed: job.cur.len(),
            dropped: String::new(),
            warnings: std::mem::take(&mut job.warnings),
        });
    }
    let mut edits = std::mem::take(&mut job.edits);
    let cur_seen: Vec<bool>;
    if job.stale {
        let map = line_map(&job.base, &job.cur).ok_or_else(|| {
            format!(
                "{}#{}: the file changed since that snapshot and is too large or too different to re-anchor, so nothing was written. {}",
                job.rel, job.tag, reread_hint(job)
            )
        })?;
        if let Err(blocked) = translate(&mut edits, &map, job.cur.len()) {
            return Err(mismatch_error(job, store, &blocked, &map));
        }
        let base_flags = job.seen.flags(job.base.len());
        let mut flags = vec![false; job.cur.len()];
        for (bi, slot) in map.iter().enumerate() {
            if let Some(ci) = *slot {
                flags[ci] = base_flags[bi];
            }
        }
        cur_seen = flags;
        job.warnings.push(format!(
            "snapshot #{} was stale; the file changed elsewhere ({} changed region(s)) and the edit was re-anchored onto the current content",
            job.tag,
            hunks(&map, job.cur.len()).len()
        ));
    } else {
        cur_seen = job.seen.flags(job.cur.len());
    }
    check_overlap(&edits).map_err(|e| format!("{}#{}: {e}", job.rel, job.tag))?;
    let applied = apply_edits(&job.cur, &cur_seen, &edits);
    let trailing = job.cur_lf.ends_with('\n') || job.cur_lf.is_empty();
    let new_lf = join_lines(&applied.lines, trailing);
    let changed = new_lf != job.cur_lf;
    if changed && job.mixed && job.raw_revision.is_none() {
        job.warnings.push(format!(
            "{} had mixed line endings; unchanged lines kept their own endings and new lines use {}",
            job.rel,
            if job.crlf { "CRLF" } else { "LF" }
        ));
    }
    let desired = new_lf;
    if job.raw_revision.is_some() && job.enforce_seen {
        super::edit_source::structural_witness(
            &job.cur_lf,
            &cur_seen,
            &desired,
            &applied.seen,
            &job.abs,
        )?;
    }
    let persisted = if !changed {
        super::edit_source::Persisted {
            raw: job.original.text.clone(),
            projection: None,
        }
    } else {
        job.carrier
            .take()
            .ok_or("source carrier was already consumed")?
            .persist(None, &job.original.text, desired.clone(), &job.abs)?
    };
    let new_lf = persisted
        .projection
        .map_or(desired, |projection| projection.after);
    let new_text = persisted.raw;
    let canonical_lines = split_lines(&new_lf);
    let (preview, shown) = preview_rows(
        &canonical_lines,
        &applied.regions,
        PREVIEW_CONTEXT,
        PREVIEW_MAX_ROWS,
    );
    let mut seen = Seen::from_ranges(&ranges_from_flags(&applied.seen));
    for (a, b) in shown {
        seen.add(a, b);
    }
    let carrier_changed = job.mv.as_ref().is_some_and(|(dest, _)| {
        super::edit_source::notebook_path(&job.abs) != super::edit_source::notebook_path(dest)
    });
    Ok(Prepared {
        abs: job.abs.clone(),
        original: job.original.clone(),
        rel: job.rel.clone(),
        rem: false,
        mv: job.mv.take(),
        changed,
        diff: super::diff::preview(&job.cur_lf, &new_lf),
        new_lf,
        new_text,
        projected: job.raw_revision.is_some(),
        carrier_changed,
        seen_after: seen.0,
        preview,
        added: applied.added,
        removed: applied.removed,
        dropped: dropped_report(&job.cur, &edits),
        warnings: std::mem::take(&mut job.warnings),
    })
}

fn hash_of(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn targets(input: &str) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    for section in parse_patch(input)? {
        paths.push(section.path);
        if let Some(dest) = section.mv {
            paths.push(dest);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
/// Apply a hashline patch. `Ok` text is the tool result; `Err` is an error the
/// model can act on. Nothing is written unless the whole patch validated.
pub(crate) fn edit(ws: &Workspace, input: &str) -> Result<String, String> {
    edit_with(ws, input, true, &super::editing::Execution::default())
}
pub(crate) fn edit_with(
    ws: &Workspace,
    input: &str,
    commit: bool,
    execution: &super::editing::Execution<'_>,
) -> Result<String, String> {
    ws.edit_settings.validate()?;
    let sections = parse_patch(input)?;
    let multi = sections.len() > 1;
    let mut block_anchors: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    let mut resolved = Vec::with_capacity(sections.len());
    for sec in &sections {
        let abs = ws
            .resolve(&sec.path)
            .map_err(|e| format!("line {}: {e}", sec.line_no))?;
        for op in &sec.ops {
            let anchor = match &op.kind {
                OpKind::Put { anchor, .. } | OpKind::Cut { anchor, .. } => anchor,
            };
            if let Anchor::Block(line) | Anchor::AfterBlock(line) = anchor {
                block_anchors.entry(abs.clone()).or_default().push(*line);
            }
        }
        resolved.push((abs, sec));
    }
    let mut store = ws.store();
    let mut regs = Registers {
        named: store.named.clone(),
        ..Registers::default()
    };
    let mut jobs: Vec<Job> = Vec::new();
    let mut section_jobs = Vec::with_capacity(resolved.len());
    for (abs, sec) in &resolved {
        let ji = match jobs.iter().position(|j| j.abs == *abs) {
            Some(i) => {
                if jobs[i].tag != sec.tag {
                    return Err(format!(
                        "{}: sections for the same file must use the same tag (#{} and #{})",
                        jobs[i].rel, jobs[i].tag, sec.tag
                    ));
                }
                i
            }
            None => {
                let anchors = block_anchors.get(abs).map_or(&[][..], Vec::as_slice);
                jobs.push(load_job(ws, &store, sec, abs.clone(), anchors)?);
                jobs.len() - 1
            }
        };
        section_jobs.push(ji);
    }

    // Validate every anchor before resolving registers or staging edits. Otherwise each retry
    // reveals just the next unread range (and a CUT can hide later blockers behind a register).
    let mut failures: Vec<Vec<String>> = vec![Vec::new(); jobs.len()];
    let mut unseen_failures: Vec<Option<String>> = vec![None; jobs.len()];
    for ((_, sec), &ji) in resolved.iter().zip(&section_jobs) {
        let job = &mut jobs[ji];
        for op in &sec.ops {
            let anchor = match &op.kind {
                OpKind::Put { anchor, .. } | OpKind::Cut { anchor, .. } => anchor,
            };
            let unseen_before = job.unseen.len();
            if let Err(error) = resolve_anchor(job, anchor) {
                let error = format!("line {}: {error}", op.line_no);
                if job.unseen.len() > unseen_before {
                    unseen_failures[ji].get_or_insert(error);
                } else {
                    failures[ji].push(error);
                }
            }
        }
        job.warnings.clear(); // Actual execution emits fallback warnings once.
    }
    if failures.iter().any(|errors| !errors.is_empty())
        || unseen_failures.iter().any(Option::is_some)
    {
        let mut errors = Vec::new();
        for ((job, failures), unseen_failure) in jobs.iter_mut().zip(failures).zip(unseen_failures)
        {
            errors.extend(failures);
            if let Some(error) = unseen_failure {
                errors.push(show_unseen(job, &mut store, error));
            }
        }
        return Err(errors.join("\n\n"));
    }

    for ((_, sec), &ji) in resolved.iter().zip(&section_jobs) {
        if sec.rem {
            jobs[ji].rem = true;
        }
        if let Some(dest) = &sec.mv {
            let dest_abs = ws.resolve(dest).map_err(|e| format!("MV {dest}: {e}"))?;
            if dest_abs == jobs[ji].abs {
                return Err(format!("MV {dest}: destination is the source file"));
            }
            if dest_abs == ws.root() || dest_abs.is_dir() {
                return Err(format!("MV {dest}: destination is a directory"));
            }
            if jobs[ji].mv.is_some() {
                return Err(format!("{}: only one MV per file", jobs[ji].rel));
            }
            let dest_rel = super::edit_source::display_path(ws.rel(&dest_abs));
            jobs[ji].mv = Some((dest_abs, dest_rel));
        }
        for op in &sec.ops {
            let job = &mut jobs[ji];
            let located = |e: String| format!("line {}: {e}", op.line_no);
            match &op.kind {
                OpKind::Put { anchor, src } => {
                    let lines = match src {
                        Src::Body(body) => body.clone(),
                        Src::Register(name) => regs.get(name).map_err(located)?,
                    };
                    if op.bare > 0 {
                        job.warnings.push(format!("line {}: {} body row(s) had no `+` prefix and were taken as content; check the diff, and start every body row with `+` (a lone `+` for a blank line)", op.line_no, op.bare));
                    }
                    match resolve_anchor(job, anchor)
                        .map_err(|e| show_unseen(job, &mut store, e))
                        .map_err(located)?
                    {
                        Resolved::Replace(a, b) => job.edits.push(Edit {
                            lo: a,
                            hi: b,
                            insert: false,
                            lines,
                        }),
                        Resolved::Insert(g) => job.edits.push(Edit {
                            lo: g,
                            hi: g,
                            insert: true,
                            lines,
                        }),
                    }
                }
                OpKind::Cut { anchor, name } => {
                    let Resolved::Replace(a, b) = resolve_anchor(job, anchor)
                        .map_err(|e| show_unseen(job, &mut store, e))
                        .map_err(located)?
                    else {
                        return Err(located("CUT takes N.=M or N*".to_owned()));
                    };
                    let captured: Vec<String> = job.base[a - 1..b].to_vec();
                    match name {
                        Some(n) => {
                            regs.named.insert(n.clone(), captured.clone());
                            regs.pending.push((n.clone(), captured, ji));
                        }
                        None => regs.anon = Some(captured),
                    }
                    job.edits.push(Edit {
                        lo: a,
                        hi: b,
                        insert: false,
                        lines: Vec::new(),
                    });
                }
            }
        }
    }

    let mut prepared = Vec::with_capacity(jobs.len());
    for job in jobs.iter_mut() {
        prepared.push(finalize(job, &mut store)?);
    }

    // No-op handling.
    let noops: Vec<&Prepared> = prepared
        .iter()
        .filter(|p| !p.changed && p.mv.is_none())
        .collect();
    if !noops.is_empty() {
        execution.mark_no_change();
    }
    if multi && !noops.is_empty() {
        return Err(format!(
            "{} would not change anything; a no-change section rejects the whole batch (nothing was written)",
            noops
                .iter()
                .map(|p| p.rel.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !multi && !noops.is_empty() {
        if !commit {
            return Ok(format!(
                "Preview only; no changes would be made to {}",
                noops[0].rel
            ));
        }
        let key = hash_of(input);
        let count = match store.noop {
            Some((k, n)) if k == key => n + 1,
            _ => 1,
        };
        store.noop = Some((key, count));
        let rel = &noops[0].rel;
        if count >= 3 {
            return Err(format!(
                "{rel}: this identical edit has now produced no change {count} times in a row; the file already has the requested content. Re-read it and move on or change approach"
            ));
        }
        return Ok(format!(
            "No changes: the edit leaves {rel} identical. Nothing was written."
        ));
    }

    // Destination / size checks before any mutation.
    for p in &prepared {
        if p.new_text.len() as u64 > MAX_FILE {
            return Err(format!(
                "{}: the edited file would exceed {MAX_FILE} bytes",
                p.rel
            ));
        }
    }
    let mut targets: Vec<&Path> = Vec::new();
    for p in &prepared {
        let target: &Path = p.mv.as_ref().map_or(p.abs.as_path(), |(d, _)| d.as_path());
        if targets.contains(&target)
            || (p.mv.is_some()
                && prepared
                    .iter()
                    .any(|o| o.abs == target && !std::ptr::eq(o, p)))
        {
            return Err(format!(
                "{}: two operations in this patch write the same file",
                ws.rel(target)
            ));
        }
        targets.push(target);
    }
    drop(store); // Never hold edit state while awaiting current-owner model repair/approval.
    let _lease = if commit {
        Some(super::document_transaction::Lease::acquire(
            prepared.iter().flat_map(|proposal| {
                std::iter::once(proposal.abs.clone())
                    .chain(proposal.mv.as_ref().map(|(path, _)| path.clone()))
            }),
        )?)
    } else {
        None
    };
    let mut warnings = Vec::new();
    let mut records = Vec::new();
    let mut changes = Vec::new();
    for p in &mut prepared {
        if commit && !p.rem && p.mv.is_none() && !p.projected && !p.carrier_changed {
            let mut proposal = super::edit_transaction::Change::existing(
                p.abs.clone(),
                p.original.clone(),
                Some(p.new_text.clone()),
            );
            let notes =
                super::document_transaction::rebase(ws, std::slice::from_mut(&mut proposal))?;
            if !notes.is_empty() {
                let after = proposal.after.take().expect("text proposal");
                let flags = Seen::from_ranges(&p.seen_after).flags(split_lines(&p.new_lf).len());
                let new_lf = normalize_body(&after).into_owned();
                p.seen_after = ranges_from_flags(&transition_flags(&p.new_lf, &new_lf, &flags));
                p.original = proposal.before.take().expect("existing text");
                p.diff = super::diff::preview(&p.original.text, &after);
                p.new_text = after;
                p.new_lf = new_lf;
                p.preview.clear();
                warnings.extend(notes);
            }
        }
        execution.check_cancel()?;
        let target =
            p.mv.as_ref()
                .map_or(p.abs.as_path(), |(path, _)| path.as_path());
        super::editing::generated_guard(
            ws,
            target,
            if p.mv.is_none() {
                Some(&p.original.text)
            } else {
                None
            },
        )?;
        if !p.rem && commit && !p.projected && !p.carrier_changed {
            let proposed_flags =
                Seen::from_ranges(&p.seen_after).flags(split_lines(&p.new_lf).len());
            let carrier = super::edit_source::Source::plain(&p.original.text);
            let source = super::syntax::prepare_edit(
                ws,
                target,
                &carrier.text,
                &p.new_lf,
                Some(&proposed_flags),
                execution.repair,
            )?;
            if source.text != p.new_lf {
                let repaired = if source.text.contains("\r\n") {
                    source.text.replace("\r\n", "\n")
                } else {
                    source.text
                };
                let flags = transition_flags(&p.new_lf, &repaired, &proposed_flags);
                p.diff = super::diff::preview(&carrier.text, &repaired);
                p.new_text = carrier.persist(repaired.clone(), target)?.raw;
                p.new_lf = repaired;
                p.seen_after = ranges_from_flags(&flags);
                p.preview = String::new(); // Original-numbering preview no longer describes a repaired file.
            }
            warnings.extend(source.warnings);
            if let Some(record) = source.blackbox {
                records.push(record);
            }
        }
        if !p.rem && p.mv.is_none() && p.new_text == p.original.text {
            execution.mark_no_change();
            return Err(format!(
                "{} has no change after parse repair; nothing was written",
                p.rel
            ));
        }
        if p.rem {
            changes.push(super::edit_transaction::Change::existing(
                p.abs.clone(),
                p.original.clone(),
                None,
            ));
        } else if let Some((dest, _)) = &p.mv {
            let mut destination =
                super::edit_transaction::Change::load(ws, dest.clone(), Some(p.new_text.clone()))?;
            if destination.before.is_some() {
                return Err(format!(
                    "{} already exists; MV never clobbers a destination",
                    ws.rel(dest)
                ));
            }
            super::edit_transaction::inherit_permissions(&mut destination, &p.original);
            changes.push(destination);
            changes.push(super::edit_transaction::Change::existing(
                p.abs.clone(),
                p.original.clone(),
                None,
            ));
        } else {
            changes.push(super::edit_transaction::Change::existing(
                p.abs.clone(),
                p.original.clone(),
                Some(p.new_text.clone()),
            ));
        }
    }
    if commit {
        execution.authorize(
            &changes
                .iter()
                .map(|change| change.path.clone())
                .collect::<Vec<_>>(),
        )?;
        warnings.extend(super::edit_transaction::commit(ws, &changes)?);
        let mut store = ws.store();
        for p in &prepared {
            if p.rem {
                store.forget(&p.abs);
            } else {
                let target = if let Some((dest, _)) = &p.mv {
                    store.forget(&p.abs);
                    dest
                } else {
                    &p.abs
                };
                if p.carrier_changed {
                    store.forget(target);
                } else {
                    store.record_version(
                        target,
                        &p.new_lf,
                        if p.projected {
                            Some(super::edit_source::revision(&p.new_text))
                        } else {
                            None
                        },
                        &p.seen_after,
                    );
                }
            }
        }
        for (name, lines, _) in regs.pending {
            store.named.insert(name, lines);
        }
        store.noop = None;
        drop(store);
        for record in records {
            if let Err(error) = super::syntax::record_blackbox(ws, &record) {
                warnings.push(format!(
                    "Edit committed, but parse-regression recording failed: {error}"
                ));
            }
        }
    }
    let mut out = prepared.iter().map(describe).collect::<Vec<_>>().join("\n");
    if !commit {
        out.insert_str(0, "Preview only; no files modified.\n");
    }
    for p in &prepared {
        warnings.extend(p.warnings.iter().map(|w| format!("{}: {w}", p.rel)));
    }
    if !warnings.is_empty() {
        out.push_str("\nWarnings:\n");
        for warning in warnings {
            out.push_str(&format!("- {warning}\n"));
        }
    }
    Ok(out)
}

fn describe(p: &Prepared) -> String {
    if p.rem {
        return format!("deleted {}{}", p.rel, p.diff);
    }
    let target_rel = p.mv.as_ref().map_or(p.rel.as_str(), |(_, r)| r.as_str());
    let mut out = if p.carrier_changed {
        format!(
            "moved raw bytes to {target_rel}; carrier extension changed, read the destination before further edits.\n"
        )
    } else {
        format!(
            "[{target_rel}#{}]\n[Source raw SHA256:{}]\n{}",
            tag_of(&p.new_lf),
            super::edit_source::revision_hex(&p.new_text),
            if p.projected {
                "[Notebook cell-source projection]\n"
            } else {
                ""
            }
        )
    };
    match &p.mv {
        Some(_) => out.push_str(&format!(
            "moved from {} (+{} -{} lines)\n",
            p.rel, p.added, p.removed
        )),
        None => out.push_str(&format!("updated (+{} -{} lines)\n", p.added, p.removed)),
    }
    out.push_str(&p.dropped);
    out.push_str(&p.preview);
    out.push_str(&p.diff);
    out
}

// ------------------------------------------------------------------ read

/// Hashline-mode `read`; `path_text` and `sel` come from `select::split_target`.
/// Only the rows returned are recorded as seen, so edits anchored on any
/// other line are refused until that line has been read.
pub(crate) fn read(
    ws: &Workspace,
    path_text: &str,
    sel: &Selector,
    offset: usize,
    limit: usize,
    budget: usize,
) -> Result<String, String> {
    let abs = ws.resolve(path_text)?;
    let meta = fs::metadata(&abs).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("reading {path_text:?}: no such file or directory"),
        std::io::ErrorKind::PermissionDenied => format!("reading {path_text:?}: permission denied"),
        _ => format!("reading {path_text:?}: {e}"),
    })?;
    if meta.is_dir() {
        return crate::tools::fs::files::list_dir(ws, &abs);
    }
    if sel.raw {
        return crate::tools::fs::files::read_plain(ws, path_text, sel, offset, limit, budget);
    }
    let rel = super::edit_source::display_path(ws.rel(&abs));
    if meta.len() > MAX_FILE {
        return if sel.raw {
            crate::tools::fs::files::read_plain(ws, path_text, sel, offset, limit, budget)
        } else {
            Err(format!(
                "{rel} is {} bytes, over the {MAX_FILE}-byte limit for anchored reads/edits; page it with `{rel}:raw:A-B` or use bash",
                meta.len()
            ))
        };
    }
    let raw_text = super::edit_transaction::read(ws, &abs)?
        .ok_or("read target disappeared")?
        .text;
    let source = if sel.raw {
        super::edit_source::Source::plain(&raw_text)
    } else {
        super::edit_source::Source::load(&abs, Some(&raw_text))?
    };
    let lf = &source.text;
    let lines = split_lines(lf);
    let len = lines.len();
    let spans = plan_spans(sel, offset, limit, len);

    let row_limit = budget.saturating_sub(READ_FOOTER_RESERVE);
    let mut out = String::new();
    let mut shown: Vec<(usize, usize)> = Vec::new();
    let mut witnessed = Vec::new();
    let mut stopped_at = None;
    for span in &spans {
        let mut last = None;
        for n in span.start..=span.end {
            let (text, cut) = truncate_chars(lines[n - 1], MAX_READ_LINE);
            let suffix = if cut { " …[line truncated]" } else { "" };
            let row = if sel.raw {
                format!("{text}{suffix}\n")
            } else {
                format!("{n}:{text}{suffix}\n")
            };
            if out.len() + row.len() > row_limit {
                stopped_at = Some(n);
                break;
            }
            if last.is_none() && !shown.is_empty() && !sel.raw {
                out.push_str("…\n");
            }
            out.push_str(&row);
            if !cut {
                witnessed.push((n, n));
            }
            last = Some(n);
        }
        if let Some(end) = last {
            shown.push((span.start, end));
        }
        if stopped_at.is_some() {
            break;
        }
    }

    let next = stopped_at.or_else(|| {
        spans
            .last()
            .filter(|s| s.bounded && s.end < len)
            .map(|s| s.end + 1)
    });
    let footer = select::footer(&rel, sel, &shown, len, next, stopped_at.is_some());
    if sel.raw {
        return Ok(if shown.is_empty() {
            empty_read(len)
        } else {
            out + &footer
        });
    }
    let tag = ws
        .store()
        .record_version(&abs, lf, source.raw_revision, &witnessed);
    let mut text = format!(
        "[{rel}#{tag}]\n[Source raw SHA256:{}]\n{}",
        super::edit_source::hex_revision(
            source
                .raw_revision
                .unwrap_or_else(|| super::edit_source::revision(&raw_text))
        ),
        if source.raw_revision.is_some() {
            "[Notebook cell-source projection]\n"
        } else {
            ""
        }
    );
    if shown.is_empty() {
        text.push_str(&empty_read(len));
        text.push('\n');
    }
    text.push_str(&out);
    text.push_str(&footer);
    Ok(text)
}

/// Restore authentic returned rows, not inferred content from a four-digit tag alone.
/// Every notebook witness additionally binds its own result's raw JSON SHA256.
type SeenFile = (bool, Vec<(usize, String)>, Option<[u8; 32]>, bool, bool);
pub fn rehydrate<'a>(
    ws: &Workspace,
    results: impl IntoIterator<Item = (&'a str, &'a Value, &'a str)>,
) {
    fn header(ws: &Workspace, requested: &[PathBuf], line: &str) -> Option<(PathBuf, String)> {
        let (rel, tag) = line
            .strip_prefix('[')?
            .strip_suffix(']')?
            .rsplit_once('#')?;
        if rel.is_empty() || tag.len() != 4 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let raw = ws.resolve(rel).ok().filter(|path| requested.contains(path));
        let decoded = serde_json::from_str::<String>(rel)
            .ok()
            .and_then(|path| ws.resolve(&path).ok())
            .filter(|path| requested.contains(path));
        let path = match (raw, decoded) {
            (Some(raw), Some(decoded)) if raw != decoded => return None,
            (Some(path), _) | (_, Some(path)) => path,
            _ => return None,
        };
        Some((path, tag.to_ascii_uppercase()))
    }
    let mut found: Vec<((PathBuf, String), SeenFile)> = Vec::new();
    let mut witness_bytes = 0;
    for (tool, args, text) in results {
        let requested = match tool {
            "read" => {
                let Some(input) = args["path"].as_str() else {
                    continue;
                };
                let Ok((path, selector)) = select::split_target(ws, input) else {
                    continue;
                };
                if selector.raw {
                    continue;
                }
                let Ok(physical) = ws.resolve(path) else {
                    continue;
                };
                if !physical.is_file()
                    || crate::llm::images::is_image(&physical)
                    || (crate::tools::fs::documents::kind(path).is_some()
                        && !super::edit_source::notebook_path(Path::new(path)))
                {
                    continue;
                }
                vec![path.to_owned()]
            }
            "write" => {
                let Some(path) = args["path"].as_str() else {
                    continue;
                };
                vec![path.to_owned()]
            }
            "edit" => {
                let Ok(targets) = crate::tools::declared_edit_targets(args) else {
                    continue;
                };
                targets
            }
            _ => continue,
        };
        let requested = requested
            .iter()
            .map(|path| ws.resolve(path))
            .collect::<Result<Vec<_>, _>>();
        let Ok(mut requested) = requested else {
            continue;
        };
        requested.sort();
        requested.dedup();
        let mut current = None;
        if tool == "read"
            && !text
                .lines()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| header(ws, &requested, line).is_some())
        {
            continue;
        }
        for line in text.lines() {
            if let Some(key) = header(ws, &requested, line) {
                if found.len() < 512 {
                    found.push((
                        key,
                        (tool == "write", Vec::new(), None, false, tool == "edit"),
                    ));
                    current = Some(found.len() - 1);
                } else {
                    current = None;
                }
                continue;
            }
            let Some(entry) = current
                .and_then(|index| found.get_mut(index))
                .map(|(_, entry)| entry)
            else {
                continue;
            };
            if let Some(hex) = line
                .strip_prefix("[Source raw SHA256:")
                .and_then(|s| s.strip_suffix(']'))
            {
                entry.2 = super::edit_source::parse_revision(hex);
                continue;
            }
            if line == "[Notebook cell-source projection]" {
                entry.3 = true;
                continue;
            }
            if let Some((number, body)) = line.split_once(':')
                && number.bytes().all(|b| b.is_ascii_digit())
                && let Ok(number) = number.parse::<usize>()
                && entry.1.len() < 32_768
                && body.len() <= 32 * 1024 * 1024usize - witness_bytes
            {
                witness_bytes += body.len();
                entry.1.push((number, body.to_owned()));
            }
        }
    }
    let mut store = ws.store();
    for ((abs, tag), (whole, rows, raw_revision, projected, edit_result)) in found {
        // Legacy edit headers had no physical request/path-encoding version proof.
        if edit_result && raw_revision.is_none() {
            continue;
        }
        let Ok(Some(raw)) = super::edit_transaction::read(ws, &abs) else {
            continue;
        };
        if raw_revision.is_some_and(|digest| digest != super::edit_source::revision(&raw.text)) {
            continue;
        }
        if projected && raw_revision.is_none() {
            continue;
        }
        let source = if projected {
            let Ok(source) = super::edit_source::Source::load(&abs, Some(&raw.text)) else {
                continue;
            };
            source
        } else {
            super::edit_source::Source::plain(&raw.text)
        };
        if tag_of(&source.text) != tag {
            continue;
        }
        let lines = split_lines(&source.text);
        let witnessed = if whole && !projected && raw_revision.is_some() {
            vec![(1, lines.len())]
        } else {
            rows.into_iter()
                .filter_map(|(number, body)| {
                    number
                        .checked_sub(1)
                        .filter(|index| lines.get(*index).is_some_and(|line| *line == body))
                        .map(|_| (number, number))
                })
                .collect()
        };
        if !witnessed.is_empty() {
            store.record_version(&abs, &source.text, source.raw_revision, &witnessed);
        }
    }
}

/// Used by `tools::dispatch` to read the `input` argument.
pub(crate) fn input_arg(args: &Value) -> Result<&str, String> {
    match args.get("input") {
        Some(Value::String(text)) => Ok(text),
        None | Some(Value::Null) => {
            Err("missing required argument \"input\" (the hashline patch text)".to_owned())
        }
        Some(_) => Err("argument \"input\" must be a string".to_owned()),
    }
}

#[cfg(test)]
mod tests;
