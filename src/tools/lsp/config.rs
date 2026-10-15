//! `[lsp]` configuration: write-through switches plus per-server tables.
//!
//! ```toml
//! [lsp]
//! diagnostics_on_write = true   # report language-server diagnostics in edit/write results
//! format_on_write = false       # format edited files through the server before reporting
//! start_on_write = false        # let an edit start a not-yet-running server (runs project code)
//!
//! [lsp.rust-analyzer]           # override a built-in server by name ...
//! settings = { rust-analyzer = { checkOnSave = true } }
//!
//! [lsp.zig]                     # ... or add a new one
//! program = "zls"
//! extensions = ["zig"]
//! root_markers = ["build.zig"]
//! ```
//!
//! The switches and server tables share one TOML table, so the switch names are reserved.

use serde::de::{Deserializer, Error as _};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A stdio language server used by the `lsp` tool, or an override of a built-in one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    /// Executable; empty inherits the built-in server of the same name.
    #[serde(default, skip_serializing_if = "is_empty_path")]
    pub program: PathBuf,
    /// Arguments; omitted inherits the built-in server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// File extensions (without the dot) or exact file names this server handles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Files (globs allowed, e.g. `*.cabal`) marking a project root. Empty: the workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_markers: Option<Vec<String>>,
    /// Linters run after the primary server and only contribute diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linter: Option<bool>,
    /// Answers to `workspace/configuration` and the initial `didChangeConfiguration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    /// `initializationOptions` sent with `initialize`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_options: Option<Value>,
    /// LSP `languageId` override for opened documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    /// Turn off a built-in server of the same name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

fn is_empty_path(path: &PathBuf) -> bool {
    path.as_os_str().is_empty()
}

#[derive(Debug, Clone, PartialEq)]
pub struct LspConfig {
    /// Fetch diagnostics after edit/write and append them to the tool result.
    pub diagnostics_on_write: bool,
    /// Ask the server to format each written file (applied as a checkpointed transaction).
    pub format_on_write: bool,
    /// Allow an edit to start a language server that is not running yet. Starting a server runs
    /// project code outside the sandbox, so this is an explicit operator opt-in.
    pub start_on_write: bool,
    pub servers: BTreeMap<String, LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            diagnostics_on_write: true,
            format_on_write: false,
            start_on_write: false,
            servers: BTreeMap::new(),
        }
    }
}

const SWITCHES: [&str; 3] = ["diagnostics_on_write", "format_on_write", "start_on_write"];

impl Serialize for LspConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(SWITCHES.len() + self.servers.len()))?;
        map.serialize_entry("diagnostics_on_write", &self.diagnostics_on_write)?;
        map.serialize_entry("format_on_write", &self.format_on_write)?;
        map.serialize_entry("start_on_write", &self.start_on_write)?;
        for (name, server) in &self.servers {
            map.serialize_entry(name, server)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for LspConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Entry {
            Switch(bool),
            Server(LspServerConfig),
        }
        let raw = BTreeMap::<String, Entry>::deserialize(deserializer)?;
        let mut config = Self::default();
        for (key, entry) in raw {
            match (key.as_str(), entry) {
                ("diagnostics_on_write", Entry::Switch(value)) => {
                    config.diagnostics_on_write = value
                }
                ("format_on_write", Entry::Switch(value)) => config.format_on_write = value,
                ("start_on_write", Entry::Switch(value)) => config.start_on_write = value,
                (name, Entry::Switch(_)) => {
                    return Err(D::Error::custom(format!(
                        "unknown lsp setting {name:?}; expected one of {} or a [lsp.<server>] table",
                        SWITCHES.join(", ")
                    )));
                }
                (name, Entry::Server(_)) if SWITCHES.contains(&name) => {
                    return Err(D::Error::custom(format!(
                        "lsp.{name} is a boolean switch, not a server"
                    )));
                }
                (_, Entry::Server(server)) => {
                    config.servers.insert(key, server);
                }
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_and_servers_share_the_lsp_table() {
        let config: LspConfig = toml::from_str(
            "format_on_write = true\n[zig]\nprogram = \"zls\"\nextensions = [\"zig\"]\n[rust-analyzer]\nsettings = { rust-analyzer = { checkOnSave = true } }\n",
        )
        .unwrap();
        assert!(config.diagnostics_on_write && config.format_on_write && !config.start_on_write);
        assert_eq!(config.servers["zig"].program, PathBuf::from("zls"));
        assert!(
            config.servers["rust-analyzer"]
                .program
                .as_os_str()
                .is_empty()
        );
        let round: LspConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(round, config);
    }

    #[test]
    fn unknown_switches_are_rejected() {
        assert!(toml::from_str::<LspConfig>("diagnostics = true").is_err());
        assert!(toml::from_str::<LspConfig>("[format_on_write]\nprogram = \"x\"").is_err());
    }
}
