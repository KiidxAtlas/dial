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
