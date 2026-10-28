//! `[context_files]`: extra instruction file names read beside every project `AGENTS.md`.
use super::*;

/// Names the context walk already reads by name; `context_files.extra` cannot repeat them.
pub const RESERVED_CONTEXT_FILE_NAMES: [&str; 5] = [
    "AGENTS.md",
    "AGENTS.local.md",
    "CLAUDE.md",
    ".cursorrules",
    "GEMINI.md",
];

/// `[context_files]`: extra instruction file names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextFilesConfig {
    /// Extra plain file names (no path separators) read from each walked directory, after the
    /// built-in names. Reserved built-in names are rejected by validation.
    pub extra: Vec<String>,
}

/// Rejects extra names that are empty, contain path separators, are `.`/`..`, or repeat a reserved
/// built-in name.
pub fn validate_context_file_names(names: &[String]) -> Result<(), String> {
    for name in names {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err(format!(
                "context_files.extra entry {name:?} must be a plain file name"
            ));
        }
        if RESERVED_CONTEXT_FILE_NAMES.contains(&name.as_str()) {
            return Err(format!(
                "context_files.extra entry {name:?} is a built-in context file name"
            ));
        }
    }
    Ok(())
}
