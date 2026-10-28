//! Document-start init scripts for the browser context. The driver keeps the Playwright disposables; this
//! module validates model input and loads path-based sources. Script text is never echoed or logged.

use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

/// Largest accepted init script, in bytes, whether given inline or read from a workspace file.
pub const INIT_SCRIPT_MAX_BYTES: usize = 64 * 1024;

const SCRIPT_ID_MAX_BYTES: usize = 64;

/// Validate an init-script action and build the driver request fields.
///
/// `resolve` maps a model-supplied workspace path to an absolute path. Path sources are read here so the driver
/// receives only validated text.
pub fn parse<E: std::fmt::Display>(
    action: &str,
    args: &Value,
    resolve: impl FnOnce(&str) -> Result<std::path::PathBuf, E>,
) -> Result<Value, String> {
    match action {
        "add_init_script" => match (args.get("source"), args.get("path")) {
            (Some(Value::String(source)), None) => {
                check_source(source)?;
                Ok(json!({"source": source}))
            }
            (None, Some(Value::String(input))) => {
                let path = resolve(input).map_err(|error| error.to_string())?;
                Ok(json!({"source": read_script_file(&path)?}))
            }
            _ => Err("add_init_script requires exactly one of source (inline script text) or path (workspace file)".into()),
        },
        "remove_init_script" => {
            let id = args
                .get("script_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= SCRIPT_ID_MAX_BYTES)
                .ok_or("remove_init_script requires script_id, an id from add_init_script or init_scripts")?;
            Ok(json!({"script_id": id}))
        }
        "init_scripts" => Ok(json!({})),
        _ => Err(format!("unsupported init script action {action:?}")),
    }
}

fn check_source(source: &str) -> Result<(), String> {
    if source.trim().is_empty() {
        return Err("init script source must not be empty".into());
    }
    if source.len() > INIT_SCRIPT_MAX_BYTES {
        return Err(format!(
            "init script source exceeds {INIT_SCRIPT_MAX_BYTES} bytes"
        ));
    }
    Ok(())
}

/// Read a workspace file as UTF-8 text no larger than [`INIT_SCRIPT_MAX_BYTES`]. Errors never include the content.
pub fn read_script_file(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("init script file: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("init script path is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(INIT_SCRIPT_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > INIT_SCRIPT_MAX_BYTES {
        return Err(format!(
            "init script file exceeds {INIT_SCRIPT_MAX_BYTES} bytes"
        ));
    }
    let source =
        String::from_utf8(bytes).map_err(|_| "init script file is not UTF-8".to_owned())?;
    check_source(&source)?;
    Ok(source)
}

#[cfg(test)]
mod tests;
