//! Opt-in idle session preparation. Configuration never grants tool execution authority.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreparationConfig {
    /// Send one bounded request to the resident local backend before the first user turn.
    pub local_prefix: bool,
    /// Workspace-relative source files whose language servers should be prepared through LSP.
    pub lsp_paths: Vec<String>,
    /// Prepare the current workspace's Rust build with an offline, locked check.
    pub cargo_check: bool,
    /// Maximum duration of each preparation action, including local prefix generation.
    pub timeout_secs: u64,
}

impl Default for PreparationConfig {
    fn default() -> Self {
        Self {
            local_prefix: false,
            lsp_paths: Vec::new(),
            cargo_check: false,
            timeout_secs: 60,
        }
    }
}

impl PreparationConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=300).contains(&self.timeout_secs),
            "preparation.timeout_secs must be 1..300"
        );
        ensure!(
            self.lsp_paths.len() <= 8,
            "preparation.lsp_paths permits at most eight source files"
        );
        for path in &self.lsp_paths {
            ensure!(
                !path.is_empty()
                    && path.len() <= 4096
                    && !path.contains("://")
                    && !path.chars().any(char::is_control)
                    && !std::path::Path::new(path).is_absolute()
                    && std::path::Path::new(path).components().all(|part| matches!(
                        part,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )),
                "preparation.lsp_paths must contain bounded workspace-relative source paths"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
