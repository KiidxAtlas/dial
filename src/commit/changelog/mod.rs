//! Changelog maintenance for staged commits.
//!
//! The legacy flow generates entries per target with one model call ([`run_changelog_flow`]). The
//! agentic flow proposes entries through its tool loop and applies them with [`apply_changelog_proposals`].

pub(crate) mod detect;
pub(crate) mod generate;
pub(crate) mod parse;

use std::path::Path;

use crate::commit::agentic::state::ChangelogTargetProposal;
use crate::commit::run::{CommitBackend, CommitError};
use crate::config::types::Config;
use detect::detect_targets;
use parse::{apply_to_content, parse_unreleased};

/// Inputs of the legacy changelog step.
pub(crate) struct ChangelogFlowArgs<'a> {
    pub cwd: &'a Path,
    pub config: &'a Config,
    pub backend: &'a dyn CommitBackend,
    pub staged_files: &'a [String],
    pub dry_run: bool,
}

/// Files the flow wrote, relative to `cwd`, so the caller can stage them into the same commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangelogOutcome {
    pub changed_files: Vec<String>,
}

/// Result of applying changelog proposals to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApplyOutcome {
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
}

/// Read the Unreleased section of the changelog at `path` (relative to `cwd`) as `(section, items)` pairs.
/// `None` when the file is missing or has no Unreleased section.
pub(crate) fn read_unreleased(cwd: &Path, path: &str) -> Option<Vec<(String, Vec<String>)>> {
    let content = std::fs::read_to_string(cwd.join(path)).ok()?;
    parse_unreleased(&content).map(|unreleased| unreleased.sections)
}

/// Apply proposals to their changelog files. Unchanged files are not reported. Dry-run writes nothing.
pub(crate) fn apply_changelog_proposals(
    cwd: &Path,
    proposals: &[ChangelogTargetProposal],
    dry_run: bool,
) -> Result<ApplyOutcome, CommitError> {
    let mut outcome = ApplyOutcome::default();
    for proposal in proposals {
        let file = cwd.join(&proposal.path);
        if !file.is_file() {
            outcome
                .warnings
                .push(format!("Changelog not found: {}", proposal.path));
            continue;
        }
        let content = std::fs::read_to_string(&file).map_err(|error| {
            CommitError::Git(format!(
                "Changelog update failed: {}: {error}",
                proposal.path
            ))
        })?;
        let Some(updated) = apply_to_content(&content, &proposal.entries, &proposal.deletions)
        else {
            outcome
                .warnings
                .push(format!("Unreleased section not found: {}", proposal.path));
            continue;
        };
        if updated == content {
            continue;
        }
        if !dry_run {
            crate::storage::atomic::write_replacing(&file, updated.as_bytes()).map_err(
                |error| {
                    CommitError::Git(format!(
                        "Changelog update failed: {}: {error}",
                        proposal.path
                    ))
                },
            )?;
        }
        outcome.changed.push(proposal.path.clone());
    }
    Ok(outcome)
}

/// Legacy changelog step: generate entries for each target covering the staged files, apply them, and
/// report the files written.
pub(crate) fn run_changelog_flow(
    args: &ChangelogFlowArgs<'_>,
) -> Result<ChangelogOutcome, CommitError> {
    let targets = detect_targets(args.cwd, args.staged_files);
    let mut proposals = Vec::new();
    for target in &targets {
        if let Some(proposal) =
            generate::generate_target(args.cwd, args.config, args.backend, target)?
        {
            proposals.push(proposal);
        }
    }
    let applied = apply_changelog_proposals(args.cwd, &proposals, args.dry_run)?;
    for warning in &applied.warnings {
        eprintln!("Warning: {warning}");
    }
    Ok(ChangelogOutcome {
        changed_files: applied.changed,
    })
}

#[cfg(test)]
mod tests {
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
        let outcome = apply_changelog_proposals(dir.path(), &[proposal("CHANGELOG.md")], true)
            .expect("apply");
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
}
