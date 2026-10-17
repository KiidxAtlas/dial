//! Arguments for the `commit` command (`/commit` slash command and `dial commit`).

/// Arguments accepted by the commit command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitCommandArgs {
    /// Push after committing.
    pub push: bool,
    /// Preview the message without committing.
    pub dry_run: bool,
    /// Skip changelog updates.
    pub no_changelog: bool,
    /// Use the deterministic legacy pipeline instead of the agentic flow.
    pub legacy: bool,
    /// Additional user context for the model.
    pub context: Option<String>,
    /// Model override (role or `provider/model` pattern).
    pub model: Option<String>,
}
