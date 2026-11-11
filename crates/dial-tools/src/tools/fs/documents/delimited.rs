//! CSV and TSV files, plain or gzip/zstd/xz/bzip2-compressed, as an aligned table.
//!
//! The first record is the header. `path:rows=O+N` pages data rows (zero-based, default
//! `0+100`) and `path:table` shows the first page. A bare uncompressed `.csv`/`.tsv` read stays
//! the ordinary editable source view; only these selectors (or a compressed file) route here.
//! A `.csv` delimiter is sniffed from the header line among `,`, `;`, tab and `|`.

use super::{archives, table_view};
use std::fmt::Write as _;

#[cfg(test)]
mod tests;

/// Characters kept from one cell value.
const MAX_CELL: usize = 2_000;

/// `"csv"` or `"tsv"` for a delimited file name, with or without a compression suffix.
pub(super) fn format_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let base = super::without_compression(&lower);
    if base.ends_with(".csv") {
        Some("csv")
    } else if base.ends_with(".tsv") || base.ends_with(".tab") {
        Some("tsv")
    } else {
        None
    }
}

pub(super) fn read(format: &str, payload: &[u8], member: Option<&str>) -> Result<String, String> {
    let (offset, count) = table_view::window(member.filter(|m| *m != "table"))?;
    let decoded = archives::decompressed(payload)?;
    let bytes = decoded.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&decoded);
    if bytes.contains(&0) {
        return Err(format!(
            "{} content has NUL bytes; it is not delimited text",
            format.to_ascii_uppercase()
        ));
    }
    let delimiter = if format == "tsv" { b'\t' } else { sniff(bytes) };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);
    let mut header: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    let (mut total, mut columns) = (0usize, 0usize);
    for record in reader.byte_records() {
        let record = record.map_err(|e| format!("invalid {format} data: {e}"))?;
        columns = columns.max(record.len());
        let cells = record.iter().map(cell);
        if header.is_none() {
            header = Some(std::iter::once("#".to_owned()).chain(cells).collect());
            continue;
        }
        if total >= offset && rows.len() < count {
            rows.push(std::iter::once(total.to_string()).chain(cells).collect());
        }
        total += 1;
    }
    let delimiter_name = match delimiter {
        b',' => "comma",
        b';' => "semicolon",
        b'\t' => "tab",
        _ => "pipe",
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}: {total} rows x {columns} columns; {delimiter_name}-delimited; first row is the header",
        format.to_ascii_uppercase()
    );
    let header = header.unwrap_or_else(|| vec!["#".to_owned()]);
    table_view::page(&mut out, &header, &rows, offset, count, total);
    Ok(out)
}

fn cell(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    table_view::cell(&text.chars().take(MAX_CELL).collect::<String>())
}

/// The candidate delimiter occurring most often outside quotes on the first line; `,` on a tie.
fn sniff(bytes: &[u8]) -> u8 {
    let mut counts = [0usize; 4];
    let candidates = *b",;\t|";
    let mut quoted = false;
    for &byte in bytes.iter().take(64 * 1024) {
        match byte {
            b'"' => quoted = !quoted,
            b'\n' if !quoted => break,
            _ if !quoted => {
                if let Some(i) = candidates.iter().position(|c| *c == byte) {
                    counts[i] += 1;
                }
            }
            _ => {}
        }
    }
    let mut best = 0;
    for i in 1..candidates.len() {
        if counts[i] > counts[best] {
            best = i;
        }
    }
    candidates[best]
}
