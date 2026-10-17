//! `[dap]` adapter configuration.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, path::PathBuf};

/// Operator-controlled command used to start one DAP adapter.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DapTransport {
    #[default]
    Stdio,
    Tcp,
    Socket,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DapAdapterConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub transport: DapTransport,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub file_types: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub launch_defaults: serde_json::Map<String, Value>,
    #[serde(default)]
    pub attach_defaults: serde_json::Map<String, Value>,
}

fn default_timeout_secs() -> u64 {
    0
}

const DEFAULTS: &str = include_str!("dap/defaults.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DefaultAdapter {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    connect_mode: Option<String>,
    #[serde(default)]
    file_types: Vec<String>,
    #[serde(default)]
    root_markers: Vec<String>,
    #[serde(default)]
    launch_defaults: Map<String, Value>,
    #[serde(default)]
    attach_defaults: Map<String, Value>,
}

/// The built-in adapters, keyed by adapter name.
pub fn defaults() -> BTreeMap<String, DapAdapterConfig> {
    let parsed: BTreeMap<String, DefaultAdapter> =
        serde_json::from_str(DEFAULTS).expect("bundled DAP defaults are valid JSON");
    parsed
        .into_iter()
        .map(|(name, adapter)| {
            let transport = match adapter.connect_mode.as_deref() {
                Some("tcp") => DapTransport::Tcp,
                Some("socket") => DapTransport::Socket,
                _ => DapTransport::Stdio,
            };
            let config = DapAdapterConfig {
                program: PathBuf::from(adapter.command),
                args: adapter.args,
                env: BTreeMap::new(),
                timeout_secs: 0,
                transport,
                address: String::new(),
                file_types: adapter.file_types,
                root_markers: adapter.root_markers,
                launch_defaults: adapter.launch_defaults,
                attach_defaults: adapter.attach_defaults,
            };
            (name, config)
        })
        .collect()
}

/// Configured adapters override built-ins of the same name.
pub fn catalogue(
    configured: &BTreeMap<String, DapAdapterConfig>,
) -> BTreeMap<String, DapAdapterConfig> {
    let mut all = defaults();
    all.extend(configured.iter().map(|(k, v)| (k.clone(), v.clone())));
    all
}
