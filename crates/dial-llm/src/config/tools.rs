//! Tool settings declared by configuration: names, defaults and edit settings.

use crate::config::types::EditMode;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod bash_interceptor;
pub mod browser;
pub mod dap;
pub mod eval;
pub mod git;
pub mod ida;
pub mod integrations;
pub mod kernel;
pub mod lsp;
pub mod mcp;
pub mod ssh;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct EditToggle {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditSettings {
    pub mode: EditMode,
    pub model_variants: BTreeMap<String, EditMode>,
    pub fuzzy_match: bool,
    pub fuzzy_threshold: f64,
    pub streaming_abort: bool,
    pub recover_inline_edits: bool,
    pub block_auto_generated: bool,
    pub enforce_seen_lines: bool,
    pub blackbox: EditToggle,
    pub auto_repair: EditToggle,
}

impl Default for EditSettings {
    fn default() -> Self {
        Self {
            mode: EditMode::Hashline,
            model_variants: BTreeMap::new(),
            fuzzy_match: true,
            fuzzy_threshold: 0.95,
            streaming_abort: false,
            recover_inline_edits: true,
            block_auto_generated: true,
            enforce_seen_lines: true,
            blackbox: EditToggle::default(),
            auto_repair: EditToggle::default(),
        }
    }
}

impl EditSettings {
    pub fn validate(&self) -> Result<(), String> {
        if !self.fuzzy_threshold.is_finite() || !(0.0..=1.0).contains(&self.fuzzy_threshold) {
            return Err("edit.fuzzy_threshold must be finite in 0..1".into());
        }
        let mut patterns = BTreeSet::new();
        for pattern in self.model_variants.keys() {
            if pattern.trim().is_empty() || pattern.len() > 1024 {
                return Err(
                    "edit.model_variants selectors must be nonempty and at most 1024 bytes".into(),
                );
            }
            if !patterns.insert(pattern.to_lowercase()) {
                return Err("edit.model_variants has duplicate case-insensitive selectors".into());
            }
        }
        Ok(())
    }
    /// Explicit profile/harness pins override selectors; longest matching selector wins, then lexical.
    pub fn resolve_mode(&self, selector: &str, pinned: Option<EditMode>) -> EditMode {
        pinned
            .or_else(|| self.variant_mode(selector))
            .unwrap_or(self.mode)
    }
    /// The mode of the longest `model_variants` selector contained in `selector` (ties: lexical).
    pub fn variant_mode(&self, selector: &str) -> Option<EditMode> {
        let lower = selector.to_lowercase();
        self.model_variants
            .iter()
            .filter(|(pattern, _)| lower.contains(&pattern.to_lowercase()))
            .min_by(|(a, _), (b, _)| {
                b.len()
                    .cmp(&a.len())
                    .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
            })
            .map(|(_, mode)| *mode)
    }
}

pub const BUILTIN: &[&str] = &["read", "write", "edit", "glob", "grep", "bash"];

/// dial's own tools beyond the file/shell built-ins.
pub const EXTRA_TOOLS: &[&str] = &[
    "todo",
    "finish",
    "task",
    "side_request",
    "solve",
    "escalate",
    "skill",
    "skill_manage",
    "session_search",
    "tool_search",
    "tool_load",
    "artifact_read",
    "working_state",
    "message",
    "recall",
    "peers",
    "retain",
    "memory_update",
    "forget",
    "memory_history",
    "memory_import",
    "context_notes",
    "new_context",
    "checkpoint",
    "rewind",
    "ask",
    "tts",
    "check",
    "revert_edit",
];

/// Tools dial enables by default: the built-ins plus the core dial tools below. The rest of
/// [`EXTRA_TOOLS`] stays opt-in through the harness or tool flags.
pub fn default_tools() -> Vec<String> {
    BUILTIN
        .iter()
        .chain(
            [
                "todo",
                "task",
                "side_request",
                "skill",
                "tool_search",
                "tool_load",
                "artifact_read",
                "working_state",
                "recall",
                "retain",
                "memory_update",
            ]
            .iter(),
        )
        .map(|s| s.to_string())
        .collect()
}
