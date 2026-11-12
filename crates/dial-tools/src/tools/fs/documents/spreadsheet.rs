//! Legacy and OpenDocument spreadsheets (`.xls`, `.xlsb`, `.xlsm`, `.ods`) through calamine:
//! cached cell values per sheet as an aligned table headed by column letters, each row led by
//! its sheet row number. Formulas are not evaluated; `:sheet=NAME` or `:sheet=N` selects one
//! sheet.

use super::table_view;
use calamine::{Reader, open_workbook_auto_from_rs};
use std::{fmt::Write as _, io::Cursor};

#[cfg(test)]
mod tests;

/// Most rows rendered per sheet.
const MAX_ROWS: usize = 100_000;
const MAX_CELL: usize = 2_000;

pub(super) fn read(bytes: &[u8], member: Option<&str>) -> Result<String, String> {
    let mut book = open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
        .map_err(|e| format!("invalid spreadsheet: {e}"))?;
    let names = book.sheet_names();
    let wanted = member
        .map(|m| {
            let key = m
                .strip_prefix("sheet=")
                .ok_or("spreadsheet selector must be :sheet=NAME or :sheet=N (1-based)")?;
            match key.parse::<usize>() {
                Ok(n) if n >= 1 && n <= names.len() => Ok(names[n - 1].clone()),
                _ if names.iter().any(|name| name == key) => Ok(key.to_owned()),
                _ => Err(format!(
                    "sheet {key:?} not found; sheets: {}",
                    names.join(", ")
                )),
            }
        })
        .transpose()?;
    let mut out = format!("Sheets: {}\n", names.join(", "));
    for name in &names {
        if wanted.as_ref().is_some_and(|w| w != name) {
            continue;
        }
        let range = book
            .worksheet_range(name)
            .map_err(|e| format!("reading sheet {name:?}: {e}"))?;
        let (rows, cols) = range.get_size();
        let start = range.start().unwrap_or((0, 0));
        let _ = writeln!(
            out,
            "\n[sheet {name}: {rows} rows x {cols} columns, starting at row {} column {}]",
            start.0 + 1,
            start.1 + 1
        );
        let mut header = vec!["#".to_owned()];
        header.extend((0..cols).map(|i| column_name(start.1 as usize + i)));
        let table: Vec<Vec<String>> = range
            .rows()
            .take(MAX_ROWS)
            .enumerate()
            .map(|(index, row)| {
                let mut cells = vec![(start.0 as usize + index + 1).to_string()];
                cells.extend(row.iter().map(|cell| {
                    table_view::cell(&cell.to_string().chars().take(MAX_CELL).collect::<String>())
                }));
                while cells.last().is_some_and(String::is_empty) {
                    cells.pop();
                }
                cells
            })
            .collect();
        out.push_str(&table_view::render(&header, &table));
        if rows > MAX_ROWS {
            out.push_str("…[more rows omitted]\n");
        }
    }
    Ok(out)
}

/// Spreadsheet column letters for a zero-based index: 0 → `A`, 25 → `Z`, 26 → `AA`.
fn column_name(mut index: usize) -> String {
    let mut name = Vec::new();
    loop {
        name.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_default()
}
