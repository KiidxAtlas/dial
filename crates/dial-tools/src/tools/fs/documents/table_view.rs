//! One aligned text layout for tabular rows: CSV/TSV, Parquet/Arrow, spreadsheets and SQLite.
//!
//! Every cell is written in full (callers bound cell length); columns are padded to the widest
//! cell up to [`MAX_PAD`] characters, so one long value never stretches a whole column. Control
//! characters and the `|` separator are escaped, so each row is exactly one line.
use std::fmt::Write as _;

#[cfg(test)]
mod tests;

/// Padding stops at this width; a longer cell is written whole and shifts only its own row.
const MAX_PAD: usize = 32;
pub(super) const DEFAULT_ROWS: usize = 100;
pub(super) const MAX_ROWS: usize = 10_000;

/// A `:rows=O` or `:rows=O+N` selector (zero-based offset, default `0+100`, N at most 10,000).
pub(super) fn window(member: Option<&str>) -> Result<(usize, usize), String> {
    let Some(member) = member else {
        return Ok((0, DEFAULT_ROWS));
    };
    let usage = || format!("row selector must be :rows=O or :rows=O+N (N <= {MAX_ROWS})");
    let spec = member.strip_prefix("rows=").ok_or_else(usage)?;
    let (offset, count) = spec.split_once('+').unwrap_or((spec, "100"));
    let offset = offset.parse().map_err(|_| usage())?;
    let count: usize = count.parse().map_err(|_| usage())?;
    if count == 0 || count > MAX_ROWS {
        return Err(usage());
    }
    Ok((offset, count))
}

/// One line per cell: newlines, tabs and carriage returns become `\n`, `\t`, `\r`; `|` is `\|`.
pub(super) fn cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '|' => out.push_str("\\|"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Render `header` and `rows` (already escaped with [`cell`]) as `a | b` lines under a `-+-`
/// rule. Ragged rows are padded with empty cells; trailing padding is trimmed.
pub(super) fn render(header: &[String], rows: &[Vec<String>]) -> String {
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        for (width, value) in widths.iter_mut().zip(row) {
            *width = (*width).max(value.chars().count().min(MAX_PAD));
        }
    }
    let line = |out: &mut String, row: &[String]| {
        let mut text = String::new();
        for (index, (value, width)) in row.iter().zip(&widths).enumerate() {
            if index > 0 {
                text.push_str(" | ");
            }
            let _ = write!(text, "{value:<width$}");
        }
        out.push_str(text.trim_end());
        out.push('\n');
    };
    let mut out = String::new();
    line(&mut out, header);
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat((*w).max(1))).collect();
    out.push_str(&rule.join("-+-"));
    out.push('\n');
    for row in rows {
        line(&mut out, row);
    }
    out
}

/// `[rows A..B of T]`, the table, then `[more rows: :rows=B+N]` while rows remain.
pub(super) fn page(
    out: &mut String,
    header: &[String],
    rows: &[Vec<String>],
    first: usize,
    count: usize,
    total: usize,
) {
    let end = first + rows.len();
    let _ = writeln!(out, "[rows {first}..{end} of {total}]");
    out.push_str(&render(header, rows));
    if end < total {
        let _ = writeln!(out, "[more rows: :rows={end}+{count}]");
    }
}
