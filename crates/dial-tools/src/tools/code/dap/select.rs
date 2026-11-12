//! Built-in adapter catalogue and automatic selection by
//! program extension and project root markers when no adapter is named.
use crate::config::tools::dap::DapAdapterConfig;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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

/// Appends (never substitutes) a `PATHEXT` suffix on Windows, so `node.v2` finds `node.v2.cmd`.
fn resolve_program(candidate: &Path) -> Option<PathBuf> {
    crate::platform::fs::executable_candidate(candidate)
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
    let candidates = ranked(catalogue, program, root);
    let installed = candidates
        .iter()
        .find(|(_, config)| available(config, root))
        .map(|(name, config)| ((*name).clone(), (*config).clone()));
    installed.ok_or_else(|| {
        if candidates.is_empty() {
            "no DAP adapter matches this program or project; pass `adapter` (e.g. gdb, lldb-dap, debugpy, dlv) or configure [dap.<id>]".to_owned()
        } else {
            format!(
                "matching DAP adapters are not installed: {}; install one or pass `adapter`",
                candidates
                    .iter()
                    .map(|(name, config)| format!("{name} (`{}`)", config.program.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    })
}

/// Adapters matching `program` (by extension) or the project (by root markers), best first.
fn ranked<'a>(
    catalogue: &'a BTreeMap<String, DapAdapterConfig>,
    program: Option<&Path>,
    root: &Path,
) -> Vec<(&'a String, &'a DapAdapterConfig)> {
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
    candidates
        .into_iter()
        .map(|(.., name, config)| (name, config))
        .collect()
}

/// The best adapter `select` would want that is missing but has a Dial install recipe for its
/// bare program name, as `(adapter, recipe id)`. `None` when one is installed already, nothing
/// matches, or no matching adapter can be installed.
pub fn installable(
    catalogue: &BTreeMap<String, DapAdapterConfig>,
    requested: Option<&str>,
    program: Option<&Path>,
    root: &Path,
) -> Option<(String, String)> {
    let candidates: Vec<(&String, &DapAdapterConfig)> = match requested {
        Some(name) => catalogue.get_key_value(name).into_iter().collect(),
        None => ranked(catalogue, program, root),
    };
    if candidates.iter().any(|(_, config)| available(config, root)) {
        return None;
    }
    candidates.into_iter().find_map(|(name, config)| {
        let bare = config.program.to_str()?;
        (config.address.is_empty()
            && config.program.components().count() == 1
            && crate::setup::deps::recipe(bare).is_some())
        .then(|| (name.clone(), bare.to_owned()))
    })
}

#[cfg(test)]
mod tests;
