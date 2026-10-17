//! Plain-text CLI tables. Domain modules own their columns and values.

use unicode_width::UnicodeWidthStr;

/// Render untruncated cells with two spaces between display-width-aligned columns.
/// Fixed-size rows keep headers and values in agreement; the last column is unpadded.
pub(super) fn render<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> String {
    let mut widths = header.map(UnicodeWidthStr::width);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.width());
        }
    }

    let mut out = String::new();
    for row in
        std::iter::once(header).chain(rows.iter().map(|row| row.each_ref().map(String::as_str)))
    {
        for (index, cell) in row.iter().enumerate() {
            out.push_str(cell);
            if index + 1 < N {
                out.extend(std::iter::repeat_n(' ', widths[index] - cell.width() + 2));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;
