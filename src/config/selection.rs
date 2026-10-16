//! Explicit model selection writes shared by setup and interactive controllers.
use super::Config;
use crate::llm::model_config::{ModelSelection, ROLES};
use crate::storage::settings_store::Change;
use anyhow::{Result, ensure};

pub fn protocol(
    config: &Config,
    selection: &ModelSelection,
) -> crate::llm::model_config::ApiProtocol {
    selection
        .protocol
        .or_else(|| {
            config
                .providers
                .get(&selection.provider)
                .and_then(|provider| provider.protocol)
        })
        .unwrap_or_else(|| crate::llm::model_config::ApiProtocol::builtin(&selection.provider))
}

/// The settings writes that make `selection` the model for `role` (`main` or a helper role).
pub fn changes(config: &Config, selection: &ModelSelection, role: &str) -> Result<Vec<Change>> {
    ensure!(!selection.model.trim().is_empty(), "a model id is required");
    let mut changes = Vec::new();
    let set = |path: String, value: toml::Value| Change::Set { path, value };
    let protocol = protocol(config, selection);
    if role == "main" {
        changes.push(set("model".into(), selection.model.clone().into()));
        changes.push(set("provider".into(), selection.provider.clone().into()));
        if config.model_profile.as_ref().is_some_and(|name| {
            config.model_profiles.get(name).is_some_and(|profile| {
                profile.provider != selection.provider || profile.model != selection.model
            })
        }) {
            // An explicit empty marker clears a profile inherited from the other settings layer.
            changes.push(set("model_profile".into(), "".into()));
        }
        let remote = config.selection_base_url(selection).is_some();
        match &selection.base_url {
            Some(base) => {
                changes.push(set("endpoint.base_url".into(), base.clone().into()));
                // An empty variable name clears an inherited one (the loader treats it as unset).
                changes.push(set(
                    "endpoint.api_key_env".into(),
                    selection.api_key_env.clone().unwrap_or_default().into(),
                ));
                changes.push(set(
                    "endpoint.allow_insecure".into(),
                    selection.allow_insecure.into(),
                ));
                changes.push(set("endpoint.protocol".into(), protocol.as_str().into()));
                if !config.providers.contains_key(&selection.provider) {
                    let prefix = format!("providers.{}", selection.provider);
                    changes.push(set(
                        format!("{prefix}.label"),
                        selection.provider.clone().into(),
                    ));
                    changes.push(set(format!("{prefix}.base_url"), base.clone().into()));
                    if let Some(variable) = &selection.api_key_env {
                        changes.push(set(
                            format!("{prefix}.api_key_env"),
                            variable.clone().into(),
                        ));
                    }
                    changes.push(set(format!("{prefix}.enabled"), true.into()));
                    changes.push(set(
                        format!("{prefix}.allow_insecure"),
                        selection.allow_insecure.into(),
                    ));
                    changes.push(set(format!("{prefix}.protocol"), protocol.as_str().into()));
                }
            }
            None if remote => {
                let resolved = config.selection_endpoint(selection).unwrap();
                changes.push(set("endpoint.base_url".into(), resolved.base_url.into()));
                changes.push(set(
                    "endpoint.api_key_env".into(),
                    resolved.api_key_env.unwrap_or_default().into(),
                ));
                changes.push(set(
                    "endpoint.allow_insecure".into(),
                    resolved.allow_insecure.into(),
                ));
                changes.push(set(
                    "endpoint.protocol".into(),
                    resolved
                        .protocol
                        .unwrap_or_else(|| {
                            crate::llm::model_config::ApiProtocol::builtin(&selection.provider)
                        })
                        .as_str()
                        .into(),
                ));
            }
            None => changes.push(Change::Remove {
                path: "endpoint".into(),
            }),
        }
        if remote {
            changes.push(set(
                "endpoint.headers".into(),
                crate::llm::model_config::headers_setting(&selection.headers),
            ));
            changes.push(set(
                "endpoint.account_id".into(),
                selection.account_id.clone().unwrap_or_default().into(),
            ));
        }
        // Capacity is metadata, not a user override. An explicit Auto marker also clears
        // a numeric cap inherited from the other settings layer when switching models.
        changes.push(set("context_window".into(), "auto".into()));
    } else {
        ensure!(
            ROLES.iter().any(|(name, _)| *name == role) && role != "main",
            "unknown model role {role:?}"
        );
        let prefix = format!("model_roles.{role}");
        changes.push(set(
            format!("{prefix}.provider"),
            selection.provider.clone().into(),
        ));
        changes.push(set(
            format!("{prefix}.model"),
            selection.model.clone().into(),
        ));
        changes.push(set(format!("{prefix}.protocol"), protocol.as_str().into()));
        changes.push(set(
            format!("{prefix}.account_id"),
            selection.account_id.clone().unwrap_or_default().into(),
        ));
        changes.push(set(
            format!("{prefix}.headers"),
            crate::llm::model_config::headers_setting(&selection.headers),
        ));
        match &selection.base_url {
            Some(base) => {
                changes.push(set(format!("{prefix}.base_url"), base.clone().into()));
                changes.push(set(
                    format!("{prefix}.api_key_env"),
                    selection.api_key_env.clone().unwrap_or_default().into(),
                ));
                changes.push(set(
                    format!("{prefix}.allow_insecure"),
                    selection.allow_insecure.into(),
                ));
            }
            None => {
                if let Some(resolved) = config.selection_endpoint(selection) {
                    changes.push(set(format!("{prefix}.base_url"), resolved.base_url.into()));
                    changes.push(set(
                        format!("{prefix}.api_key_env"),
                        resolved.api_key_env.unwrap_or_default().into(),
                    ));
                    changes.push(set(
                        format!("{prefix}.allow_insecure"),
                        resolved.allow_insecure.into(),
                    ));
                } else {
                    changes.push(Change::Remove {
                        path: format!("{prefix}.base_url"),
                    });
                    changes.push(set(format!("{prefix}.api_key_env"), "".into()));
                    changes.push(set(format!("{prefix}.allow_insecure"), false.into()));
                }
            }
        }
        changes.push(set(format!("{prefix}.context_window"), "auto".into()));
    }
    Ok(changes)
}
