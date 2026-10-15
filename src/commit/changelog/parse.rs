//! Parsing and rewriting the `## Unreleased` section of a `CHANGELOG.md`.

/// Changelog subsections in the order they are written.
pub(crate) const CATEGORY_ORDER: [&str; 7] = [
    "Breaking Changes",
    "Added",
    "Changed",
    "Deprecated",
    "Removed",
    "Fixed",
    "Security",
];

/// The Unreleased section of a changelog: header line index, end line index (exclusive), and subsections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unreleased {
    pub header_line: usize,
    pub end_line: usize,
    pub sections: Vec<(String, Vec<String>)>,
}

/// `^##\s+\[?Unreleased\]?`, case-insensitive.
fn is_unreleased_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("##") else {
        return false;
    };
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    rest.get(..10)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("unreleased"))
}

/// Entry text of a `-` or `*` bullet line, `None` for other lines or empty bullets.
fn entry_text(line: &str) -> Option<String> {
    let rest = line.strip_prefix('-').or_else(|| line.strip_prefix('*'))?;
    let text = rest.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Locate and parse the Unreleased section; `None` when the changelog has none.
pub(crate) fn parse_unreleased(content: &str) -> Option<Unreleased> {
    let lines: Vec<&str> = content.split('\n').collect();
    let header_line = lines.iter().position(|line| is_unreleased_header(line))?;
    let end_line = lines[header_line + 1..]
        .iter()
        .position(|line| line.starts_with("## "))
        .map_or(lines.len(), |offset| header_line + 1 + offset);
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for line in &lines[header_line + 1..end_line] {
        if let Some(name) = line.strip_prefix("### ") {
            sections.push((name.trim().to_owned(), Vec::new()));
        } else if let (Some(item), Some(section)) = (entry_text(line), sections.last_mut()) {
            section.1.push(item);
        }
    }
    Some(Unreleased {
        header_line,
        end_line,
        sections,
    })
}

/// Trim entries, drop trailing periods and empties, and dedupe case-insensitively (first spelling wins).
pub(crate) fn normalize_items(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let text = item.trim().trim_end_matches('.').trim_end();
        if text.is_empty()
            || out
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(text))
        {
            continue;
        }
        out.push(text.to_owned());
    }
    out
}

/// Remove `deletions`, merge `entries` (case-insensitive dedupe, existing order kept), and rewrite the
/// Unreleased body. Returns `None` when the changelog has no Unreleased section.
pub(crate) fn apply_to_content(
    content: &str,
    entries: &[(String, Vec<String>)],
    deletions: &[(String, Vec<String>)],
) -> Option<String> {
    let parsed = parse_unreleased(content)?;
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for (name, items) in parsed.sections {
        match sections.iter_mut().find(|section| section.0 == name) {
            Some(section) => section.1.extend(items),
            None => sections.push((name, items)),
        }
    }

    for (name, items) in deletions {
        let targets: Vec<String> = items.iter().map(|item| item.trim().to_owned()).collect();
        if let Some(section) = sections.iter_mut().find(|section| section.0 == *name) {
            section.1.retain(|existing| {
                !targets
                    .iter()
                    .any(|target| existing.eq_ignore_ascii_case(target))
            });
        }
    }

    for (name, items) in entries {
        let index = match sections.iter().position(|section| section.0 == *name) {
            Some(index) => index,
            None => {
                sections.push((name.clone(), Vec::new()));
                sections.len() - 1
            }
        };
        let existing = &mut sections[index].1;
        for item in normalize_items(items) {
            if !existing
                .iter()
                .any(|current| current.eq_ignore_ascii_case(&item))
            {
                existing.push(item);
            }
        }
    }

    let mut ordered: Vec<&(String, Vec<String>)> = Vec::new();
    for category in CATEGORY_ORDER {
        if let Some(section) = sections
            .iter()
            .find(|section| section.0 == category && !section.1.is_empty())
        {
            ordered.push(section);
        }
    }
    for section in &sections {
        if !CATEGORY_ORDER.contains(&section.0.as_str()) && !section.1.is_empty() {
            ordered.push(section);
        }
    }

    let mut body: Vec<String> = Vec::new();
    for (index, (name, items)) in ordered
        .iter()
        .map(|section| (&section.0, &section.1))
        .enumerate()
    {
        if index == 0 {
            body.push(String::new());
        }
        body.push(format!("### {name}"));
        body.extend(items.iter().map(|item| format!("- {item}")));
        if index + 1 < ordered.len() {
            body.push(String::new());
        }
    }
    let lines: Vec<&str> = content.split('\n').collect();
    if parsed.end_line < lines.len() || content.ends_with('\n') {
        body.push(String::new());
    }

    let mut out: Vec<String> = lines[..=parsed.header_line]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
    out.extend(body);
    out.extend(
        lines[parsed.end_line..]
            .iter()
            .map(|line| (*line).to_owned()),
    );
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- Crash on start\n\n## [1.0.0]\n\n### Added\n- Old\n";

    #[test]
    fn parses_unreleased_sections_and_bounds() {
        let parsed = parse_unreleased(CHANGELOG).expect("unreleased present");
        assert_eq!(parsed.header_line, 2);
        assert_eq!(parsed.end_line, 7);
        assert_eq!(
            parsed.sections,
            vec![("Fixed".to_owned(), vec!["Crash on start".to_owned()])]
        );
    }

    #[test]
    fn missing_unreleased_returns_none() {
        assert!(parse_unreleased("# Changelog\n\n## [1.0.0]\n").is_none());
    }

    #[test]
    fn merge_dedupes_case_insensitively_and_orders_categories() {
        let entries = vec![
            (
                "Added".to_owned(),
                vec!["New thing.".to_owned(), "new THING".to_owned()],
            ),
            (
                "Breaking Changes".to_owned(),
                vec!["Dropped flag".to_owned()],
            ),
        ];
        let updated = apply_to_content(CHANGELOG, &entries, &[]).expect("applied");
        let expected = "# Changelog\n\n## [Unreleased]\n\n### Breaking Changes\n- Dropped flag\n\n### Added\n- New thing\n\n### Fixed\n- Crash on start\n\n## [1.0.0]\n\n### Added\n- Old\n";
        assert_eq!(updated, expected);
    }

    #[test]
    fn deletions_apply_before_merge() {
        let deletions = vec![("Fixed".to_owned(), vec!["CRASH ON START".to_owned()])];
        let entries = vec![("Fixed".to_owned(), vec!["Fixed parser".to_owned()])];
        let updated = apply_to_content(CHANGELOG, &entries, &deletions).expect("applied");
        assert!(updated.contains("### Fixed\n- Fixed parser\n"));
        assert!(!updated.contains("Crash on start"));
    }

    #[test]
    fn normalize_items_trims_and_dedupes() {
        let items = vec![" A thing. ".to_owned(), "a thing".to_owned(), String::new()];
        assert_eq!(normalize_items(&items), vec!["A thing".to_owned()]);
    }
}
