//! Controller-owned read provenance for worker reports. A receipt is exactly the rows that one of
//! the worker's own successful single-target `read` calls displayed in full, bound to the revision
//! the native output reported. It is parsed only from an admitted anchored presentation, and only
//! when the call's own arguments name the one file whose header the output carries. Everything else
//! is declined (fail closed): multi-target `paths` reads, raw or format reads, unknown options,
//! notebook projections, pointer or log text, and any output whose header does not match the
//! arguments. Worker text never creates or extends a receipt, and rows from the parent, `grep`,
//! `find` or the edit store's older witnesses are never included. A receipt shows exposure to a
//! revision, not comprehension or correctness.
use crate::{llm::ToolCall, runtime::tool_output::ToolOutput, tools::Workspace};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Files retained per worker. Later files are counted by `truncated`, never silently dropped.
const MAX_FILES: usize = 32;
/// Row ranges retained per file. Overflow is marked and can only narrow what is proven.
const MAX_RANGES: usize = 16;
/// Suffix the read tool appends to a row it did not show in full.
const CUT_ROW: &str = " …[line truncated]";
/// Default `limit` of the `read` tool; a window without a `limit` reads this many rows.
const DEFAULT_LIMIT: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::agent) struct Receipt {
    /// Workspace-relative path, as the read output names it and the call named it.
    pub path: String,
    /// SHA-256 hex of the raw file bytes the read hashed and displayed.
    pub revision: String,
    /// 1-based inclusive row ranges displayed in full at `revision`.
    pub ranges: Vec<(usize, usize)>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ranges_overflow: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::agent) struct ReadReceipts {
    pub files: Vec<Receipt>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Parent-side judgement of one cited row span against the receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum SpanStatus {
    /// A receipt covers the span at the parent's current file revision.
    Current,
    /// Displayed to the worker only at another revision; the file has since changed.
    Changed,
    /// No controller receipt covers the span.
    NoReceipt,
    /// A receipt covers the span but the parent cannot read the current revision.
    UnknownRevision,
}

impl SpanStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Changed => "changed",
            Self::NoReceipt => "no_receipt",
            Self::UnknownRevision => "unknown_revision",
        }
    }
}

impl ReadReceipts {
    /// Parses stored receipts. Missing or malformed data (older results) yields no receipts.
    pub fn from_value(value: &Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// Records the rows one successful read call displayed. Anything the admitted grammar does not
    /// cover records nothing.
    pub fn record(&mut self, workspace: &Workspace, call: &ToolCall, output: &ToolOutput) {
        if call.name != "read" || output.is_error {
            return;
        }
        let Some(mut receipt) = displayed_rows(workspace, &call.arguments, &output.text) else {
            return;
        };
        match self
            .files
            .iter()
            .position(|known| known.path == receipt.path)
        {
            Some(index) => {
                let slot = &mut self.files[index];
                if slot.revision == receipt.revision {
                    // Same bytes: rows displayed by earlier reads of this revision stay displayed.
                    receipt.ranges.extend(slot.ranges.iter().copied());
                    receipt.ranges_overflow |= slot.ranges_overflow;
                }
                self.files[index] = normalized(receipt);
            }
            None if self.files.len() < MAX_FILES => self.files.push(normalized(receipt)),
            None => self.truncated = true,
        }
    }

    /// Judges a cited span of workspace-relative `path` against the parent's current revision.
    pub fn span_status(
        &self,
        workspace: &Workspace,
        path: &str,
        span: (usize, usize),
    ) -> SpanStatus {
        let covers = |receipt: &Receipt| {
            receipt.path == path
                && receipt
                    .ranges
                    .iter()
                    .any(|&(start, end)| start <= span.0 && span.1 <= end)
        };
        if !self.files.iter().any(covers) {
            return SpanStatus::NoReceipt;
        }
        let Some(current) = workspace.read_identity(path) else {
            return SpanStatus::UnknownRevision;
        };
        let current = crate::hex(&current);
        if self
            .files
            .iter()
            .any(|receipt| covers(receipt) && receipt.revision == current)
        {
            SpanStatus::Current
        } else {
            SpanStatus::Changed
        }
    }
}

/// The one file a `read` call names and the rows its arguments ask for, as `(relative path,
/// (first row, last row))`. Accepts only a `path` string with optional integer `offset`/`limit`,
/// or a single `:START-END` selector; anything else is declined.
fn target(workspace: &Workspace, arguments: &Value) -> Option<(String, (usize, usize))> {
    let object = arguments.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "path" | "offset" | "limit"))
    {
        return None;
    }
    let input = object.get("path")?.as_str()?;
    if input.is_empty() || input.contains("://") {
        return None;
    }
    let is_file = |candidate: &str| {
        workspace
            .resolve_read(candidate)
            .is_ok_and(|resolved| resolved.is_file())
    };
    let window = |key: &str, default: usize| match object.get(key) {
        None => Some(default),
        Some(value) => value.as_u64().and_then(|n| usize::try_from(n).ok()),
    };
    let (path, rows) = if is_file(input) {
        let offset = window("offset", 1)?;
        let limit = window("limit", DEFAULT_LIMIT)?;
        if offset == 0 || limit == 0 {
            return None;
        }
        (input, (offset, offset.saturating_add(limit - 1)))
    } else {
        // A selector is accepted only alone: offset or limit alongside it is ambiguous.
        if object.contains_key("offset") || object.contains_key("limit") {
            return None;
        }
        let (path, range) = input.rsplit_once(':')?;
        let (start, end) = range.split_once('-')?;
        let (start, end) = (start.parse::<usize>().ok()?, end.parse::<usize>().ok()?);
        if start == 0 || end < start || !is_file(path) {
            return None;
        }
        (path, (start, end))
    };
    let resolved = workspace.resolve_read(path).ok()?;
    Some((workspace.rel(&resolved), rows))
}

/// Rows displayed in full by an admitted native read. The output must open with a `[path#tag]`
/// header naming the call's own target, then `[Source raw SHA256:…]`, then consecutive `N:` rows
/// starting at the requested first row. The body ends at the first line that is not the next row
/// (footers, notes, appended logs or pointers) and at a row the tool cut; nothing after it counts.
fn displayed_rows(workspace: &Workspace, arguments: &Value, text: &str) -> Option<Receipt> {
    let (rel, (first, last)) = target(workspace, arguments)?;
    let mut lines = text.lines();
    let header = lines.next()?.strip_prefix('[')?.strip_suffix(']')?;
    let (path, _tag) = header.rsplit_once('#')?;
    if path != rel {
        return None;
    }
    let revision = lines
        .next()?
        .strip_prefix("[Source raw SHA256:")?
        .strip_suffix(']')?;
    if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut expected = first;
    for line in lines {
        if expected > last {
            break;
        }
        let Some((number, _)) = line.split_once(':') else {
            break;
        };
        if number.parse::<usize>().ok() != Some(expected) || line.ends_with(CUT_ROW) {
            break;
        }
        expected += 1;
    }
    if expected == first {
        return None;
    }
    Some(Receipt {
        path: rel,
        revision: revision.to_ascii_lowercase(),
        ranges: vec![(first, expected - 1)],
        ranges_overflow: false,
    })
}

/// Sorts and merges row ranges, then applies the per-file bound.
fn normalized(mut receipt: Receipt) -> Receipt {
    receipt.ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in receipt.ranges.drain(..) {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    receipt.ranges_overflow |= merged.len() > MAX_RANGES;
    merged.truncate(MAX_RANGES);
    receipt.ranges = merged;
    receipt
}

#[cfg(test)]
mod tests;
