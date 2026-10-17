//! `/changelog` view over the bundled `CHANGELOG.md`.
//!
//! Entries are the `## ` sections of the file, newest first. The default view is the Unreleased
//! section plus the most recent release; `full` shows every entry; `last N` shows the first N.

/// The bundled changelog, compiled into the binary.
pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// Usage line printed for arguments that are not `full` or `last [N]`.
pub const USAGE: &str = "Usage: /changelog [full|last [N]]";

/// One `## ` section of the changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Version without brackets, or `Unreleased`.
    pub title: String,
    /// Release date when the heading carries one (`## [0.1.3] - 2026-10-07`).
    pub date: Option<String>,
    /// Whether this section is a release (not `Unreleased`).
    pub released: bool,
    /// The section as Markdown, heading included.
    pub markdown: String,
}

/// A parsed `/changelog` argument list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Unreleased section plus the most recent release.
    Recent,
    /// Every entry.
    Full,
    /// The first N entries.
    Last(usize),
}

/// Parses the `## ` sections of a changelog, newest first as written. Text before the first
/// section (the file preamble) is ignored.
pub fn parse(content: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<(&str, Vec<&str>)> = None;
    for line in content.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some((heading, body)) = current.take() {
                entries.push(entry(heading, &body));
            }
            current = Some((heading, vec![line]));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some((heading, body)) = current {
        entries.push(entry(heading, &body));
    }
    entries
}

fn entry(heading: &str, lines: &[&str]) -> Entry {
    let heading = heading.trim();
    let (title, rest) = match heading.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((title, rest)) => (title.trim(), rest),
            None => (heading, ""),
        },
        None => (heading, ""),
    };
    let date = rest.trim_start_matches([' ', '-']).trim().to_owned();
    Entry {
        title: title.to_owned(),
        date: (!date.is_empty()).then_some(date),
        released: !title.eq_ignore_ascii_case("unreleased"),
        markdown: lines.join("\n").trim_end().to_owned(),
    }
}

/// Parses `/changelog` arguments. `Err` carries the usage text to print instead of entries.
pub fn parse_view(args: &str) -> Result<View, String> {
    let words: Vec<String> = args
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => Ok(View::Recent),
        ["full"] => Ok(View::Full),
        ["last"] => Ok(View::Last(1)),
        ["last", count] => match count.parse::<usize>() {
            Ok(count) if count >= 1 => Ok(View::Last(count)),
            _ => Err(format!("{USAGE} (N must be a positive integer)")),
        },
        _ => Err(USAGE.to_owned()),
    }
}

/// Entries selected by `view`, newest first.
pub fn select(entries: &[Entry], view: View) -> Vec<&Entry> {
    match view {
        View::Full => entries.iter().collect(),
        View::Last(count) => entries.iter().take(count).collect(),
        View::Recent => {
            let mut out = Vec::new();
            for entry in entries {
                out.push(entry);
                if entry.released {
                    break;
                }
            }
            out
        }
    }
}

/// Markdown for `/changelog <args>` over the bundled changelog.
pub fn render(args: &str) -> String {
    render_from(CHANGELOG, args)
}

fn render_from(content: &str, args: &str) -> String {
    let view = match parse_view(args) {
        Ok(view) => view,
        Err(usage) => return usage,
    };
    let entries = parse(content);
    if entries.is_empty() {
        return "No changelog entries found.".to_owned();
    }
    let selected = select(&entries, view);
    if selected.is_empty() {
        return "No changelog entries match that view.".to_owned();
    }
    selected
        .iter()
        .map(|entry| entry.markdown.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests;
