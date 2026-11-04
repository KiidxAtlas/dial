//! `[weights]` settings.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The local MLX model that weight inspection and edits act on (id or path). Empty uses the
    /// main model, which must then itself be a Dial-owned local MLX model. Setting it lets any
    /// main model, including a hosted frontier model, drive edits to a different local model.
    pub model: String,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.model.trim() == self.model && self.model.len() <= 1024,
            "weights.model must be a model id or path without surrounding whitespace"
        );
        Ok(())
    }
}
