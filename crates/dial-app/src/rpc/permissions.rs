//! Session permission rules through the owner `/permissions` uses: list, add and remove rules with
//! the same validation, command-prefix normalisation and `--persist` store as the terminal.
//! Indexes are zero-based positions in the list that `list_permission_rules` returns.
use super::coded;
use crate::agent::Agent;
use crate::agent::safety::approval::{Effect, PermissionRule, bash_prefix};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_TOOL_BYTES: usize = 128;

fn invalid(error: impl std::fmt::Display) -> anyhow::Error {
    coded(ErrorCode::InvalidParams, error)
}

fn request<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T> {
    serde_json::from_value(if params.is_null() {
        json!({})
    } else {
        params.clone()
    })
    .map_err(invalid)
}

/// `list_permission_rules` takes no parameters; any field is refused rather than ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRequest {
    effect: Effect,
    tool: String,
    #[serde(default)]
    command_prefix: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    persist: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveRequest {
    index: usize,
    #[serde(default)]
    persist: bool,
}

/// One rule as the wire shows it: its zero-based index, the persisted fields and the `/permissions` wording.
fn rule_json(index: usize, rule: &PermissionRule) -> Value {
    json!({
        "index": index,
        "effect": rule.effect,
        "tool": rule.tool,
        "command_prefix": rule.command_prefix,
        "paths": rule.paths,
        "scope": rule.scope(),
    })
}

/// Every session rule, in evaluation order. Result: `{rules:[{index, effect, tool, command_prefix, paths, scope}]}`.
pub(super) fn list(agent: &Agent<'_>, params: &Value) -> Result<Value> {
    request::<ListRequest>(params)?;
    let rules: Vec<Value> = agent
        .permission_rules()
        .iter()
        .enumerate()
        .map(|(index, rule)| rule_json(index, rule))
        .collect();
    Ok(json!({ "rules": rules }))
}

/// Normalise one requested rule as `/permissions allow|deny` does. A bash prefix is stored as the
/// parsed program and arguments; every rule passes the shared validator before the store sees it,
/// so a bad request is `invalid_params` while a failed save stays `failed`.
fn build_rule(params: AddRequest) -> Result<(PermissionRule, bool)> {
    let tool = params.tool.trim();
    if tool.is_empty() || tool.len() > MAX_TOOL_BYTES || tool.chars().any(char::is_whitespace) {
        return Err(invalid(
            "tool must be a non-empty name without whitespace, at most 128 bytes",
        ));
    }
    let command_prefix = if tool == "bash" {
        if params.command_prefix.iter().any(|word| word.trim().is_empty()) {
            return Err(invalid("command_prefix words must not be empty"));
        }
        let words: Vec<String> = params
            .command_prefix
            .iter()
            .map(|word| word.trim().to_owned())
            .collect();
        bash_prefix(&words).map_err(invalid)?
    } else {
        params.command_prefix
    };
    let rule = PermissionRule {
        effect: params.effect,
        tool: tool.to_owned(),
        command_prefix,
        paths: params
            .paths
            .into_iter()
            .map(|path| path.trim().to_owned())
            .collect(),
    };
    rule.validate().map_err(invalid)?;
    Ok((rule, params.persist))
}

/// Add a session rule, or save it with the project when `persist` is true.
/// Params: `{effect: allow|deny, tool, command_prefix?: [word], paths?: [glob], persist?: bool}`.
/// Result: `{rule, persisted, count}`, where `rule.index` is the new rule's position.
pub(super) fn add(agent: &mut Agent<'_>, params: &Value) -> Result<Value> {
    let (rule, persist) = build_rule(request(params)?)?;
    let index = agent.permission_rules().len();
    let shown = rule_json(index, &rule);
    agent.add_permission_rule(rule, persist)?;
    Ok(json!({
        "rule": shown,
        "persisted": persist,
        "count": agent.permission_rules().len(),
    }))
}

/// Remove the rule at zero-based `index`, from the session and, when `persist` is true, the saved store.
/// Params: `{index, persist?: bool}`. An index outside the list is `not_found`.
/// Result: `{removed, persisted, count}`.
pub(super) fn remove(agent: &mut Agent<'_>, params: &Value) -> Result<Value> {
    let params: RemoveRequest = request(params)?;
    let Some(rule) = agent.permission_rules().get(params.index).cloned() else {
        return Err(coded(
            ErrorCode::NotFound,
            format!("no permission rule at index {}", params.index),
        ));
    };
    let shown = rule_json(params.index, &rule);
    agent.remove_permission_rule(params.index, params.persist)?;
    Ok(json!({
        "removed": shown,
        "persisted": params.persist,
        "count": agent.permission_rules().len(),
    }))
}
