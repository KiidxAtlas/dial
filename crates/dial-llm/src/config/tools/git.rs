//! `[git]` hook and signing policy.

use serde::{Deserialize, Serialize};

/// `[git]` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    /// Skip repository hooks (`--no-verify`, empty hooksPath) on commit/push in the agent git
    /// tool, the `/git` panel, and `dial commit`. The agent tool also skips commit signing, since it
    /// cannot answer a pinentry prompt; user-initiated commits keep the configured signing.
    pub bypass_hooks: bool,
}
