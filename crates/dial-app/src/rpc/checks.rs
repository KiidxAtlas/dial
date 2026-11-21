//! Project verification checks over plain RPC (`checks` group). `list_checks` answers what `/checks`
//! shows: the automatic plan for the uncommitted changes and the configured `verification.commands`.
//! `add_check` and `remove_check` edit the project check list through the owner `/checks add` and
//! `/checks remove` use (`crate::checks`). A check is a shell command that runs after edits, so both
//! writes need `confirm: true`, as `skill_action` does for changes that remove or run something.
//! Saved changes apply on `apply_settings`. A duplicate or unknown name is a coded refusal.
use super::{check_keys, coded};
use crate::config::paths::Layout;
use crate::config::types::{Config, VerificationCommand};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};

/// Keys `add_check` accepts; anything else is refused rather than ignored.
const ADD_KEYS: &[&str] = &[
    "name",
    "command",
    "file_extensions",
    "timeout_secs",
    "confirm",
];
/// Keys `remove_check` accepts.
const REMOVE_KEYS: &[&str] = &["name", "confirm"];

/// `list_checks`: the automatic checks for the working tree and the configured checks.
pub(super) fn list(config: &Config) -> Value {
    let plan = crate::checks::plan(config);
    json!({
        "enabled": config.verification.enabled,
        "changed_files": plan.changed,
        "automatic": plan.automatic.iter().map(|check| json!({
            "name": check.name,
            "command": check.command,
            "timeout_secs": check.timeout_secs,
        })).collect::<Vec<_>>(),
        "configured": config.verification.commands.iter().map(configured_row).collect::<Vec<_>>(),
    })
}

fn configured_row(check: &VerificationCommand) -> Value {
    json!({
        "name": check.name,
        "command": check.command,
        "file_extensions": check.file_extensions,
        "timeout_secs": check.timeout_secs,
    })
}

/// `add_check`: appends one check to the project check list, as `/checks add` does.
pub(super) fn add(layout: &Layout, params: &Value) -> Result<Value> {
    check_keys(params, ADD_KEYS, "add_check")?;
    confirm(
        params,
        "add_check",
        "adds a shell command that runs after edits",
    )?;
    let check = add_request(params)?;
    // The owner checks the saved effective configuration under the settings locks, not this
    // session's snapshot: a name removed (but not yet applied) can be added back at once.
    crate::checks::add_project(layout, &check).map_err(edit_refused)?;
    Ok(json!({
        "name": check.name,
        "scope": "project",
        "saved": true,
        "apply_settings": true,
    }))
}

/// `remove_check`: removes a check from the project check list, as `/checks remove` does. Checks
/// from the user configuration are not removed here.
pub(super) fn remove(layout: &Layout, params: &Value) -> Result<Value> {
    check_keys(params, REMOVE_KEYS, "remove_check")?;
    confirm(params, "remove_check", "removes a project check")?;
    let name = name_param(params, "remove_check")?;
    crate::checks::remove_project(layout, &name).map_err(edit_refused)?;
    Ok(json!({
        "name": name,
        "scope": "project",
        "removed": true,
        "apply_settings": true,
    }))
}

/// Refuses a write that needs `confirm: true` when the client did not send it.
fn confirm(params: &Value, method: &str, effect: &str) -> Result<()> {
    match params.get("confirm") {
        Some(Value::Bool(true)) => Ok(()),
        Some(Value::Bool(false)) | None => Err(coded(
            ErrorCode::InvalidParams,
            format!("{method} {effect}; ask the user, then send it again with confirm: true"),
        )),
        Some(_) => Err(coded(
            ErrorCode::InvalidParams,
            "params.confirm must be a boolean",
        )),
    }
}

/// The check `add_check` describes. Its shape is parsed here; the rules (name, command, extension
/// normalisation) are the owner's, shared with `/checks add`.
fn add_request(params: &Value) -> Result<VerificationCommand> {
    let name = name_param(params, "add_check")?;
    let command = match params.get("command") {
        Some(Value::String(command)) => command.clone(),
        _ => {
            return Err(coded(
                ErrorCode::InvalidParams,
                "add_check requires params.command",
            ));
        }
    };
    let file_extensions = match params.get("file_extensions").filter(|v| !v.is_null()) {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    coded(
                        ErrorCode::InvalidParams,
                        "params.file_extensions must hold strings",
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(coded(
                ErrorCode::InvalidParams,
                "params.file_extensions must be an array of strings",
            ));
        }
    };
    let timeout_secs = match params.get("timeout_secs").filter(|v| !v.is_null()) {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| {
            coded(
                ErrorCode::InvalidParams,
                "params.timeout_secs must be a whole number of seconds",
            )
        })?,
    };
    VerificationCommand::new(&name, &command, file_extensions, timeout_secs)
        .map_err(|error| coded(ErrorCode::InvalidParams, format!("{error:#}")))
}

/// `params.name` as a string; the owner's rule decides whether it is a valid check name.
fn name_param(params: &Value, method: &str) -> Result<String> {
    match params.get("name") {
        Some(Value::String(name)) => Ok(name.clone()),
        _ => Err(coded(
            ErrorCode::InvalidParams,
            format!("{method} requires params.name"),
        )),
    }
}

/// Maps the owner's refusals to coded errors; any other failure stays a plain error.
fn edit_refused(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<crate::checks::EditRefused>() {
        Some(crate::checks::EditRefused::Exists(_)) => {
            coded(ErrorCode::Conflict, error.to_string())
        }
        Some(crate::checks::EditRefused::Missing(_)) => {
            coded(ErrorCode::NotFound, error.to_string())
        }
        None => error,
    }
}

#[cfg(test)]
mod tests;
