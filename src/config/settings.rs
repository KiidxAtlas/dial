//! Immutable validated settings snapshots and typed handles. Serialized keys remain compatible;
//! generation changes are published only after complete validation, never for a failed reload.
use super::{Config, schema};
use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    marker::PhantomData,
    sync::{
        Arc, Weak,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
};
use toml::{Table, Value};

pub struct Setting<T> {
    path: &'static str,
    value: PhantomData<fn() -> T>,
}
impl<T> Copy for Setting<T> {}
impl<T> Clone for Setting<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Setting<T> {
    pub const fn new(path: &'static str) -> Self {
        Self {
            path,
            value: PhantomData,
        }
    }
    pub fn path(self) -> &'static str {
        self.path
    }
    pub fn definition(self) -> Result<&'static schema::SettingDef> {
        schema::registry()
            .iter()
            .find(|definition| definition.path == self.path)
            .with_context(|| format!("setting {} is not declared", self.path))
    }
}
impl<T: DeserializeOwned> Setting<T> {
    pub fn get(self, snapshot: &Snapshot) -> Result<T> {
        let definition = self.definition()?;
        let value = schema::get(&snapshot.values, self.path)
            .cloned()
            .or_else(|| definition.default_value())
            .context("setting has no effective value")?;
        value
            .try_into()
            .with_context(|| format!("setting {} has an incompatible handle type", self.path))
    }
}
impl<T: Serialize> Setting<T> {
    pub fn change(self, value: T) -> Result<crate::storage::settings_store::Change> {
        self.definition()?;
        Ok(crate::storage::settings_store::Change::Set {
            path: self.path.into(),
            value: Value::try_from(value)?,
        })
    }
}

#[derive(Clone)]
pub struct Snapshot {
    config: Arc<Config>,
    values: Table,
    pub revision: u64,
    digest: [u8; 32],
}
impl Snapshot {
    pub fn new(config: Config, revision: u64) -> Result<Self> {
        config.validate()?;
        let values = Table::try_from(&config)?;
        let digest = Sha256::digest(toml::to_string(&values)?.as_bytes()).into();
        Ok(Self {
            config: Arc::new(config),
            values,
            revision,
            digest,
        })
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn values(&self) -> &Table {
        &self.values
    }
    pub fn changed(&self, previous: &Self) -> Vec<&'static schema::SettingDef> {
        schema::registry()
            .iter()
            .filter(|definition| {
                schema::get(&self.values, definition.path)
                    != schema::get(&previous.values, definition.path)
            })
            .collect()
    }
}

/// Subscription lifetime is the returned receiver's owner. Notifications contain revisions only;
/// consumers always read the latest snapshot rather than applying stale intermediate values.
pub struct Settings {
    current: Snapshot,
    listeners: Vec<Weak<SyncSender<u64>>>,
}
pub struct Subscription {
    _sender: Arc<SyncSender<u64>>,
    pub revisions: Receiver<u64>,
}
impl Settings {
    pub fn new(config: Config) -> Result<Self> {
        Ok(Self {
            current: Snapshot::new(config, 0)?,
            listeners: Vec::new(),
        })
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.current
    }
    pub fn subscribe(&mut self) -> Subscription {
        let (sender, revisions) = mpsc::sync_channel(1);
        let sender = Arc::new(sender);
        self.listeners.push(Arc::downgrade(&sender));
        Subscription {
            _sender: sender,
            revisions,
        }
    }
    pub fn publish(&mut self, config: Config) -> Result<bool> {
        let revision = self
            .current
            .revision
            .checked_add(1)
            .context("settings revision exhausted")?;
        let candidate = Snapshot::new(config, revision)?;
        if candidate.digest == self.current.digest {
            // Nonserialized execution context may change without a settings revision.
            self.current.config = candidate.config;
            return Ok(false);
        }
        self.current = candidate;
        self.listeners.retain(|listener| {
            let Some(sender) = listener.upgrade() else {
                return false;
            };
            !matches!(
                sender.try_send(revision),
                Err(TrySendError::Disconnected(_))
            )
        });
        Ok(true)
    }
}

/// Generated UI/CLI/documentation view; defaults always come from the real Config authority.
pub fn reference() -> serde_json::Value {
    serde_json::json!({"schema_version": 1, "settings": schema::registry().iter().map(|definition| {
        serde_json::json!({"path": definition.path, "domain": definition.path.split('.').next(),
            "label": definition.label, "description": definition.description,
            "tab": definition.tab, "section": definition.section,
            "kind": format!("{:?}", definition.kind), "constraints":definition.constraints(), "default": definition.default_value(),
            "restart": definition.restart})
    }).collect::<Vec<_>>(), "resources":crate::tools::resources::DESCRIPTORS,
    "native_tools":crate::tools::schemas(crate::tools::BUILTIN, crate::config::types::EditMode::default()).expect("built-in schemas are valid"),
    "artifact_tool":crate::tools::artifact::schema()})
}
pub fn reference_markdown() -> String {
    let mut out =
        "# Dial settings reference\n\nGenerated by `dial config reference`.\n\n".to_owned();
    for definition in schema::registry() {
        out.push_str(&format!(
            "## `{}`\n\n{}\n\nDefault: `{}`. {}\n\n",
            definition.path,
            definition.description,
            definition
                .default_value()
                .map_or("unset".into(), |value| value.to_string()),
            if definition.restart {
                "Requires a new session."
            } else {
                "Applies at the next safe configuration boundary."
            }
        ));
    }
    out
}

#[cfg(test)]
mod tests;
