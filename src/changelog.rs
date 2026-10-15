//! `/changelog` view over the bundled `CHANGELOG.md`.
//!
//! Entries are the `## ` sections of the file, newest first. The default view is the Unreleased
//! section plus the most recent release; `full` shows every entry; `last N` shows the first N.

/// The bundled changelog, compiled into the binary.
pub const CHANGELOG: &str = include_str!("../CHANGELOG.md");

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
mod tests {
    use super::*;

    const SAMPLE: &str = "# Changelog\n\nintro\n\n## [Unreleased]\n\n### Added\n- new\n\n## [1.2.0] - 2026-01-02\n\n### Fixed\n- a\n\n## [1.1.0] - 2025-12-01\n\n- b\n\n## [1.0.0] - 2025-11-01\n\n- c\n";

    #[test]
    fn parse_reads_sections_newest_first_with_dates() {
        let entries = parse(SAMPLE);
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Unreleased", "1.2.0", "1.1.0", "1.0.0"]);
        assert_eq!(entries[0].date, None);
        assert!(!entries[0].released);
        assert_eq!(entries[1].date.as_deref(), Some("2026-01-02"));
        assert!(entries[1].released);
        assert!(entries[1].markdown.starts_with("## [1.2.0] - 2026-01-02"));
        assert!(!entries[1].markdown.contains("## [1.1.0]"));
        assert!(!entries[0].markdown.contains("intro"));
    }

    #[test]
    fn bundled_changelog_has_entries() {
        let entries = parse(CHANGELOG);
        assert!(!entries.is_empty());
        assert_eq!(entries[0].title, "Unreleased");
        assert!(render("").contains("## [Unreleased]"));
    }

    #[test]
    fn default_view_is_unreleased_plus_latest_release() {
        let out = render_from(SAMPLE, "");
        assert!(out.contains("## [Unreleased]"));
        assert!(out.contains("## [1.2.0]"));
        assert!(!out.contains("## [1.1.0]"));
    }

    #[test]
    fn full_view_shows_everything() {
        let out = render_from(SAMPLE, "full");
        for title in ["[Unreleased]", "[1.2.0]", "[1.1.0]", "[1.0.0]"] {
            assert!(out.contains(title), "missing {title}");
        }
        assert_eq!(render_from(SAMPLE, "  FULL "), out);
    }

    #[test]
    fn last_n_shows_first_n_entries() {
        let two = render_from(SAMPLE, "last 2");
        assert!(two.contains("## [Unreleased]") && two.contains("## [1.2.0]"));
        assert!(!two.contains("## [1.1.0]"));
        let one = render_from(SAMPLE, "last");
        assert!(one.contains("## [Unreleased]") && !one.contains("## [1.2.0]"));
        assert_eq!(render_from(SAMPLE, "last 99"), render_from(SAMPLE, "full"));
    }

    #[test]
    fn invalid_arguments_return_usage() {
        assert_eq!(render_from(SAMPLE, "bogus"), USAGE);
        assert_eq!(
            render_from(SAMPLE, "last 0"),
            format!("{USAGE} (N must be a positive integer)")
        );
        assert_eq!(
            render_from(SAMPLE, "last x"),
            format!("{USAGE} (N must be a positive integer)")
        );
        assert_eq!(render_from(SAMPLE, "last 2 3"), USAGE);
    }

    #[test]
    fn empty_changelog_reports_no_entries() {
        assert_eq!(
            render_from("# Changelog\n\nno sections\n", ""),
            "No changelog entries found."
        );
    }
}
