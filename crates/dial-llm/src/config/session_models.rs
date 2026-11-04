//! Model choices owned by one running session.
//!
//! Every terminal runs its own Dial process over the same settings files. A model chosen in one
//! terminal is saved for future launches, but it must not retarget the other running sessions when
//! they reload shared settings. [`SessionModels`] keeps this process's raw values for the model keys
//! of the merged settings table; applying it replaces those keys (absent means removed) before the
//! normal loader runs, so endpoints, context windows and profiles derive exactly as at launch.

/// Top-level keys that make up the main model selection.
const MAIN_KEYS: &[&str] = &[
    "model",
    "provider",
    "endpoint",
    "model_profile",
    "context_window",
];
/// Helper-role assignments, pinned per role.
const ROLES: &str = "model_roles";
/// Explicit vision feature selections, pinned independently of chat roles.
const VISION: &str = "vision";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionModels(toml::Table);

impl SessionModels {
    /// Pin the model keys of a merged settings table (both files plus the command-line overlay).
    pub fn capture(merged: &toml::Table) -> Self {
        let mut models = Self::default();
        for key in pinned_keys() {
            copy(&mut models.0, merged, key);
        }
        copy_field(&mut models.0, merged, "laya", "checkpoint");
        models
    }

    /// Replace the model keys of `merged` with this session's values.
    pub(super) fn apply(&self, merged: &mut toml::Table) {
        for key in pinned_keys() {
            copy(merged, &self.0, key);
        }
        copy_field(merged, &self.0, "laya", "checkpoint");
    }

    /// Adopt the merged values at settings paths this session wrote itself, so its own choices
    /// apply here. Paths outside the model keys are ignored.
    pub fn adopt_paths<'a>(
        &mut self,
        merged: &toml::Table,
        paths: impl IntoIterator<Item = &'a str>,
    ) {
        for path in paths {
            let mut parts = path.split('.');
            match (parts.next(), parts.next()) {
                (Some(key), _) if MAIN_KEYS.contains(&key) => copy(&mut self.0, merged, key),
                (Some(ROLES), None) => copy(&mut self.0, merged, ROLES),
                (Some(ROLES), Some(role)) => copy_field(&mut self.0, merged, ROLES, role),
                (Some(VISION), None) => copy(&mut self.0, merged, VISION),
                (Some(VISION), Some(feature)) => copy_field(&mut self.0, merged, VISION, feature),
                (Some("laya"), None | Some("checkpoint")) => {
                    copy_field(&mut self.0, merged, "laya", "checkpoint");
                }
                _ => {}
            }
        }
    }

    /// Adopt the complete merged selection for `role` (`main` or a helper role) after this session
    /// assigned it.
    pub fn adopt_role(&mut self, merged: &toml::Table, role: &str) {
        if role == "main" {
            for key in MAIN_KEYS {
                copy(&mut self.0, merged, key);
            }
        } else {
            copy_field(&mut self.0, merged, ROLES, role);
        }
    }
}

fn pinned_keys() -> impl Iterator<Item = &'static str> {
    MAIN_KEYS.iter().copied().chain([ROLES, VISION])
}

/// Make `target[key]` equal `source[key]`, removing it when the source has none.
fn copy(target: &mut toml::Table, source: &toml::Table, key: &str) {
    match source.get(key) {
        Some(value) => {
            target.insert(key.to_owned(), value.clone());
        }
        None => {
            target.remove(key);
        }
    }
}

/// Replace one complete selection without adopting unrelated fields from another session.
fn copy_field(target: &mut toml::Table, source: &toml::Table, section: &str, key: &str) {
    let value = source
        .get(section)
        .and_then(|value| value.get(key))
        .cloned();
    let mut fields = match target.remove(section) {
        Some(toml::Value::Table(fields)) => fields,
        _ => toml::Table::new(),
    };
    match value {
        Some(value) => {
            fields.insert(key.to_owned(), value);
        }
        None => {
            fields.remove(key);
        }
    }
    if !fields.is_empty() {
        target.insert(section.to_owned(), toml::Value::Table(fields));
    }
}

#[cfg(test)]
mod tests;
