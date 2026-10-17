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
