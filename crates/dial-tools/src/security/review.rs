//! The model-driven reviewer seam. `security/**` never reaches the agent runtime directly; the
//! `/security` command supplies an implementation that runs one `reviewer` subagent task.

use anyhow::Result;
use serde_json::Value;

/// Runs one reviewer subagent task and returns its final text output.
pub trait Reviewer {
    /// Runs `prompt` as a reviewer task constrained to `output_schema`.
    fn run_reviewer(&mut self, prompt: &str, output_schema: &Value) -> Result<String>;

    /// Whether the user interrupted the surrounding session (cooperative cancellation).
    fn interrupted(&self) -> bool {
        false
    }
}
