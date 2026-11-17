//! Skill packages over plain RPC (`skills` group). `list_skills` answers the read-only `/skills`
//! forms (`list`, `sources`, `installed`, `search`) with the owner's own text. `skill_action` runs
//! one change (`enable`, `disable`, `import`, `install`, `update`, `remove`) through the same owner,
//! `storage::skills::management::command`, on a worker thread, one at a time. Each typed field
//! becomes one argument word for that owner; no shell line is parsed. Changes that write files,
//! download a package or remove or disable something need `confirm: true`, as `extension_action`.
use super::{Sender, coded};
use crate::config::paths::Layout;
use crate::storage::skills::management;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Keys each `skill_action` accepts; anything else is refused rather than ignored.
const ACTION_KEYS: &[&str] = &[
    "action", "name", "spec", "path", "scope", "dry_run", "update", "confirm",
];

/// The one skill change running at a time.
#[derive(Default)]
pub(super) struct SkillJobs {
    running: AtomicBool,
}

/// Clears the busy flag when the worker ends, including by panic.
struct Running(Arc<SkillJobs>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
    }
}

/// A validated `skill_action`: the owner's words, the label echoed back, and whether it needs
/// `confirm: true`.
#[derive(Debug, PartialEq)]
pub(super) struct Change {
    label: String,
    words: Vec<String>,
    needs_confirm: bool,
}

/// `list_skills`: a read-only `/skills` form, answered with the owner's text.
pub(super) fn list(
    layout: &Layout,
    registries: &BTreeMap<String, String>,
    params: &Value,
) -> Result<Value> {
    let words = list_words(params)?;
    let text = management::command(
        layout,
        registries,
        words.clone(),
        &AtomicBool::new(false),
        &mut |_: &str| {},
    )?;
    Ok(json!({"action": words[0], "text": text}))
}

/// The owner words for a read-only `list_skills`: `list` (default), `sources`, `installed`, or
/// `search` with `query` (whitespace-separated words, none of them flags).
fn list_words(params: &Value) -> Result<Vec<String>> {
    let action = match params.get("action").filter(|value| !value.is_null()) {
        None => "list",
        Some(Value::String(action)) => action.as_str(),
        Some(_) => {
            return Err(coded(
                ErrorCode::InvalidParams,
                "list_skills action must be a string",
            ));
        }
    };
    match action {
        "list" | "sources" | "installed" => Ok(vec![action.to_owned()]),
        // `search` fetches each configured registry index over the network. Only the query is
        // bounded here; the owner validates the registry names and the transfers.
        "search" => {
            const MAX_WORDS: usize = 8;
            const MAX_BYTES: usize = 200;
            let query = string_field(params, "query")?
                .ok_or_else(|| coded(ErrorCode::InvalidParams, "search requires params.query"))?;
            let words: Vec<String> = query.split_whitespace().map(str::to_owned).collect();
            if words.is_empty()
                || words.len() > MAX_WORDS
                || query.len() > MAX_BYTES
                || words.iter().any(|word| word.starts_with('-'))
            {
                return Err(coded(
                    ErrorCode::InvalidParams,
                    format!(
                        "search requires 1 to {MAX_WORDS} words, at most {MAX_BYTES} bytes, none starting with '-'"
                    ),
                ));
            }
            Ok(std::iter::once("search".to_owned()).chain(words).collect())
        }
        other => Err(coded(
            ErrorCode::InvalidParams,
            format!("list_skills action {other} is not one of list, sources, installed, search"),
        )),
    }
}

/// Starts `skill_action` on a worker thread that answers `id` when the change finishes. Refused
/// before anything runs when the request is malformed, needs a confirmation it did not get, or
/// another skill change is still running.
pub(super) fn start(
    jobs: &Arc<SkillJobs>,
    out: &Sender,
    layout: &Layout,
    registries: BTreeMap<String, String>,
    id: &Value,
    params: &Value,
) -> Result<()> {
    let change = change_request(params)?;
    confirm_gate(&change, params)?;
    if jobs.running.swap(true, Ordering::SeqCst) {
        return Err(coded(
            ErrorCode::Busy,
            "another skill change is still running",
        ));
    }
    let (jobs, out, layout, id) = (jobs.clone(), out.clone(), layout.clone(), id.clone());
    thread::spawn(move || {
        let _running = Running(jobs);
        let mut progress = Vec::new();
        let outcome = management::command(
            &layout,
            &registries,
            change.words,
            &AtomicBool::new(false),
            &mut |line: &str| progress.push(line.to_owned()),
        );
        out.result(
            &id,
            outcome.map(|text| {
                json!({
                    "ok": true,
                    "action": change.label,
                    "text": text,
                    "progress": progress,
                })
            }),
        );
    });
    Ok(())
}

/// Refuses a change that needs `confirm: true` when the client did not send it.
fn confirm_gate(change: &Change, params: &Value) -> Result<()> {
    if change.needs_confirm && params["confirm"] != true {
        return Err(coded(
            ErrorCode::InvalidParams,
            format!(
                "{}: this changes files, downloads a package, or removes or disables something; ask the user, then send it again with confirm: true",
                change.label
            ),
        ));
    }
    Ok(())
}

/// The owner words and confirmation rule for one `skill_action`.
fn change_request(params: &Value) -> Result<Change> {
    let Some(fields) = params.as_object() else {
        return Err(coded(
            ErrorCode::InvalidParams,
            "skill_action requires params",
        ));
    };
    if let Some(unknown) = fields
        .keys()
        .find(|key| !ACTION_KEYS.contains(&key.as_str()))
    {
        return Err(coded(
            ErrorCode::InvalidParams,
            format!("skill_action does not accept params.{unknown}"),
        ));
    }
    let action = string_field(params, "action")?.ok_or_else(|| {
        coded(
            ErrorCode::InvalidParams,
            "skill_action requires params.action",
        )
    })?;
    let scope = match string_field(params, "scope")?.as_deref() {
        None | Some("project") => Vec::new(),
        Some("user") => vec!["--user".to_owned()],
        Some(_) => {
            return Err(coded(
                ErrorCode::InvalidParams,
                "scope must be project or user",
            ));
        }
    };
    let flag = |key: &str| -> Result<bool> {
        match params.get(key).filter(|value| !value.is_null()) {
            None => Ok(false),
            Some(Value::Bool(flag)) => Ok(*flag),
            Some(_) => Err(coded(
                ErrorCode::InvalidParams,
                format!("params.{key} must be a boolean"),
            )),
        }
    };
    let name = |key: &str| -> Result<String> {
        match string_field(params, key)? {
            Some(value) if is_word(&value) => Ok(value),
            _ => Err(coded(
                ErrorCode::InvalidParams,
                format!("{action} requires params.{key}: one word that does not start with '-'"),
            )),
        }
    };
    let (words, needs_confirm) = match action.as_str() {
        "enable" => (with(["enable", &name("name")?], &scope), false),
        "disable" => (with(["disable", &name("name")?], &scope), true),
        "remove" => (vec!["remove".to_owned(), name("name")?], true),
        "install" => (with(["install", &name("spec")?], &scope), true),
        "update" => match string_field(params, "name")? {
            None => (vec!["update".to_owned()], true),
            Some(_) => (vec!["update".to_owned(), name("name")?], true),
        },
        "import" => {
            let path = string_field(params, "path")?
                .filter(|path| !path.trim().is_empty() && !path.starts_with('-'))
                .ok_or_else(|| {
                    coded(
                        ErrorCode::InvalidParams,
                        "import requires params.path: a path that does not start with '-'",
                    )
                })?;
            let dry_run = flag("dry_run")?;
            let mut words = with(["import", path.as_str()], &scope);
            if dry_run {
                words.push("--dry-run".to_owned());
            }
            if flag("update")? {
                words.push("--update".to_owned());
            }
            (words, !dry_run)
        }
        other => {
            return Err(coded(
                ErrorCode::InvalidParams,
                format!(
                    "skill_action action {other} is not one of enable, disable, import, install, update, remove"
                ),
            ));
        }
    };
    Ok(Change {
        label: action,
        words,
        needs_confirm,
    })
}

/// Owner words: the fixed leading words, then the scope flag when the user scope was asked for.
fn with<const N: usize>(head: [&str; N], scope: &[String]) -> Vec<String> {
    head.iter()
        .map(|word| (*word).to_owned())
        .chain(scope.iter().cloned())
        .collect()
}

/// A string field: `None` when absent or null, an error when it is not a string.
fn string_field(params: &Value, key: &str) -> Result<Option<String>> {
    match params.get(key).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(coded(
            ErrorCode::InvalidParams,
            format!("params.{key} must be a string"),
        )),
    }
}

/// A package name, spec or flag-free word: no whitespace, not empty, not shaped like a flag.
fn is_word(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('-') && !value.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests;
