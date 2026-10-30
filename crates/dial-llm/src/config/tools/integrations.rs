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
    ("evidence_search", true, true),
    ("investigate", true, true),
    ("workbench", true, true),
    ("data", true, true),
    ("document", true, true),
    ("audio", true, true),
    ("visual", true, true),
    ("timeline", true, true),
    ("research", true, true),
    ("reality", true, true),
    ("perception", true, true),
    ("runtime_inspect", true, true),
    ("network_inspect", true, true),
    ("database", true, true),
    // Availability is additionally gated by experimental.video_analysis (off by default).
    ("video", true, false),
    ("archive", true, true),
    ("dep_source", true, true),
    // Registered only while the `image_embedding` role is assigned.
    ("image_search", true, true),
    ("github", false, false),
    ("notebook", false, false),
    ("security_scan", false, false),
    ("computer", false, false),
    ("glab_mr", false, false),
    ("ida", false, false),
    ("image_gen", false, false),
    ("report_tool_issue", false, false),
}
