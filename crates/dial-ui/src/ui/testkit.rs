//! Shared helpers for UI tests that assert on rendered rows.
use super::Rows;

/// The plain text of each row: its span contents in order, with all styling dropped.
pub(crate) fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|row| row.spans.iter().map(|span| span.content.as_ref()).collect())
        .collect()
}
