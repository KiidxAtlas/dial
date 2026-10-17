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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionModels(toml::Table);

impl SessionModels {
    /// Pin the model keys of a merged settings table (both files plus the command-line overlay).
    pub fn capture(merged: &toml::Table) -> Self {
        let mut models = Self::default();
        for key in pinned_keys() {
            copy(&mut models.0, merged, key);
        }
        models
    }

    /// Replace the model keys of `merged` with this session's values.
    pub(super) fn apply(&self, merged: &mut toml::Table) {
        for key in pinned_keys() {
            copy(merged, &self.0, key);
        }
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
                (Some(ROLES), Some(role)) => self.adopt_helper_role(merged, role),
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
            self.adopt_helper_role(merged, role);
        }
    }

    fn adopt_helper_role(&mut self, merged: &toml::Table, role: &str) {
        let value = merged
            .get(ROLES)
            .and_then(toml::Value::as_table)
            .and_then(|roles| roles.get(role))
            .cloned();
        let mut roles = match self.0.remove(ROLES) {
            Some(toml::Value::Table(roles)) => roles,
            _ => toml::Table::new(),
        };
        match value {
            Some(value) => {
                roles.insert(role.to_owned(), value);
            }
            None => {
                roles.remove(role);
            }
        }
        if !roles.is_empty() {
            self.0.insert(ROLES.to_owned(), toml::Value::Table(roles));
        }
    }
}

fn pinned_keys() -> impl Iterator<Item = &'static str> {
    MAIN_KEYS.iter().copied().chain([ROLES])
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

#[cfg(test)]
mod tests;
