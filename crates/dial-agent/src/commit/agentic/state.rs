//! Mutable state shared by the agentic commit tools for one run.

use std::collections::HashMap;

use crate::commit::commit_types::CommitMessage;

/// Changelog section a user-visible detail belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangelogCategory {
    Added,
    Changed,
    Fixed,
    Deprecated,
    Removed,
    Security,
    BreakingChanges,
}

impl ChangelogCategory {
    /// Parse the exact category spelling the tool schema accepts.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "Added" => Self::Added,
            "Changed" => Self::Changed,
            "Fixed" => Self::Fixed,
            "Deprecated" => Self::Deprecated,
            "Removed" => Self::Removed,
            "Security" => Self::Security,
            "Breaking Changes" => Self::BreakingChanges,
            _ => return None,
        })
    }
}

/// One detail line of a commit body, optionally tagged for the changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConventionalDetail {
    pub(crate) text: String,
    pub(crate) changelog_category: Option<ChangelogCategory>,
    pub(crate) user_visible: Option<bool>,
}

/// Analysis of one proposed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConventionalAnalysis {
    pub(crate) commit_type: String,
    pub(crate) scope: Option<String>,
    pub(crate) details: Vec<ConventionalDetail>,
    pub(crate) issue_refs: Vec<String>,
}

/// File-level changes reported by `git_overview`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GitOverviewSnapshot {
    pub(crate) files: Vec<String>,
    pub(crate) stat: String,
    pub(crate) numstat: Vec<NumstatEntry>,
    pub(crate) scope_candidates: String,
    pub(crate) is_wide_scope: bool,
    pub(crate) untracked_files: Vec<String>,
    pub(crate) excluded_files: Vec<String>,
}

/// One `git diff --numstat` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NumstatEntry {
    pub(crate) path: String,
    /// `None` for binary files.
    pub(crate) additions: Option<u64>,
    pub(crate) deletions: Option<u64>,
}

/// A hunk or line selector for one file in a split commit group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileChange {
    /// Whole file.
    All { path: String },
    /// 1-based hunk indices within the file's staged diff.
    Indices { path: String, indices: Vec<u64> },
    /// 1-based inclusive line range in the staged file.
    Lines { path: String, start: u64, end: u64 },
}

impl FileChange {
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::All { path } | Self::Indices { path, .. } | Self::Lines { path, .. } => path,
        }
    }
}

/// A single validated proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitProposal {
    pub(crate) analysis: ConventionalAnalysis,
    pub(crate) summary: String,
    pub(crate) warnings: Vec<String>,
}

/// One commit of a split plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitCommitGroup {
    pub(crate) changes: Vec<FileChange>,
    pub(crate) commit_type: String,
    pub(crate) scope: Option<String>,
    pub(crate) summary: String,
    pub(crate) details: Vec<ConventionalDetail>,
    pub(crate) issue_refs: Vec<String>,
    pub(crate) rationale: Option<String>,
    /// 0-based indices of commits that must land before this one.
    pub(crate) dependencies: Vec<usize>,
}

/// A validated split plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitCommitPlan {
    pub(crate) commits: Vec<SplitCommitGroup>,
    pub(crate) warnings: Vec<String>,
}

/// Changelog entries proposed for one changelog file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangelogTargetProposal {
    pub(crate) path: String,
    /// Category name → entry lines.
    pub(crate) entries: Vec<(String, Vec<String>)>,
    /// Category name → existing entry lines to remove.
    pub(crate) deletions: Vec<(String, Vec<String>)>,
}

/// All changelog proposals from one agent run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangelogProposal {
    pub(crate) entries: Vec<ChangelogTargetProposal>,
}

/// Mutable per-run state the tools read and write.
#[derive(Debug, Default)]
pub(crate) struct CommitAgentState {
    pub(crate) overview: Option<GitOverviewSnapshot>,
    pub(crate) proposal: Option<CommitProposal>,
    pub(crate) split_proposal: Option<SplitCommitPlan>,
    pub(crate) changelog_proposal: Option<ChangelogProposal>,
    /// `file:staged` → diff text, so repeated `git_file_diff` calls are free.
    pub(crate) diff_cache: HashMap<String, String>,
    pub(crate) diff_text: Option<String>,
}

impl CommitProposal {
    /// Render as a [`CommitMessage`] for the shared conventional-commit formatter.
    pub(crate) fn to_message(&self) -> CommitMessage {
        CommitMessage {
            commit_type: self.analysis.commit_type.clone(),
            scope: self.analysis.scope.clone(),
            summary: self.summary.clone(),
            body: self
                .analysis
                .details
                .iter()
                .map(|detail| detail.text.clone())
                .collect(),
            footers: Vec::new(),
        }
    }
}
