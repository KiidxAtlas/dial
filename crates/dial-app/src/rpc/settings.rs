//! Native RPC settings methods served by the controller: `settings_schema`, `get_settings`,
//! `set_setting` and `reset_setting`. They share `dial config`'s owner
//! (`cli::config_models`), so coercion, validation, redaction and locked publication are
//! identical. Values describe the saved configuration files; the running process keeps the
//! configuration it was launched with, so saved changes apply when `dial rpc` is restarted.
use super::coded;
use crate::cli::config_models;
use crate::config::paths::Layout;
use crate::config::settings_store::Scope;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};

pub(super) fn scope(params: &Value) -> Result<Scope> {
    match params.get("scope").filter(|value| !value.is_null()) {
        None => Ok(Scope::Global),
        Some(value) => match value.as_str() {
            Some("global") => Ok(Scope::Global),
            Some("project") => Ok(Scope::Project),
            _ => Err(coded(
                ErrorCode::InvalidParams,
                "scope must be global or project",
            )),
        },
    }
}

pub(super) fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Global => "global",
        Scope::Project => "project",
    }
}

fn key(params: &Value, method: &str) -> Result<String> {
    let key = params["key"].as_str().ok_or_else(|| {
        coded(
            ErrorCode::InvalidParams,
            format!("{method} requires params.key"),
        )
    })?;
    if key.trim().is_empty() {
        return Err(coded(ErrorCode::InvalidParams, "key must not be empty"));
    }
    Ok(key.to_owned())
}

/// The TOML form of a JSON value. Integers stay integers (TOML settings distinguish them from
/// floats); null, including inside arrays and tables, has no TOML form.
fn toml_value(value: &Value) -> Option<toml::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(flag) => toml::Value::Boolean(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => toml::Value::Integer(integer),
            None => toml::Value::Float(number.as_f64()?),
        },
        Value::String(text) => toml::Value::String(text.clone()),
        Value::Array(items) => {
            toml::Value::Array(items.iter().map(toml_value).collect::<Option<_>>()?)
        }
        Value::Object(fields) => toml::Value::Table(
            fields
                .iter()
                .map(|(name, value)| Some((name.clone(), toml_value(value)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

/// The settings reference: every declared setting with its kind, constraints and default.
pub(super) fn schema() -> Value {
    crate::config::settings::reference()
}

/// One saved setting (`key`) or every saved setting, with credentials redacted.
pub(super) fn get(layout: &Layout, params: &Value) -> Result<Value> {
    let overlay = toml::Table::new();
    match params.get("key").filter(|value| !value.is_null()) {
        Some(_) => {
            let key = key(params, "get_settings")?;
            let value = config_models::setting_value(layout, &overlay, &key)?;
            Ok(json!({"key": key, "value": value}))
        }
        None => Ok(json!({"settings": config_models::setting_entries(layout, &overlay)?})),
    }
}

/// Saves one setting through the `dial config set` path. The value is never echoed.
pub(super) fn set(layout: &Layout, params: &Value) -> Result<Value> {
    let key = key(params, "set_setting")?;
    let scope = scope(params)?;
    let raw = params
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(|| {
            coded(
                ErrorCode::InvalidParams,
                "set_setting requires a non-null params.value",
            )
        })?;
    let value = toml_value(raw).ok_or_else(|| {
        coded(
            ErrorCode::InvalidParams,
            format!("{key}: value cannot be represented in TOML"),
        )
    })?;
    config_models::set_setting(layout, &toml::Table::new(), &key, value, scope)?;
    Ok(json!({"key": key, "scope": scope_name(scope), "saved": true, "restart_required": true}))
}

/// Removes one setting from a layer through the `dial config reset` path.
pub(super) fn reset(layout: &Layout, params: &Value) -> Result<Value> {
    let key = key(params, "reset_setting")?;
    let scope = scope(params)?;
    let removed = config_models::reset_setting(layout, &key, scope)?;
    Ok(
        json!({"key": key, "scope": scope_name(scope), "removed": removed, "restart_required": removed}),
    )
}

#[cfg(test)]
mod tests;
