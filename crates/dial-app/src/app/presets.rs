//! Named model presets (`model_presets.<name>`): a saved Roles tab, the Conversation model and every
//! helper role with its thinking level. The Roles tab's preset rows and `/modelpreset` both create,
//! update, apply, rename and delete presets through these owners; neither writes presets itself.

use crate::app::selection::main_thinking_changes;
use crate::config::paths::Layout;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::{Config, ModelPreset};
use crate::llm::model_config::role_label;
use anyhow::{Result, anyhow, ensure};

/// What a valid preset name looks like, for refusals.
pub(crate) const NAME_RULE: &str =
    "names start with a letter, then letters, digits, '-' or '_' (no spaces)";

pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_name(name: &str) -> Result<()> {
    ensure!(
        valid_name(name),
        "invalid preset name {name:?}: {NAME_RULE}"
    );
    Ok(())
}

/// How saving treats an existing preset of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Save {
    /// A new preset; an existing name is refused.
    Create,
    /// Replace an existing preset's models with the current ones; a missing name is refused.
    Update,
    /// Create or replace (the legacy `/modelpreset save`).
    Overwrite,
}

/// The current Roles tab as a preset: the main model and its thinking level, each assigned helper
/// role with its level, the vision selection and the Laya checkpoint.
pub(crate) fn snapshot(config: &Config) -> ModelPreset {
    let mut main = config.selection();
    main.thinking = crate::config::thinking::main_level(config);
    ModelPreset {
        main: Some(main),
        roles: config.model_roles.clone(),
        vision: Some(config.vision.clone()),
        laya_checkpoint: config.laya.checkpoint.clone(),
        ..Default::default()
    }
}

/// Settings writes that save the current roles as preset `name` (global).
pub(crate) fn save_changes(config: &Config, name: &str, mode: Save) -> Result<Vec<Change>> {
    check_name(name)?;
    let existing = config.model_presets.get(name);
    match mode {
        Save::Create => ensure!(
            existing.is_none(),
            "a preset named {name} already exists; update or rename it instead"
        ),
        Save::Update => ensure!(existing.is_some(), "preset not found: {name}"),
        Save::Overwrite => {}
    }
    let mut preset = snapshot(config);
    // Updating a captured working setup keeps its controls; only the models are replaced.
    if let Some(existing) = existing {
        preset.setup = existing.setup.clone();
    }
    let path = format!("model_presets.{name}");
    let value = toml::Value::try_from(&preset)?;
    Ok(vec![if mode == Save::Create {
        Change::Insert { path, value }
    } else {
        Change::Set { path, value }
    }])
}

/// Settings writes that apply `preset`: the main model with its level, the helper roles (each with
/// its own level), vision, the Laya checkpoint and any captured working controls.
pub(crate) fn apply_changes(config: &Config, preset: &ModelPreset) -> Result<Vec<Change>> {
    preset.validate_setup()?;
    let mut changes = Vec::new();
    if let Some(main) = &preset.main {
        let mut plain = main.clone();
        plain.thinking = None;
        changes.extend(crate::config::selection::changes(config, &plain, "main")?);
        let level = main.thinking.as_deref().unwrap_or("default");
        changes.extend(main_thinking_changes(
            &config.with_selection(&plain),
            level,
        )?);
    }
    // Safe captures contain no model fields. Model presets restore their roles.
    if preset.setup.is_empty() || !preset.roles.is_empty() {
        changes.push(Change::Set {
            path: "model_roles".into(),
            value: toml::Value::try_from(&preset.roles)?,
        });
    }
    if let Some(vision) = &preset.vision {
        changes.push(Change::Set {
            path: "vision".into(),
            value: toml::Value::try_from(vision)?,
        });
    }
    if let Some(checkpoint) = &preset.laya_checkpoint {
        changes.push(Change::Set {
            path: "laya.checkpoint".into(),
            value: checkpoint.clone().into(),
        });
    }
    // Presets saved before `main` existed carry only the active profile's level.
    if preset.main.is_none()
        && let Some(effort) = preset.thinking
        && let Some(active) = config.active_model_profile()
    {
        let name = config
            .model_profiles
            .iter()
            .find(|(_, profile)| std::ptr::eq(*profile, active))
            .map(|(name, _)| name)
            .ok_or_else(|| anyhow!("active model profile is not in model_profiles"))?;
        changes.push(Change::Set {
            path: format!("model_profiles.{name}.reasoning_effort"),
            value: toml::Value::try_from(effort)?,
        });
    }
    changes.extend(preset.setup.iter().map(|(path, value)| Change::Set {
        path: path.clone(),
        value: value.clone(),
    }));
    Ok(changes)
}

/// Settings writes that apply the saved preset `name`.
pub(crate) fn apply_named(config: &Config, name: &str) -> Result<Vec<Change>> {
    let preset = config
        .model_presets
        .get(name)
        .ok_or_else(|| anyhow!("preset not found: {name}"))?;
    apply_changes(config, preset)
}

/// Presets written in the project file are edited there; the Roles tab and command save globally.
fn refuse_project_preset(layout: &Layout, name: &str) -> Result<()> {
    let project = settings_store::load_layer(layout, Scope::Project)?;
    ensure!(
        !project
            .get("model_presets")
            .and_then(|value| value.as_table())
            .is_some_and(|table| table.contains_key(name)),
        "preset {name} is defined in the project config; change it there"
    );
    Ok(())
}

/// Settings writes that rename preset `from` to `to`, keeping its contents.
pub(crate) fn rename_changes(
    layout: &Layout,
    config: &Config,
    from: &str,
    to: &str,
) -> Result<Vec<Change>> {
    check_name(to)?;
    let preset = config
        .model_presets
        .get(from)
        .ok_or_else(|| anyhow!("preset not found: {from}"))?;
    ensure!(from != to, "the preset is already named {to}");
    ensure!(
        !config.model_presets.contains_key(to),
        "a preset named {to} already exists"
    );
    refuse_project_preset(layout, from)?;
    Ok(vec![
        Change::Insert {
            path: format!("model_presets.{to}"),
            value: toml::Value::try_from(preset)?,
        },
        Change::Remove {
            path: format!("model_presets.{from}"),
        },
    ])
}

/// Settings writes that delete preset `name`.
pub(crate) fn delete_changes(layout: &Layout, config: &Config, name: &str) -> Result<Vec<Change>> {
    ensure!(
        config.model_presets.contains_key(name),
        "preset not found: {name}"
    );
    refuse_project_preset(layout, name)?;
    Ok(vec![Change::Remove {
        path: format!("model_presets.{name}"),
    }])
}

/// One line per role a preset sets, as `/modelpreset list` shows it.
pub(crate) fn describe(preset: &ModelPreset) -> Vec<String> {
    let row = |role: &str, selection: &crate::llm::model_config::ModelSelection| {
        let level = selection
            .thinking
            .as_deref()
            .map(|level| format!(" · thinking {level}"))
            .unwrap_or_default();
        format!("{}: {}{level}", role_label(role), selection.display_name())
    };
    let mut rows: Vec<String> = preset.main.iter().map(|main| row("main", main)).collect();
    rows.extend(
        preset
            .roles
            .iter()
            .map(|(role, selection)| row(role, selection)),
    );
    if rows.is_empty() && !preset.setup.is_empty() {
        rows.push(format!(
            "working setup: {} control(s), no models",
            preset.setup.len()
        ));
    }
    rows
}

#[cfg(test)]
mod tests;
