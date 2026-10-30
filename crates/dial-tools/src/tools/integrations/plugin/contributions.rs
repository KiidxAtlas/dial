//! Validated registrations, revisioned views and private extension-scoped settings.
pub use crate::runtime::extensions::*;
use crate::{config::paths::Layout, runtime::schema};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub fn validate(contributions: &Contributions) -> Result<()> {
    Contributions::parse(serde_json::to_value(contributions)?)?;
    check_settings_schema(&contributions.settings)?;
    for command in &contributions.commands {
        check_schema(&command.parameters)?;
    }
    for preset in &contributions.presets {
        validate_values(
            &contributions.settings,
            &Value::Object(preset.values.clone()),
        )?;
    }
    Ok(())
}
fn check_schema(value: &Value) -> Result<()> {
    ensure!(value.is_object(), "extension schema must be an object");
    schema::check_schema(value).map_err(anyhow::Error::msg)
}

/// Secret classification deliberately supports only literal properties and homogeneous items.
/// The general validator accepts references/applicators, but our public settings/UI traversal
/// does not resolve them. Refuse mixed secret schemas rather than silently treating aliases as
/// public values. Command/tool schemas continue to use the full general validator.
fn check_settings_schema(value: &Value) -> Result<()> {
    check_schema(value)?;
    ensure!(
        !secret_property(value),
        "the settings document cannot itself be secret"
    );
    fn contains_secret(value: &Value) -> bool {
        secret_property(value)
            || match value {
                Value::Object(object) => object.values().any(contains_secret),
                Value::Array(array) => array.iter().any(contains_secret),
                _ => false,
            }
    }
    fn literal_only(value: &Value) -> Result<()> {
        if let Some(object) = value.as_object() {
            if !secret_property(value) {
                for key in ["default", "const"] {
                    if let Some(candidate) = object.get(key) {
                        ensure_no_secrets(value, candidate)?;
                    }
                }
                for key in ["enum", "examples"] {
                    if let Some(candidates) = object.get(key).and_then(Value::as_array) {
                        for candidate in candidates {
                            ensure_no_secrets(value, candidate)?;
                        }
                    }
                }
            }
            for (key, value) in object {
                ensure!(
                    !matches!(
                        key.as_str(),
                        "$ref"
                            | "$defs"
                            | "definitions"
                            | "allOf"
                            | "anyOf"
                            | "oneOf"
                            | "not"
                            | "if"
                            | "then"
                            | "else"
                            | "patternProperties"
                            | "propertyNames"
                            | "prefixItems"
                            | "additionalItems"
                            | "contains"
                    ) && !(key == "items" && value.is_array())
                        && !(key == "additionalProperties" && value.is_object()),
                    "secret settings support only literal properties and homogeneous items; references and applicators are unsupported"
                );
                if key == "properties" {
                    if let Some(properties) = value.as_object() {
                        for property in properties.values() {
                            literal_only(property)?;
                        }
                    }
                } else if key == "items" {
                    literal_only(value)?;
                }
            }
        }
        Ok(())
    }
    if contains_secret(value) {
        literal_only(value)?;
    }
    Ok(())
}

#[derive(Default)]
pub struct ContributionState {
    views: BTreeMap<(String, String), OwnedView>,
    revision: u64,
}
impl ContributionState {
    pub fn register(&mut self, plugin: &str, contributions: &Contributions) -> Result<()> {
        super::installer::validate_name(plugin)?;
        validate(contributions)?;
        self.remove_owner(plugin);
        for view in &contributions.views {
            self.revision += 1;
            self.views.insert(
                (plugin.into(), view.id.clone()),
                OwnedView {
                    plugin: plugin.into(),
                    view: view.clone(),
                    revision: self.revision,
                },
            );
        }
        Ok(())
    }
    pub fn views(&self) -> Vec<OwnedView> {
        self.views.values().cloned().collect()
    }
    pub fn update_view(&mut self, plugin: &str, view: View, expected_revision: u64) -> Result<u64> {
        view.validate()?;
        let key = (plugin.to_owned(), view.id.clone());
        let old = self
            .views
            .get(&key)
            .context("view is not registered by this extension")?;
        ensure!(
            old.revision == expected_revision,
            "stale extension view revision"
        );
        self.revision += 1;
        self.views.insert(
            key,
            OwnedView {
                plugin: plugin.into(),
                view,
                revision: self.revision,
            },
        );
        Ok(self.revision)
    }
    pub fn remove_view(&mut self, plugin: &str, id: &str) -> Result<()> {
        ensure!(
            self.views
                .remove(&(plugin.to_owned(), id.to_owned()))
                .is_some(),
            "view is not registered by this extension"
        );
        Ok(())
    }
    pub fn remove_owner(&mut self, plugin: &str) {
        self.views.retain(|(owner, _), _| owner != plugin);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Workspace,
}
/// Layout-owned roots keep global and workspace overrides outside source trees.
pub struct SettingsStore {
    global: PathBuf,
    workspace: PathBuf,
    user: PathBuf,
}
impl SettingsStore {
    pub fn new(layout: &Layout) -> Self {
        Self {
            global: layout.user_dir.join("plugins/.settings"),
            workspace: layout.project_dir.join("extension-settings"),
            user: layout.user_dir.clone(),
        }
    }
    pub fn register_schema(&self, plugin: &str, schema: &Value) -> Result<()> {
        check_settings_schema(schema)?;
        super::installer::validate_name(plugin)?;
        write(
            &self.global.join("schemas").join(format!("{plugin}.json")),
            &serde_json::to_vec(schema)?,
        )
    }
    pub fn schema(&self, plugin: &str) -> Result<Value> {
        super::installer::validate_name(plugin)?;
        let bytes = crate::platform::atomic::read_private(
            &self.global.join("schemas").join(format!("{plugin}.json")),
        )?
        .context("no registered settings schema; initialize the extension first")?;
        let schema = serde_json::from_slice(&bytes)?;
        check_settings_schema(&schema)?;
        Ok(schema)
    }
    fn path(&self, plugin: &str, scope: SettingsScope) -> Result<PathBuf> {
        super::installer::validate_name(plugin)?;
        Ok(match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Workspace => &self.workspace,
        }
        .join(format!("{plugin}.json")))
    }
    /// Reads only the selected override document, without defaults or the other scope.
    /// This permits editors to complete required fields before validating on submission.
    pub fn layer(&self, plugin: &str, scope: SettingsScope) -> Result<Value> {
        let value = match crate::platform::atomic::read_private(&self.path(plugin, scope)?)? {
            Some(bytes) => {
                serde_json::from_slice(&bytes).context("invalid extension settings file")?
            }
            None => json!({}),
        };
        ensure!(value.is_object(), "settings override must be an object");
        Ok(value)
    }
    pub fn load(&self, plugin: &str, schema: &Value) -> Result<Value> {
        self.load_with(plugin, schema, None)
    }
    fn load_with(
        &self,
        plugin: &str,
        schema: &Value,
        replacement: Option<(SettingsScope, &Value)>,
    ) -> Result<Value> {
        check_settings_schema(schema)?;
        let mut values = defaults(schema);
        for scope in [SettingsScope::Global, SettingsScope::Workspace] {
            let layer = if replacement
                .as_ref()
                .is_some_and(|(selected, _)| *selected == scope)
            {
                replacement.as_ref().unwrap().1.clone()
            } else {
                self.layer(plugin, scope)?
            };
            ensure!(layer.is_object(), "settings override must be an object");
            ensure_no_secrets(schema, &layer)?;
            values
                .as_object_mut()
                .unwrap()
                .extend(layer.as_object().unwrap().clone());
        }
        validate_values(schema, &values)?;
        Ok(values)
    }
    pub fn set(
        &self,
        plugin: &str,
        schema: &Value,
        scope: SettingsScope,
        values: Value,
    ) -> Result<()> {
        check_settings_schema(schema)?;
        ensure!(values.is_object(), "settings must be an object");
        ensure_no_secrets(schema, &values)?;
        self.load_with(plugin, schema, Some((scope, &values)))?;
        let path = self.path(plugin, scope)?;
        write(&path, &serde_json::to_vec_pretty(&values)?)
    }
    /// Change one key without copying effective workspace values into the global layer.
    pub fn patch(
        &self,
        plugin: &str,
        schema: &Value,
        scope: SettingsScope,
        key: &str,
        value: Value,
    ) -> Result<()> {
        check_settings_schema(schema)?;
        let path = self.path(plugin, scope)?;
        let _lock = crate::config::settings_store::try_locked(&path)?;
        let mut layer: Value = match crate::platform::atomic::read_private(&path)? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => json!({}),
        };
        layer
            .as_object_mut()
            .context("settings override must be an object")?
            .insert(key.to_owned(), value);
        self.load_with(plugin, schema, Some((scope, &layer)))?;
        crate::platform::atomic::write_private(&path, &serde_json::to_vec_pretty(&layer)?)
    }
    pub fn reset(&self, plugin: &str, scope: SettingsScope) -> Result<()> {
        remove(&self.path(plugin, scope)?)
    }
    pub fn export(&self, plugin: &str, schema: &Value) -> Result<Value> {
        let value = self.load(plugin, schema)?;
        ensure_no_secrets(schema, &value)?;
        Ok(value)
    }
    /// Secrets use the existing locked credential owner, never extension manifests/settings files.
    pub fn set_secret(&self, plugin: &str, key: &str, value: &str) -> Result<()> {
        let credential = secret_key(plugin, key)?;
        let index = self
            .global
            .join("secret-index")
            .join(format!("{plugin}.json"));
        let _lock = crate::config::settings_store::try_locked(&index)?;
        let mut keys: std::collections::BTreeSet<String> =
            match crate::platform::atomic::read_private(&index)? {
                Some(bytes) => serde_json::from_slice(&bytes)?,
                None => Default::default(),
            };
        keys.insert(key.to_owned());
        ensure!(keys.len() <= 256, "extension secret limit is 256");
        crate::platform::atomic::write_private(&index, &serde_json::to_vec(&keys)?)?;
        crate::llm::catalog::save_credential(&self.user, &credential, value)
    }
    pub fn secret(&self, plugin: &str, key: &str) -> Result<Option<String>> {
        crate::llm::catalog::credential_token(&self.user, &secret_key(plugin, key)?)
    }
    pub fn remove_secret(&self, plugin: &str, key: &str) -> Result<()> {
        crate::llm::catalog::remove_credential(&self.user, &secret_key(plugin, key)?)
    }
    /// Purges this extension's tracked credentials and this workspace/global overrides only.
    pub fn purge(&self, plugin: &str) -> Result<()> {
        super::installer::validate_name(plugin)?;
        let index = self
            .global
            .join("secret-index")
            .join(format!("{plugin}.json"));
        let _lock = crate::config::settings_store::try_locked(&index)?;
        if let Some(bytes) = crate::platform::atomic::read_private(&index)? {
            let keys: std::collections::BTreeSet<String> = serde_json::from_slice(&bytes)?;
            for key in keys {
                self.remove_secret(plugin, &key)?;
            }
        }
        self.reset(plugin, SettingsScope::Global)?;
        self.reset(plugin, SettingsScope::Workspace)?;
        remove(&self.global.join("schemas").join(format!("{plugin}.json")))?;
        remove(&index)
    }
}
fn secret_key(plugin: &str, key: &str) -> Result<String> {
    super::installer::validate_name(plugin)?;
    validate_id(key)?;
    Ok(format!("https://extensions.dial.invalid/{plugin}/{key}"))
}
fn defaults(schema: &Value) -> Value {
    let mut result = serde_json::Map::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, property) in properties {
            if !secret_property(property)
                && let Some(value) = property.get("default")
            {
                result.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(result)
}
fn secret_property(schema: &Value) -> bool {
    schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || schema.get("format").and_then(Value::as_str) == Some("password")
}
fn ensure_no_secrets(schema: &Value, value: &Value) -> Result<()> {
    ensure!(
        !secret_property(schema),
        "secret setting must use credential storage"
    );
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            ensure_no_secrets(&schema["properties"][key], value)?;
        }
    }
    if let Some(items) = value.as_array() {
        for value in items {
            ensure_no_secrets(&schema["items"], value)?;
        }
    }
    Ok(())
}
/// Checks a possibly incomplete settings draft before presentation without requiring fields.
pub fn validate_public_values(schema: &Value, values: &Value) -> Result<()> {
    check_settings_schema(schema)?;
    ensure_no_secrets(schema, values)
}

pub fn validate_values(schema: &Value, values: &Value) -> Result<()> {
    validate_public_values(schema, values)?;
    // Secret required fields are credentials, not values in the settings document.
    let mut public = schema.clone();
    if let Some(required) = public.get("required").and_then(Value::as_array).cloned() {
        public["required"] = json!(
            required
                .into_iter()
                .filter(|v| !secret_property(&schema["properties"][v.as_str().unwrap_or_default()]))
                .collect::<Vec<_>>()
        );
    }
    schema::validate(&public, values)
        .map_err(|_| anyhow::anyhow!("extension settings do not match declared schema"))
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let _lock = crate::config::settings_store::try_locked(path)?;
    crate::platform::atomic::write_private(path, bytes)
}
fn remove(path: &Path) -> Result<()> {
    crate::platform::atomic::validate_private_path(
        path.parent().context("settings have no parent")?,
    )?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
#[cfg(test)]
mod tests;
