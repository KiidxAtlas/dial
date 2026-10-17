//! Immutable raw-file / editable-source carrier. Only the transaction persists bytes.
use super::{
    Workspace,
    edit_native::{
        notebook,
        text::{restore_line_endings_like, strip_bom},
    },
    hashline,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) fn notebook_path(path: &Path) -> bool {
    notebook::is_notebook_path(path)
}
pub(crate) fn display_path(path: String) -> String {
    if path.chars().any(char::is_control) || path.trim() != path || path.starts_with('"') {
        serde_json::to_string(&path).expect("a path string is serializable")
    } else {
        path
    }
}
pub(crate) fn revision(raw: &str) -> [u8; 32] {
    Sha256::digest(raw.as_bytes()).into()
}
pub(crate) fn hex_revision(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 15) as usize] as char);
    }
    output
}
pub(crate) fn revision_hex(raw: &str) -> String {
    hex_revision(revision(raw))
}
pub(crate) fn parse_revision(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes[index] = ((high << 4) | low) as u8;
    }
    Some(bytes)
}
pub(crate) fn parse_notebook(raw: &str) -> Result<Value, String> {
    super::edit_native::json_carrier::parse(raw)
}
pub(crate) fn encode_notebook_with_origins(
    document: &Value,
    original_raw: &str,
    origins: &[Option<usize>],
) -> Result<String, String> {
    let body = super::edit_native::json_carrier::encode(document, original_raw, Some(origins))?;
    Ok(with_bom(
        body,
        if original_raw.starts_with('\u{feff}') {
            "\u{feff}"
        } else {
            ""
        },
    ))
}
fn with_bom(body: String, bom: &str) -> String {
    if bom.is_empty() {
        body
    } else {
        let mut output = String::with_capacity(bom.len() + body.len());
        output.push_str(bom);
        output.push_str(&body);
        output
    }
}
pub(crate) struct Source<'a> {
    pub text: std::borrow::Cow<'a, str>,
    pub raw_revision: Option<[u8; 32]>,
    document: Option<Value>,
    notebook: bool,
    bom: &'static str,
    raw: &'a str,
}
#[derive(Clone, Debug)]
pub(crate) struct Projection {
    pub before: Option<String>,
    pub after: String,
    pub kind_changed: bool,
}
pub(crate) struct Persisted {
    pub raw: String,
    pub projection: Option<Projection>,
}
impl<'a> Source<'a> {
    pub fn plain(input: &'a str) -> Self {
        let (bom, body) = strip_bom(input);
        let text = if body.contains("\r\n") {
            std::borrow::Cow::Owned(body.replace("\r\n", "\n"))
        } else {
            std::borrow::Cow::Borrowed(body)
        };
        Self {
            text,
            raw_revision: None,
            document: None,
            notebook: false,
            bom,
            raw: input,
        }
    }
    pub fn load(path: &Path, raw: Option<&'a str>) -> Result<Self, String> {
        let notebook = notebook_path(path);
        let input = raw.unwrap_or("");
        let notebook_raw = notebook && raw.is_some();
        let text = if notebook_raw {
            std::borrow::Cow::Owned(
                notebook::notebook_to_editable_text(input, &path.display().to_string())
                    .map_err(|e| e.to_string())?,
            )
        } else {
            let (_, input) = strip_bom(input);
            if input.contains("\r\n") {
                std::borrow::Cow::Owned(input.replace("\r\n", "\n"))
            } else {
                std::borrow::Cow::Borrowed(input)
            }
        };
        let document = if notebook_raw {
            Some(parse_notebook(input)?)
        } else {
            None
        };
        Ok(Self {
            text,
            raw_revision: if notebook && raw.is_some() {
                Some(revision(input))
            } else {
                None
            },
            document,
            notebook,
            bom: strip_bom(input).0,
            raw: input,
        })
    }
    pub fn flags(&self, ws: &Workspace, path: &Path) -> Vec<bool> {
        ws.store()
            .witness_flags_version(path, &self.text, self.raw_revision)
    }
    pub fn check_ranges(
        &self,
        ws: &Workspace,
        path: &Path,
        ranges: &[(usize, usize)],
    ) -> Result<(), String> {
        if !ws.edit_settings.enforce_seen_lines {
            return Ok(());
        }
        ws.store()
            .check_version_ranges(path, &self.text, self.raw_revision, ranges)
    }
    pub fn into_parts(self) -> (String, Option<[u8; 32]>, Carrier) {
        (
            self.text.into_owned(),
            self.raw_revision,
            Carrier {
                document: self.document,
                notebook: self.notebook,
                bom: self.bom,
            },
        )
    }
    pub fn persist_checked(
        self,
        ws: &Workspace,
        after: String,
        path: &Path,
        flags: &[bool],
    ) -> Result<Persisted, String> {
        if self.notebook && self.document.is_some() && ws.edit_settings.enforce_seen_lines {
            structural_witness(&self.text, &self.flags(ws, path), &after, flags, path)?;
        }
        self.persist(after, path)
    }
    pub fn persist(self, after: String, path: &Path) -> Result<Persisted, String> {
        let before = if self.raw_revision.is_some() {
            Some(self.text.into_owned())
        } else {
            None
        };
        Carrier {
            document: self.document,
            notebook: self.notebook,
            bom: self.bom,
        }
        .persist(before, self.raw, after, path)
    }
}
pub(crate) struct Carrier {
    document: Option<Value>,
    notebook: bool,
    bom: &'static str,
}
impl Carrier {
    pub fn persist(
        self,
        before: Option<String>,
        original_raw: &str,
        after: String,
        path: &Path,
    ) -> Result<Persisted, String> {
        if self.notebook {
            let new_notebook = self.document.is_none();
            let original = self
                .document
                .unwrap_or_else(notebook::create_empty_notebook);
            let edited = notebook::apply_notebook_editable_text(
                &original,
                &after,
                &path.display().to_string(),
                new_notebook,
            )
            .map_err(|e| e.to_string())?;
            let after = notebook::notebook_value_to_editable_text(
                &edited.document,
                &path.display().to_string(),
            )
            .map_err(|e| e.to_string())?;
            let body = super::edit_native::json_carrier::encode(
                &edited.document,
                original_raw,
                Some(&edited.origins),
            )?;
            Ok(Persisted {
                raw: with_bom(body, self.bom),
                projection: Some(Projection {
                    before,
                    after,
                    kind_changed: false,
                }),
            })
        } else {
            Ok(Persisted {
                raw: with_bom(
                    restore_line_endings_like(strip_bom(original_raw).1, &after),
                    self.bom,
                ),
                projection: None,
            })
        }
    }
}
fn cell_ranges(text: &str) -> Vec<(usize, usize)> {
    let lines = hashline::split_lines(text);
    let mut starts = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if super::edit_native::notebook::is_cell_marker(line) {
            starts.push(index);
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| (start, starts.get(index + 1).copied().unwrap_or(lines.len())))
        .collect()
}
pub(crate) fn structural_witness(
    before: &str,
    old_flags: &[bool],
    after: &str,
    new_flags: &[bool],
    path: &Path,
) -> Result<(), String> {
    let display = path.display().to_string();
    let original =
        notebook::parse_notebook_editable_text(before, &display).map_err(|e| e.to_string())?;
    let proposed =
        notebook::parse_notebook_editable_text(after, &display).map_err(|e| e.to_string())?;
    let old_ranges = cell_ranges(before);
    let new_ranges = cell_ranges(after);
    let used: std::collections::HashSet<_> =
        proposed.iter().filter_map(|cell| cell.cell_index).collect();
    let complete = |flags: &[bool], range: (usize, usize)| {
        range.0 <= range.1
            && range.1 <= flags.len()
            && flags[range.0..range.1].iter().all(|flag| *flag)
    };
    for (index, range) in old_ranges.iter().enumerate() {
        if !used.contains(&index) && !complete(old_flags, *range) {
            return Err(format!(
                "removing notebook cell {index} requires every original cell-source row to have been read"
            ));
        }
    }
    for (position, cell) in proposed.iter().enumerate() {
        let new_range = *new_ranges
            .get(position)
            .ok_or("notebook cell source ranges do not match parsed cells")?;
        match cell.cell_index {
            None if !complete(new_flags,new_range)=>return Err("new notebook cells cannot inherit unseen source; read it or supply complete authored cell text".into()),
            Some(index) if index!=position&&original.get(index).is_some_and(|old|old.source!=cell.source)&&(!old_ranges.get(index).is_some_and(|range|complete(old_flags,*range))||!complete(new_flags,new_range))=>{
                return Err(format!("retargeting notebook cell marker {index} with different source requires full original and proposed cell-source witnesses"));
            },
            _=>{},
        }
    }
    Ok(())
}
/// A cell result is evidence only for its actual original index/type and complete returned rows.
/// Coverage is zero-based, half-open Unicode character offsets in the real cell source.
pub(crate) fn record_cell(
    ws: &Workspace,
    path: &Path,
    raw: &str,
    index: usize,
    coverage: &[(usize, usize)],
) -> Result<(), String> {
    if raw.len() > super::edit_transaction::MAX_FILE {
        return Err("notebook exceeds native file-edit witness budget (4 MiB)".into());
    }
    if ws.resolve(&ws.rel(path))? != path {
        return Err("notebook witness target is not canonical".into());
    }
    let source = Source::load(path, Some(raw))?;
    let document = source
        .document
        .as_ref()
        .ok_or("cell evidence requires a notebook")?;
    let cells = document["cells"]
        .as_array()
        .ok_or("notebook has no cells")?;
    let cell = cells
        .get(index)
        .ok_or("notebook cell index is out of range")?;
    let text = match &cell["source"] {
        Value::String(text) => std::borrow::Cow::Borrowed(text.as_str()),
        Value::Array(lines) => std::borrow::Cow::Owned(
            lines
                .iter()
                .map(|line| line.as_str().ok_or("invalid cell source"))
                .collect::<Result<String, _>>()?,
        ),
        _ => return Err("invalid cell source".into()),
    };
    let length = text.chars().count();
    if coverage
        .iter()
        .any(|&(start, end)| start > end || end > length)
    {
        return Err("cell witness coverage is out of bounds".into());
    }
    let current = super::edit_transaction::read(ws, path)?
        .ok_or("notebook disappeared before witness publication")?;
    if current.text != raw {
        return Err("notebook changed before witness publication; not recorded".into());
    }
    let mut marker_line = 1;
    for previous in cells.iter().take(index) {
        let text = match &previous["source"] {
            Value::String(text) => std::borrow::Cow::Borrowed(text.as_str()),
            Value::Array(lines) => std::borrow::Cow::Owned(
                lines
                    .iter()
                    .map(|line| line.as_str().ok_or("invalid cell source"))
                    .collect::<Result<String, _>>()?,
            ),
            _ => return Err("invalid cell source".into()),
        };
        let normalized = super::edit_native::text::normalize_to_lf(&text);
        marker_line += 1 + if normalized.is_empty() {
            0
        } else {
            1 + normalized.bytes().filter(|byte| *byte == b'\n').count()
        };
    }
    let mut ranges = vec![(marker_line, marker_line)];
    let mut character = 0;
    let mut start = 0;
    let mut row = marker_line + 1;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        character += 1;
        if ch == '\r' || ch == '\n' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
                character += 1;
            }
            if coverage.iter().any(|&(a, b)| a <= start && b >= character) {
                ranges.push((row, row));
            }
            start = character;
            row += 1;
        }
    }
    if start < length && coverage.iter().any(|&(a, b)| a <= start && b >= length) {
        ranges.push((row, row));
    }
    if text.ends_with(['\r', '\n'])
        && coverage.iter().any(|&(a, b)| a == 0 && b == length)
        && index + 1 < cells.len()
    {
        ranges.push((row, row));
    }
    ws.store()
        .record_version(path, &source.text, source.raw_revision, &ranges);
    Ok(())
}
