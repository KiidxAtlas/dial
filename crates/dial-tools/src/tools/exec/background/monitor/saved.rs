//! Saved watch specifications for `save: true`, kept in this project's private Dial state (never in
//! the workspace). Only the arguments are saved, never captured output. Restoring re-arms the
//! watch through the normal `start` path, so it needs approval and reads only future output.
use crate::tools::Workspace;
use serde_json::{Map, Value};
use std::path::PathBuf;

const FILE: &str = "monitor-watches.json";
const MAX_SAVED: usize = 32;
/// Arguments kept from a `start` call; everything else (action, save) is dropped.
const KEPT: &[&str] = &[
    "path",
    "run",
    "after_secs",
    "pattern",
    "duration_secs",
    "note",
    "on_exit",
];

#[cfg(test)]
thread_local! {
    pub(super) static FILE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn file(workspace: &Workspace) -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = FILE_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Ok(path);
    }
    let layout = crate::config::paths::Layout::discover(workspace.root())
        .map_err(|e| format!("project state: {e:#}"))?;
    Ok(layout.state_dir.join(FILE))
}

fn read_all(workspace: &Workspace) -> Result<Map<String, Value>, String> {
    let path = file(workspace)?;
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(format!(
                "saved watches file {} is not valid",
                path.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("reading saved watches: {e}")),
    }
}

fn write_all(workspace: &Workspace, map: &Map<String, Value>) -> Result<(), String> {
    let path = file(workspace)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("saved watches directory: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(map).map_err(|e| e.to_string())?;
    crate::platform::atomic::write_private(&path, &bytes)
        .map_err(|e| format!("writing saved watches: {e:#}"))
}

pub(super) fn save(workspace: &Workspace, name: &str, args: &Value) -> Result<(), String> {
    let spec: Map<String, Value> = KEPT
        .iter()
        .filter_map(|key| {
            args.get(*key)
                .filter(|value| !value.is_null())
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .chain([("name".to_owned(), Value::from(name))])
        .collect();
    let mut map = read_all(workspace)?;
    if !map.contains_key(name) && map.len() >= MAX_SAVED {
        return Err(format!(
            "at most {MAX_SAVED} watches can be saved; forget one first"
        ));
    }
    map.insert(name.to_owned(), Value::Object(spec));
    write_all(workspace, &map)
}

pub(super) fn load(workspace: &Workspace, name: &str) -> Result<Value, String> {
    read_all(workspace)?
        .remove(name)
        .ok_or_else(|| format!("no saved watch named {name:?}; action saved lists them"))
}

pub(super) fn forget(workspace: &Workspace, name: &str) -> Result<String, String> {
    let mut map = read_all(workspace)?;
    if map.remove(name).is_none() {
        return Err(format!("no saved watch named {name:?}"));
    }
    write_all(workspace, &map)?;
    Ok(format!(
        "Saved watch {name:?} forgotten. Armed watches are unchanged."
    ))
}

pub(super) fn list(workspace: &Workspace) -> Result<String, String> {
    let map = read_all(workspace)?;
    if map.is_empty() {
        return Ok("No saved watches.".into());
    }
    Ok(map
        .iter()
        .map(|(name, spec)| {
            let source = ["path", "run", "after_secs"]
                .iter()
                .find_map(|key| spec.get(*key).map(|value| format!("{key}={value}")))
                .unwrap_or_default();
            let pattern = spec
                .get("pattern")
                .map_or(String::new(), |pattern| format!(" pattern={pattern}"));
            format!("{name}: {source}{pattern}")
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
