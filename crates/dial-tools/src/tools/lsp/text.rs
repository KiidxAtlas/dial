//! Text and position helpers for the `lsp` tool.
//!
//! The model counts columns in characters (Unicode scalar values, 1-based); LSP counts UTF-16
//! code units (0-based). Every conversion between the two lives here, together with `file://`
//! URI handling and bounded source reading.

#[cfg(test)]
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

/// Largest source file dial will read for line text or document sync.
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;

/// Characters that stay literal in a `file://` path (the same set the `url` crate leaves alone).
fn is_uri_literal(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~/:@!$&'()*+,;=".contains(&byte)
}

pub fn uri_from_path(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = path.as_os_str().as_bytes().to_vec();
    #[cfg(windows)]
    let bytes = path.to_string_lossy().replace('\\', "/").into_bytes();
    let mut uri = String::from("file://");
    #[cfg(windows)]
    if !bytes.starts_with(b"/") {
        uri.push('/');
    }
    for byte in bytes {
        if is_uri_literal(byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

pub use crate::tools::resources::path_from_uri;

/// Read a source file as UTF-8, refusing anything larger than [`MAX_SOURCE_BYTES`].
pub fn read_source(path: &Path) -> io::Result<String> {
    let file = crate::platform::fs::open_nofollow(OpenOptions::new().read(true), path)?;
    let mut text = String::new();
    file.take(MAX_SOURCE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SOURCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is larger than {} MiB",
                MAX_SOURCE_BYTES / (1024 * 1024)
            ),
        ));
    }
    Ok(text)
}

/// Lines as LSP sees them: `\n`, `\r\n`, and bare `\r` all end a line. Always at least one line.
pub fn split_lines(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                start = i + 1;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    lines.push(&text[start..]);
    lines.into_iter().map(str::to_owned).collect()
}

/// UTF-16 offset of the character at 0-based char index `char_index` (clamped to the line end).
pub fn utf16_offset(line: &str, char_index: usize) -> u32 {
    line.chars()
        .take(char_index)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// 0-based char index of the UTF-16 offset `utf16` (clamped to the line end; an offset inside a
/// surrogate pair maps to the character that contains it).
pub fn char_index(line: &str, utf16: u32) -> usize {
    let mut units = 0u32;
    for (index, c) in line.chars().enumerate() {
        let next = units + c.len_utf16() as u32;
        if utf16 < next {
            return index;
        }
        units = next;
    }
    line.chars().count()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Char index of `symbol` in `line`. Whole-word matches win; among equals the first one at or
/// after `hint` wins, else the first.
fn find_symbol(line: &str, symbol: &str, hint: usize) -> Option<usize> {
    let starts_word = symbol.chars().next().is_some_and(is_word);
    let ends_word = symbol.chars().next_back().is_some_and(is_word);
    let mut all = Vec::new();
    let mut whole = Vec::new();
    for (byte, _) in line.match_indices(symbol) {
        let index = line[..byte].chars().count();
        let before_ok = !starts_word || !line[..byte].chars().next_back().is_some_and(is_word);
        let after_ok = !ends_word
            || !line[byte + symbol.len()..]
                .chars()
                .next()
                .is_some_and(is_word);
        all.push(index);
        if before_ok && after_ok {
            whole.push(index);
        }
    }
    let pick = |found: &[usize]| {
        found
            .iter()
            .copied()
            .find(|i| *i >= hint)
            .or_else(|| found.first().copied())
    };
    pick(&whole).or_else(|| pick(&all))
}

/// A resolved cursor position.
#[derive(Debug, PartialEq, Eq)]
pub struct Located {
    /// 0-based line.
    pub line: u32,
    /// 0-based UTF-16 offset.
    pub character: u32,
    /// 1-based character column, for display.
    pub column: usize,
}

/// Turn the model's `line` + (`column` | `symbol`) into an LSP position inside `text`.
pub fn locate(
    text: &str,
    line: u64,
    column: Option<u64>,
    symbol: Option<&str>,
) -> Result<Located, String> {
    let lines = split_lines(text);
    if line == 0 || line > lines.len() as u64 {
        return Err(format!(
            "line {line} is out of range (the file has {} lines; lines start at 1)",
            lines.len()
        ));
    }
    let content = &lines[line as usize - 1];
    let length = content.chars().count();
    let index = match (symbol.filter(|s| !s.is_empty()), column) {
        (Some(symbol), column) => {
            let hint = column.map_or(0, |c| c.saturating_sub(1) as usize);
            find_symbol(content, symbol, hint).ok_or_else(|| {
                format!(
                    "symbol {symbol:?} not found on line {line}: {}",
                    content.trim()
                )
            })?
        }
        (None, Some(0)) => return Err("column starts at 1".into()),
        (None, Some(column)) if column > length as u64 + 1 => {
            return Err(format!(
                "column {column} is past the end of line {line} ({length} characters)"
            ));
        }
        (None, Some(column)) => column as usize - 1,
        (None, None) => {
            return Err(
                "give `column` (1-based) or `symbol` (text on that line) to place the cursor"
                    .into(),
            );
        }
    };
    Ok(Located {
        line: line as u32 - 1,
        character: utf16_offset(content, index),
        column: index + 1,
    })
}

#[cfg(test)]
mod tests;
