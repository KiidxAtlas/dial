//! Applies plugin actions at the idle boundary. Mutating actions wait for an explicit answer that
//! names the plugin and the exact change. Plugin text is sanitized before anyone sees it, and
//! provider secrets never pass through this path: keys are referenced by environment variable name.

use crate::agent::Agent;
use crate::agent::modes::switch::{parse_effort, resolve};
use crate::app::selection::provider_changes;
use crate::app::{App, Shell, Step};
use crate::config::settings_store::{Change, Scope};
use crate::llm::model_config::ProviderSpec;
use crate::tools::integrations::plugin::protocol::{
    Action, EntryRenderer, sanitize_block, sanitize_line, valid_name,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Session record type for plugin transcript entries. Model context never reads it.
pub(in crate::app) const PLUGIN_ENTRY_TYPE: &str = "plugin.entry";
/// Longest plugin message shown inside an approval prompt.
const PROMPT_CHARS: usize = 600;

/// What draining the plugin queue changed, so the idle loop knows whether to repaint or rebuild.
pub(in crate::app) enum Drained {
    Nothing,
    Changed,
    Rebuild(Step),
}

impl Shell {
    /// Applies queued plugin actions at an idle boundary. A rebuild stops the drain. The actions
    /// after it were never approved, so dropping them is safe, and the user is told.
    pub(in crate::app) fn drain_plugin_effects(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
    ) -> Drained {
        let Some(host) = agent.extension_host() else {
            return Drained::Nothing;
        };
        let effects = host.take_effects();
        if effects.is_empty() {
            return Drained::Nothing;
        }
        let total = effects.len();
        for (index, effect) in effects.into_iter().enumerate() {
            match self.apply_plugin_effect(app, agent, &effect.plugin, effect.action) {
                Ok(None) => {}
                Ok(Some(step)) => {
                    let dropped = total - index - 1;
                    if dropped > 0 {
                        self.note(
                            "Plugins",
                            format!(
                                "{dropped} queued plugin action(s) were dropped by a settings rebuild; plugins may send them again."
                            ),
                        );
                    }
                    return Drained::Rebuild(step);
                }
                Err(error) => self.note("Plugin action", format!("{}: {error:#}", effect.plugin)),
            }
        }
        Drained::Changed
    }

    fn apply_plugin_effect(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        plugin: &str,
        action: Action,
    ) -> Result<Option<Step>> {
        if let Some((title, detail)) = approval_prompt(plugin, &action)? {
            if !self.approve(&title, &detail) {
                self.note("Plugin action", format!("{plugin}: declined."));
                return Ok(None);
            }
        }
        match action {
            Action::Ui(_) => Ok(None),
            Action::SendUserMessage { text } => {
                self.deferred.push_back(sanitize_block(&text));
                self.note("Plugin action", format!("{plugin}: queued a message."));
                Ok(None)
            }
            Action::AppendEntry {
                kind,
                text,
                renderer,
            } => {
                self.append_plugin_entry(agent, plugin, &kind, &text, renderer.as_ref());
                Ok(None)
            }
            Action::SetModel { model, effort } => {
                switch_model(agent, &model, effort.as_deref())?;
                self.note(
                    "Plugin action",
                    format!("{plugin}: session-only model {model}."),
                );
                Ok(None)
            }
            Action::SetThinking { effort } => {
                set_thinking(agent, &effort)?;
                self.note(
                    "Plugin action",
                    format!("{plugin}: session-only thinking effort {effort}."),
                );
                Ok(None)
            }
            Action::RegisterProvider { name, config } => {
                let spec = parse_provider(&name, &config)?;
                let changes = provider_changes(&name, &spec);
                let step = self.save(app, Some(agent), &changes, Scope::Global)?;
                Ok(rebuild_only(step))
            }
            Action::UnregisterProvider { name } => {
                ensure_provider_name(&name)?;
                let changes = [Change::Remove {
                    path: format!("providers.{name}"),
                }];
                let step = self.save(app, Some(agent), &changes, Scope::Global)?;
                Ok(rebuild_only(step))
            }
        }
    }

    fn approve(&mut self, title: &str, detail: &str) -> bool {
        self.ask_with_cancel(title, detail, &[("Allow once", 'y'), ("Deny", 'n')], None) == Some(0)
    }

    /// Records a plugin entry in the session (never in model context) and shows it through its
    /// declarative renderer. A session that cannot record still shows the entry.
    fn append_plugin_entry(
        &mut self,
        agent: &mut Agent<'_>,
        plugin: &str,
        kind: &str,
        text: &str,
        renderer: Option<&EntryRenderer>,
    ) {
        let text = sanitize_block(text);
        let data = json!({
            "plugin": plugin,
            "kind": kind,
            "text": text,
            "renderer": renderer.map(renderer_json),
        });
        if let Err(error) = agent
            .session_mut()
            .append_custom_entry(PLUGIN_ENTRY_TYPE, data.clone())
        {
            self.note(
                "Plugin action",
                format!("{plugin}: entry not recorded in the session: {error:#}"),
            );
        }
        if let Some((title, body)) = entry_note(&data) {
            self.note(&title, body);
        }
    }
}

/// The approval prompt for an action that changes state, or `None` for display-only actions.
/// Invalid provider config is rejected here, before anyone is asked to approve it.
fn approval_prompt(plugin: &str, action: &Action) -> Result<Option<(String, String)>> {
    let (what, detail) = match action {
        Action::SendUserMessage { text } => (
            "send a message",
            format!("Message:\n{}", clip(&sanitize_block(text))),
        ),
        Action::SetModel { model, effort } => {
            let mut detail = format!("Model: {}", sanitize_line(model));
            if let Some(effort) = effort {
                detail.push_str(&format!("\nEffort: {}", sanitize_line(effort)));
            }
            ("switch the session model", detail)
        }
        Action::SetThinking { effort } => (
            "change the thinking effort",
            format!("Effort: {}", sanitize_line(effort)),
        ),
        Action::RegisterProvider { name, config } => {
            let spec = parse_provider(name, config)?;
            ("register a provider", provider_detail(name, config, &spec))
        }
        Action::UnregisterProvider { name } => {
            ensure_provider_name(name)?;
            ("remove a provider", format!("Provider: {name}"))
        }
        Action::Ui(_) | Action::AppendEntry { .. } => return Ok(None),
    };
    Ok(Some((
        format!("Allow plugin \"{plugin}\" to {what}?"),
        detail,
    )))
}

/// Parses plugin-declared provider config. Keys are referenced by environment variable name only;
/// anything else is refused, so a raw secret never reaches the settings file. Serde's own errors
/// can quote the offending value, so only a generic message is reported.
fn parse_provider(name: &str, config: &Value) -> Result<ProviderSpec> {
    ensure_provider_name(name)?;
    let spec: ProviderSpec = serde_json::from_value(config.clone())
        .map_err(|_| anyhow::anyhow!("provider config does not match the provider schema"))?;
    for env in spec.api_key_env.iter().chain(&spec.api_key_envs) {
        anyhow::ensure!(
            is_env_name(env),
            "API key must be an environment variable name, not a value"
        );
    }
    Ok(spec)
}

/// Provider summary for the prompt. Header values are hidden; only their names are shown.
fn provider_detail(name: &str, config: &Value, spec: &ProviderSpec) -> String {
    let mut lines = vec![
        format!("Provider: {name}"),
        format!("Base URL: {}", sanitize_line(&spec.base_url)),
    ];
    if let Some(env) = &spec.api_key_env {
        lines.push(format!("API key variable: {env}"));
    }
    if let Some(headers) = config.get("headers").and_then(Value::as_object) {
        let names: Vec<String> = headers.keys().map(|key| sanitize_line(key)).collect();
        lines.push(format!("Headers: {} (values hidden)", names.join(", ")));
    }
    lines.join("\n")
}

fn ensure_provider_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        valid_name(name) && !name.contains('.'),
        "provider name is not a valid identifier"
    );
    Ok(())
}

/// Environment variable names: uppercase ASCII, digits and underscores, not starting with a digit.
fn is_env_name(value: &str) -> bool {
    value.len() <= 64
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && value
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn switch_model(agent: &mut Agent<'_>, model: &str, effort: Option<&str>) -> Result<()> {
    let selection = resolve(agent.effective_config(), model)?;
    let effort = effort
        .map(|name| parse_effort(name).with_context(|| format!("unknown effort {name:?}")))
        .transpose()?;
    agent.switch_model(&selection, effort)
}

fn set_thinking(agent: &mut Agent<'_>, effort: &str) -> Result<()> {
    let effort = parse_effort(effort).with_context(|| format!("unknown effort {effort:?}"))?;
    let selection = agent.effective_config().selection();
    agent.switch_model(&selection, Some(effort))
}

/// Keeps only a settings rebuild; a save that changed nothing the idle loop must act on is `None`.
fn rebuild_only(step: Step) -> Option<Step> {
    matches!(step, Step::Rebuild(_)).then_some(step)
}

fn renderer_json(view: &EntryRenderer) -> Value {
    json!({
        "title": view.title,
        "fields": view.fields.iter().map(|(label, value)| json!([label, value])).collect::<Vec<_>>(),
        "markdown": view.markdown,
    })
}

/// Transcript title and body of a stored `plugin.entry` record. The live path and resume replay both
/// render through here, so a resumed transcript shows what was shown when the entry was added. Text
/// is sanitized again on the way out, because stored records are not trusted to be clean.
pub(in crate::app) fn entry_note(data: &Value) -> Option<(String, String)> {
    let plugin = data.get("plugin")?.as_str()?;
    let kind = data.get("kind")?.as_str()?;
    let text = data.get("text")?.as_str()?;
    let view = data.get("renderer").filter(|value| !value.is_null());
    let declared = |key: &str| view.and_then(|value| value.get(key));
    let title = match declared("title").and_then(Value::as_str) {
        Some(title) => sanitize_line(title),
        None => format!("{plugin}: {kind}"),
    };
    let mut parts: Vec<String> = declared("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let label = sanitize_line(pair.get(0)?.as_str()?);
            let value = sanitize_line(pair.get(1)?.as_str()?);
            Some(format!("{label}: {value}"))
        })
        .collect();
    if let Some(markdown) = declared("markdown").and_then(Value::as_str) {
        parts.push(sanitize_block(markdown));
    }
    let body = if parts.is_empty() {
        sanitize_block(text)
    } else {
        parts.join("\n")
    };
    Some((title, body))
}

fn clip(text: &str) -> String {
    if text.chars().count() <= PROMPT_CHARS {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(PROMPT_CHARS).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests;
