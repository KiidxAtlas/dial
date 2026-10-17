//! Display options of the diff pane: whitespace handling, line wrapping, the narrow stacked
//! layout and the metadata placeholder shown for binary files.

use super::image;
use super::model::*;
use crate::git_panel::Repo;
use crate::ui::images::{ImageProtocol, Thumbnail};
use crate::ui::text;
use unicode_segmentation::UnicodeSegmentation;

/// Panes narrower than this stack the changes sheet under the diff.
pub(super) const NARROW_COLS: usize = 100;
/// Gutter before a unified line's text: two 4-column line numbers, spaces and the sign.
pub(super) const INLINE_GUTTER: usize = 12;
/// Gutter before a split-view cell's text: a 4-column line number, a space and the sign.
pub(super) const CELL_GUTTER: usize = 7;

/// `b` cycles through these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Whitespace {
    #[default]
    Off,
    IgnoreSpace,
    Formatting,
}

impl Whitespace {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Off => Self::IgnoreSpace,
            Self::IgnoreSpace => Self::Formatting,
            Self::Formatting => Self::Off,
        }
    }

    /// Extra arguments for `git diff`. Git has no equivalent of import-block demotion, so
    /// `formatting` only adds blank-line ignoring on top of whitespace-insensitive matching.
    pub(super) fn git_args(self) -> &'static [&'static str] {
        match self {
            Self::Off => &[],
            Self::IgnoreSpace => &["-w"],
            Self::Formatting => &["-w", "--ignore-blank-lines"],
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Off => "Showing all changes",
            Self::IgnoreSpace => "Ignoring whitespace-only line changes",
            Self::Formatting => "Ignoring formatting and import-only changes",
        }
    }

    /// Header chip; empty when every change is shown.
    pub(super) fn chip(self) -> &'static str {
        match self {
            Self::Off => "",
            Self::IgnoreSpace => " ws",
            Self::Formatting => " fmt",
        }
    }

    /// Hunk patches are only valid against the real diff, so staging is off while whitespace is ignored.
    pub(super) fn allows_patch(self) -> bool {
        self == Self::Off
    }
}

/// Splits the diff width into the left and right cells of a split row, as `split_spans` draws them.
pub(super) fn split_widths(text_w: usize) -> (usize, usize) {
    let left = text_w.saturating_sub(1) / 2;
    (left, text_w.saturating_sub(1 + left))
}

/// Breaks `text` into lines of at most `width` columns, by grapheme. Always yields one line.
pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let w = text::width(grapheme);
        if used + w > width && !line.is_empty() {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push_str(grapheme);
        used += w;
    }
    out.push(line);
    out
}

/// The wrapped pieces of a row, one `(left, right)` pair per visual line. Unified rows only use
/// the left element; split pairs fill both cells. `text_w` is the diff width without the minimap.
pub(super) fn segments(row: &Row, text_w: usize) -> Vec<(String, String)> {
    match &row.kind {
        RowKind::Pair { left, right } => {
            let (left_w, right_w) = split_widths(text_w);
            let left_text = left.as_ref().map_or_else(
                || vec![String::new()],
                |col| wrap_text(&col.text, left_w.saturating_sub(CELL_GUTTER)),
            );
            let right_text = right.as_ref().map_or_else(
                || vec![String::new()],
                |col| wrap_text(&col.text, right_w.saturating_sub(CELL_GUTTER)),
            );
            let count = left_text.len().max(right_text.len());
            (0..count)
                .map(|i| {
                    (
                        left_text.get(i).cloned().unwrap_or_default(),
                        right_text.get(i).cloned().unwrap_or_default(),
                    )
                })
                .collect()
        }
        RowKind::Meta(text) | RowKind::Hunk(text) => wrap_text(text, text_w)
            .into_iter()
            .map(|piece| (piece, String::new()))
            .collect(),
        RowKind::Line { text, .. } => wrap_text(text, text_w.saturating_sub(INLINE_GUTTER))
            .into_iter()
            .map(|piece| (piece, String::new()))
            .collect(),
    }
}

/// A file git reports as binary: no hunks, and a `Binary files ... differ` or untracked marker.
pub(super) fn is_binary_doc(doc: &Doc) -> bool {
    doc.hunks.is_empty()
        && doc
            .head
            .iter()
            .any(|line| line.starts_with("Binary files ") || line == "Binary file (untracked)")
}

/// Loads what the placeholder shows: the byte size, and the type and dimensions when the
/// header is recognised. `rev` is the commit being viewed, if any. Raster images also get
/// decoded previews when the terminal has a graphics `protocol`.
pub(super) fn load_binary(
    repo: &Repo,
    change: &Change,
    rev: Option<&str>,
    protocol: Option<ImageProtocol>,
) -> Result<Binary, String> {
    let (size, head) = repo.binary_head(change, rev)?;
    let (kind, dims) = sniff(&head);
    let previews = image::load_previews(repo, change, rev, protocol);
    let dims = dims.or_else(|| {
        previews
            .as_ref()
            .and_then(|p| p.after.as_ref().or(p.before.as_ref()))
            .map(Thumbnail::dimensions)
    });
    Ok(Binary {
        kind,
        size,
        dims,
        previews,
    })
}

/// Sentence shown in the text placeholder whenever no native preview is drawn in the current view.
pub(super) const NO_PREVIEW: &str = "No preview is shown in this view.";

/// Rows of the placeholder: type, dimensions when known, and size, then the sentence explaining
/// that no native preview is drawn. The native column drops this sentence (see `binary_column`).
pub(super) fn binary_rows(binary: &Binary) -> Vec<Row> {
    let mut lines = vec![binary.kind.to_owned()];
    if let Some((w, h)) = binary.dims {
        lines.push(format!("{w} × {h} px"));
    }
    lines.push(format!("Size: {}", human_size(binary.size)));
    lines.push(String::new());
    lines.push(NO_PREVIEW.to_owned());
    lines
        .into_iter()
        .map(|line| Row {
            hunk: None,
            kind: RowKind::Meta(format!(" {line}")),
        })
        .collect()
}

pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Identifies a file from its leading bytes. Returns a label and the pixel size when it is parseable.
pub(super) fn sniff(bytes: &[u8]) -> (&'static str, Option<(u32, u32)>) {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return ("PNG image", png_dims(bytes));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return ("GIF image", gif_dims(bytes));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return ("JPEG image", jpeg_dims(bytes));
    }
    if bytes.starts_with(b"BM") {
        return ("BMP image", bmp_dims(bytes));
    }
    if bytes.starts_with(b"RIFF") && at::<4>(bytes, 8) == Some(*b"WEBP") {
        return ("WebP image", webp_dims(bytes));
    }
    ("Binary file", None)
}

fn at<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset + N)?.try_into().ok()
}

fn png_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    Some((
        u32::from_be_bytes(at(bytes, 16)?),
        u32::from_be_bytes(at(bytes, 20)?),
    ))
}

fn gif_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    Some((
        u16::from_le_bytes(at(bytes, 6)?).into(),
        u16::from_le_bytes(at(bytes, 8)?).into(),
    ))
}

fn bmp_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    // Height is negative for top-down bitmaps.
    Some((
        i32::from_le_bytes(at(bytes, 18)?).unsigned_abs(),
        i32::from_le_bytes(at(bytes, 22)?).unsigned_abs(),
    ))
}

/// Extended WebP (`VP8X`) only; the simple and lossless headers are not parsed.
fn webp_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if at::<4>(bytes, 12)? != *b"VP8X" {
        return None;
    }
    let w = at::<3>(bytes, 24)?;
    let h = at::<3>(bytes, 27)?;
    Some((
        u32::from_le_bytes([w[0], w[1], w[2], 0]) + 1,
        u32::from_le_bytes([h[0], h[1], h[2], 0]) + 1,
    ))
}

/// Walks the segment markers to the first start-of-frame, which carries the dimensions.
fn jpeg_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if matches!(marker, 0x01 | 0xD0..=0xD8) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes(at(bytes, i + 2)?) as usize;
        // C4 (DHT), C8 (JPG) and CC (DAC) are not frame headers.
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes(at(bytes, i + 5)?);
            let w = u16::from_be_bytes(at(bytes, i + 7)?);
            return Some((w.into(), h.into()));
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests;
