//! `@path` imports inside instruction files. An `@token` at the start of a line or after
//! whitespace expands in place to the referenced file, wrapped in an `<import>` block that shows
//! its provenance. Fenced code, inline code spans and any target outside the allowed roots are
//! left as written; refusals are marked so the model can see them.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{FILE_CAP, Imports, TRUNCATION_NOTICE, display_for, read_text, xml_attr};

/// Maximum number of nested import hops below an instruction file.
const MAX_IMPORT_DEPTH: usize = 5;
/// Maximum number of `@` references resolved from one top-level instruction file.
const MAX_IMPORTS: usize = 32;

const TRAILING_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];

/// Expands `@` imports in `text`, the body of `file`.
pub(super) fn expand(text: &str, file: &Path, imports: &Imports) -> String {
    let mut walk = Walk {
        imports,
        chain: fs::canonicalize(file).into_iter().collect(),
        count: 0,
    };
    let base = file.parent().unwrap_or(Path::new("."));
    walk.body(text, base, 0)
}

struct Walk<'a, 'b> {
    imports: &'a Imports<'b>,
    /// Files on the current import path, outermost first; a target already on it is a cycle.
    chain: Vec<PathBuf>,
    count: usize,
}

impl Walk<'_, '_> {
    fn body(&mut self, text: &str, base: &Path, depth: usize) -> String {
        let mut out = String::with_capacity(text.len());
        let mut fence: Option<(u8, usize)> = None;
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                out.push('\n');
            }
            match (fence, fence_marker(line)) {
                (None, Some(open)) => {
                    fence = Some(open);
                    out.push_str(line);
                }
                (Some((ch, len)), Some((close_ch, close_len)))
                    if ch == close_ch && close_len >= len =>
                {
                    fence = None;
                    out.push_str(line);
                }
                (Some(_), _) => out.push_str(line),
                (None, None) => self.line(line, base, depth, &mut out),
            }
        }
        out
    }

    fn line(&mut self, line: &str, base: &Path, depth: usize, out: &mut String) {
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut last = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'`' => {
                    let run = run_len(bytes, i, b'`');
                    i = close_span(bytes, i + run, run).unwrap_or(i + run);
                }
                b'@' if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                    match parse_token(line, i + 1) {
                        Some((end, token)) => match self.resolve(&token, base, depth) {
                            Some(block) => {
                                out.push_str(&line[last..i]);
                                out.push_str(&block);
                                last = end;
                                i = end;
                            }
                            None => i += 1,
                        },
                        None => i += 1,
                    }
                }
                _ => i += 1,
            }
        }
        out.push_str(&line[last..]);
    }

    /// Expansion of one `@token`, or `None` to leave the token as written.
    fn resolve(&mut self, token: &str, base: &Path, depth: usize) -> Option<String> {
        let candidate = match token.strip_prefix("~/") {
            Some(rest) => self.imports.home?.join(rest),
            None if token == "~" => self.imports.home?.to_path_buf(),
            None if Path::new(token).is_absolute() => PathBuf::from(token),
            None => base.join(token),
        };
        if depth >= MAX_IMPORT_DEPTH {
            return Some(refusal(token, "nesting limit reached"));
        }
        if self.count >= MAX_IMPORTS {
            return Some(refusal(token, "import limit reached"));
        }
        self.count += 1;
        let Ok(real) = fs::canonicalize(&candidate) else {
            return None;
        };
        if !self.imports.roots.iter().any(|root| real.starts_with(root)) {
            return Some(refusal(token, "outside the allowed directories"));
        }
        if self.chain.contains(&real) {
            return Some(refusal(token, "import cycle"));
        }
        let (body, cut) = read_text(&real, FILE_CAP)?;
        self.chain.push(real.clone());
        let parent = real.parent().unwrap_or(base).to_path_buf();
        let mut inner = self.body(&body, &parent, depth + 1);
        self.chain.pop();
        if cut {
            inner.push('\n');
            inner.push_str(TRUNCATION_NOTICE);
        }
        let shown = display_for(self.imports.layout, self.imports.home, &real);
        let inner = inner
            .trim_end_matches('\n')
            .replace("</import>", "<\\/import>");
        Some(format!(
            "<import file=\"{}\">\n{inner}\n</import>",
            xml_attr(&shown)
        ))
    }
}

/// Text that stands in for an `@token` that was not imported.
fn refusal(token: &str, reason: &str) -> String {
    format!("[@{token} not imported: {reason}]")
}

/// Opening or closing code fence: a line whose first non-blank run is at least three backticks or
/// tildes, returned as `(marker, run length)`.
fn fence_marker(line: &str) -> Option<(u8, usize)> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let ch = *trimmed.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = run_len(trimmed.as_bytes(), 0, ch);
    (len >= 3).then_some((ch, len))
}

/// Length of the run of `ch` starting at `from`.
fn run_len(bytes: &[u8], from: usize, ch: u8) -> usize {
    bytes[from..].iter().take_while(|&&b| b == ch).count()
}

/// Index just past the backtick run of exactly `len` that closes an inline code span.
fn close_span(bytes: &[u8], from: usize, len: usize) -> Option<usize> {
    let mut j = from;
    while j < bytes.len() {
        if bytes[j] == b'`' {
            let run = run_len(bytes, j, b'`');
            if run == len {
                return Some(j + run);
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// Parses the import token that starts at `start` (just after `@`). Returns the end index in
/// `line` and the token with `\ ` unescaped, when it looks like a path.
fn parse_token(line: &str, start: usize) -> Option<(usize, String)> {
    let bytes = line.as_bytes();
    let first = *bytes.get(start)?;
    let starts_path = first == b'.'
        || first == b'/'
        || first == b'~'
        || first == b'_'
        || first == b'-'
        || first.is_ascii_alphanumeric();
    if !starts_path {
        return None;
    }
    let mut j = start;
    while j < bytes.len() {
        if bytes[j] == b'\\' && bytes.get(j + 1) == Some(&b' ') {
            j += 2;
        } else if bytes[j].is_ascii_whitespace() {
            break;
        } else {
            j += 1;
        }
    }
    let raw = line[start..j].trim_end_matches(|c: char| TRAILING_PUNCT.contains(&c));
    if raw.is_empty() {
        return None;
    }
    let token = raw.replace("\\ ", " ");
    let path_like = token.contains('/') || token.contains('.') || token.starts_with('~');
    path_like.then(|| (start + raw.len(), token))
}
