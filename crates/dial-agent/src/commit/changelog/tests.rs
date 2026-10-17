use super::*;

const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- Crash on start\n\n## [1.0.0]\n\n### Added\n- Old\n";

fn proposal(path: &str) -> ChangelogTargetProposal {
    ChangelogTargetProposal {
        path: path.to_owned(),
        entries: vec![("Added".to_owned(), vec!["New thing".to_owned()])],
        deletions: Vec::new(),
    }
}

#[test]
fn dry_run_reports_change_without_writing() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("CHANGELOG.md"), CHANGELOG).expect("write");
    let outcome =
        apply_changelog_proposals(dir.path(), &[proposal("CHANGELOG.md")], true).expect("apply");
    assert_eq!(outcome.changed, vec!["CHANGELOG.md".to_owned()]);
    assert!(outcome.warnings.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("CHANGELOG.md")).expect("read"),
        CHANGELOG
    );
}

#[test]
fn apply_writes_and_reads_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("CHANGELOG.md"), CHANGELOG).expect("write");
    apply_changelog_proposals(dir.path(), &[proposal("CHANGELOG.md")], false).expect("apply");
    let sections = read_unreleased(dir.path(), "CHANGELOG.md").expect("unreleased");
    assert!(sections.contains(&("Added".to_owned(), vec!["New thing".to_owned()])));
    assert!(sections.contains(&("Fixed".to_owned(), vec!["Crash on start".to_owned()])));
}

#[test]
fn missing_file_and_missing_section_are_warnings() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("NOTES.md"), "# Notes\n").expect("write");
    let outcome = apply_changelog_proposals(
        dir.path(),
        &[proposal("CHANGELOG.md"), proposal("NOTES.md")],
        false,
    )
    .expect("apply");
    assert!(outcome.changed.is_empty());
    assert_eq!(
        outcome.warnings,
        vec![
            "Changelog not found: CHANGELOG.md".to_owned(),
            "Unreleased section not found: NOTES.md".to_owned(),
        ]
    );
}
