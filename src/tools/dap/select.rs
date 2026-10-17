//! Built-in adapter catalogue and automatic selection by
//! program extension and project root markers when no adapter is named.
use super::{DapAdapterConfig, DapTransport};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const DEFAULTS: &str = include_str!("defaults.json");

/// Preference order when several adapters match equally (declaration order).
const ORDER: &[&str] = &[
    "gdb",
    "lldb-dap",
    "codelldb",
    "debugpy",
    "dlv",
    "js-debug-adapter",
    "netcoredbg",
    "kotlin-debug-adapter",
    "rdbg",
    "php-debug-adapter",
    "bash-debug-adapter",
    "dart-debug-adapter",
    "flutter-debug-adapter",
    "elixir-ls-debugger",
];

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

/// Resolve a bare command on PATH; paths with separators resolve against `root`, the adapter's spawn cwd.
pub fn which(program: &Path, root: &Path) -> Option<PathBuf> {
    if program.as_os_str().is_empty() {
        return None;
    }
    if program.components().count() > 1 {
        return resolve_program(&root.join(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| resolve_program(&dir.join(program)))
}

fn resolve_program(candidate: &Path) -> Option<PathBuf> {
    if crate::platform::fs::is_executable(candidate) {
        return Some(candidate.to_path_buf());
    }
    #[cfg(windows)]
    if candidate.extension().is_none() {
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.COM;.BAT;.CMD".into());
        return extensions
            .split(';')
            .map(|extension| candidate.with_extension(extension.trim_start_matches('.')))
            .find(|path| crate::platform::fs::is_executable(path));
    }
    None
}

fn available(config: &DapAdapterConfig, root: &Path) -> bool {
    !config.address.is_empty() || which(&config.program, root).is_some()
}

fn marker_present(dir: &Path, marker: &str) -> bool {
    if let Some(suffix) = marker.strip_prefix('*') {
        return std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().ends_with(suffix))
        });
    }
    dir.join(marker).exists()
}

/// True when a root marker exists in `start` or any ancestor up to and including `root`.
fn root_match(start: &Path, root: &Path, markers: &[String]) -> bool {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if markers.iter().any(|marker| marker_present(current, marker)) {
            return true;
        }
        if current == root {
            break;
        }
        dir = current.parent().filter(|parent| parent.starts_with(root));
    }
    false
}

fn rank(name: &str) -> usize {
    ORDER.iter().position(|n| *n == name).unwrap_or(ORDER.len())
}

/// Pick an adapter for a session. An explicit `requested` name wins; otherwise the program's
/// extension narrows candidates, root markers break ties, then the built-in preference order. Only
/// adapters whose executable is installed are considered.
pub fn select(
    catalogue: &BTreeMap<String, DapAdapterConfig>,
    requested: Option<&str>,
    program: Option<&Path>,
    root: &Path,
) -> Result<(String, DapAdapterConfig), String> {
    if let Some(name) = requested {
        let config = catalogue.get(name).ok_or_else(|| {
            format!(
                "unknown DAP adapter {name:?}; known: {}",
                catalogue.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        if !available(config, root) {
            return Err(format!(
                "DAP adapter {name:?} needs `{}` on PATH; install it or configure [dap.{name}] program",
                config.program.display()
            ));
        }
        return Ok((name.to_owned(), config.clone()));
    }
    let extension = program
        .and_then(|p| p.extension())
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()));
    let start = program
        .and_then(|p| if p.is_dir() { Some(p) } else { p.parent() })
        .unwrap_or(root);
    let mut candidates: Vec<(bool, bool, usize, &String, &DapAdapterConfig)> = catalogue
        .iter()
        .map(|(name, config)| {
            let ext = extension.as_ref().is_some_and(|ext| {
                config
                    .file_types
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(ext))
            });
            let marker = root_match(start, root, &config.root_markers);
            (ext, marker, rank(name), name, config)
        })
        .filter(|(ext, marker, ..)| if extension.is_some() { *ext } else { *marker })
        .collect();
    // Extension match first, then root marker, then preference order.
    candidates.sort_by_key(|(ext, marker, rank, ..)| (!*ext, !*marker, *rank));
    let installed = candidates
        .iter()
        .find(|(.., config)| available(config, root))
        .map(|(.., name, config)| ((*name).clone(), (*config).clone()));
    installed.ok_or_else(|| {
        if candidates.is_empty() {
            "no DAP adapter matches this program or project; pass `adapter` (e.g. gdb, lldb-dap, debugpy, dlv) or configure [dap.<id>]".to_owned()
        } else {
            format!(
                "matching DAP adapters are not installed: {}; install one or pass `adapter`",
                candidates
                    .iter()
                    .map(|(.., name, config)| format!("{name} (`{}`)", config.program.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    })
}

#[cfg(test)]
mod tests;
