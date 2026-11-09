//! Integration tool names offered by configuration and their defaults.

/// Configuration metadata only; schemas and authorization remain in the existing tool contracts.
pub struct Integration {
    pub name: &'static str,
    pub default_enabled: bool,
    pub always_registered: bool,
}

pub fn default_integrations() -> Vec<String> {
    INTEGRATIONS
        .iter()
        .filter(|entry| entry.default_enabled)
        .map(|entry| entry.name.to_owned())
        .collect()
}

macro_rules! integrations {
    ($(($name:literal, $default:literal, $always:literal)),* $(,)?) => {
        pub const INTEGRATION_TOOLS: &[&str] = &[$($name),*];
        pub const INTEGRATIONS: &[Integration] = &[$(Integration {
            name: $name, default_enabled: $default, always_registered: $always,
        }),*];
    };
}
integrations! {
    ("bg", true, false),
    ("monitor", true, true),
    ("web_fetch", true, false),
    ("web_search", true, false),
    ("lsp", true, false),
    ("ast_search", true, false),
    ("ast_edit", true, false),
    ("dap", true, false),
    ("git", true, false),
    ("ssh", true, false),
    ("browser", true, true),
    ("eval", true, true),
    ("find", true, true),
    ("autoresearch", true, true),
    ("evidence", true, true),
    ("data", true, true),
    ("document", true, true),
    ("audio", true, true),
    ("visual", true, true),
    ("research", true, true),
    ("runtime_inspect", true, true),
    ("network_inspect", true, true),
    ("database", true, true),
    // Availability is additionally gated by experimental.video_analysis (off by default).
    ("video", true, false),
    ("archive", true, true),
    ("dep_source", true, true),
    // Registered only while vision.image_embedding is configured.
    ("image_search", true, true),
    ("image_weights", true, true),
    ("github", false, false),
    ("notebook", false, false),
    ("security_scan", false, false),
    ("computer", false, false),
    ("glab_mr", false, false),
    // Read-only local MLX weight and activation inspection; approval-gated.
    ("model_weights", false, false),
    ("ida", false, false),
    ("image_gen", false, false),
    ("report_tool_issue", false, false),
}

/// Integration names retired by a merge: (retired, successor). Saved configuration and
/// `--tools` entries that name a retired integration select its successor
/// (`config::migrate_merged_integrations`, `cli::tool_flags::canonical_tool_name`).
/// `kernel` became `eval` (`language = "py"`); the evidence family became `evidence` actions.
pub const MERGED_INTEGRATIONS: &[(&str, &str)] = &[
    ("kernel", "eval"),
    ("evidence_search", "evidence"),
    ("investigate", "evidence"),
    ("workbench", "evidence"),
    ("reality", "evidence"),
    ("timeline", "evidence"),
    ("perception", "evidence"),
];

/// The successor of a retired integration name, or the name itself.
pub fn canonical_integration(name: &str) -> &str {
    MERGED_INTEGRATIONS
        .iter()
        .find(|(retired, _)| retired.eq_ignore_ascii_case(name))
        .map_or(name, |&(_, successor)| successor)
}
