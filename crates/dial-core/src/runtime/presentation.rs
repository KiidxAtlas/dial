//! Portable display contract shared by tools, journals and surfaces.
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyStyle {
    #[default]
    Text,
    Quiet,
    Diff,
    Write,
    Plan,
    Workers,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Tools,
    Shell,
    Edit,
    Connect,
    Workers,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presentation {
    pub label: String,
    pub target: String,
    #[serde(default)]
    pub tone: Tone,
    #[serde(default)]
    pub body: BodyStyle,
    #[serde(default)]
    pub facts: Vec<String>,
    /// Optional compact result, never a replacement for expanded evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Bound a display fragment by UTF-8 bytes without changing its semantic payload.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}
