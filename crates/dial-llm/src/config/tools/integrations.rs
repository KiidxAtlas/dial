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
    ("web_fetch", true, false),
    ("web_search", true, false),
    ("lsp", true, false),
    ("ast_search", true, false),
    ("ast_edit", true, false),
    ("dap", true, false),
    ("git", true, false),
    ("ssh", true, false),
    ("browser", true, true),
    ("kernel", true, true),
    ("eval", true, true),
    ("find", true, true),
    ("autoresearch", true, true),
    ("archive", true, true),
    ("dep_source", true, true),
    ("github", false, false),
    ("notebook", false, false),
    ("security_scan", false, false),
    ("computer", false, false),
}
