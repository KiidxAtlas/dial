//! Native RPC settings methods served by the controller: `settings_schema`, `get_settings`,
//! `set_setting` and `reset_setting`. They share `dial config`'s owner
//! (`cli::config_models`), so coercion, validation, redaction and locked publication are
//! identical. Values describe the saved configuration files; the running process keeps the
//! configuration it was launched with, so saved changes apply when `dial rpc` is restarted.
use super::coded;
use crate::cli::config_models;
use crate::config::paths::Layout;
use crate::config::settings_store::{self, Scope};
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

/// One saved setting (`key`) or every saved setting, with credentials redacted. Each result
/// names the layer that sets the key (`scope`: `project` over `global`); a key set by neither
/// layer (a built-in default) carries no `scope`.
pub(super) fn get(layout: &Layout, params: &Value) -> Result<Value> {
    let overlay = toml::Table::new();
    let layers = Layers::load(layout)?;
    match params.get("key").filter(|value| !value.is_null()) {
        Some(_) => {
            let key = key(params, "get_settings")?;
            let value = config_models::setting_value(layout, &overlay, &key)?;
            let mut result = json!({"key": key, "value": value});
            layers.annotate(&mut result, &key);
            Ok(result)
        }
        None => {
            let mut entries = config_models::setting_entries(layout, &overlay)?;
            for entry in &mut entries {
                if let Some(key) = entry["key"].as_str().map(str::to_owned) {
                    layers.annotate(entry, &key);
                }
            }
            Ok(json!({"settings": entries}))
        }
    }
}

/// The saved project and global layers, loaded once per `get_settings` call.
struct Layers {
    project: toml::Table,
    global: toml::Table,
}

impl Layers {
    fn load(layout: &Layout) -> Result<Self> {
        Ok(Self {
            project: settings_store::load_layer(layout, Scope::Project)?,
            global: settings_store::load_layer(layout, Scope::Global)?,
        })
    }

    /// The most specific layer that sets exactly `key`, if any.
    fn scope_of(&self, key: &str) -> Option<Scope> {
        if config_models::lookup(&self.project, key).is_some() {
            Some(Scope::Project)
        } else if config_models::lookup(&self.global, key).is_some() {
            Some(Scope::Global)
        } else {
            None
        }
    }

    fn annotate(&self, entry: &mut Value, key: &str) {
        if let (Some(scope), Some(fields)) = (self.scope_of(key), entry.as_object_mut()) {
            fields.insert("scope".to_owned(), json!(scope_name(scope)));
        }
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
