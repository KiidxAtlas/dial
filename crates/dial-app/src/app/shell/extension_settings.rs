//! Schema-backed extension settings forms. The same settings owner validates/persists CLI and UI
//! writes; secret properties never enter a terminal input or exported public settings.
use super::Shell;
use crate::agent::{Agent, Ui};
use crate::app::{App, Bridge, PAINT_EVERY};
use crate::extensions::panel::{Element, Outcome, Panel, Value as InputValue};
use crate::tools::integrations::plugin::contributions::{SettingsScope, SettingsStore};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, time::Instant};

struct Field {
    id: String,
    name: String,
    kind: String,
    choices: Option<Vec<Value>>,
}
struct Form {
    panel: Panel,
    fields: Vec<Field>,
    values: Map<String, Value>,
}
impl Form {
    fn new(plugin: &str, schema: &Value, values: Value) -> Result<Self> {
        crate::tools::integrations::plugin::contributions::validate_public_values(schema, &values)?;
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .context("extension settings schema has no properties")?;
        ensure!(
            properties.len() <= 48,
            "settings form supports at most 48 properties"
        );
        let values = values.as_object().cloned().unwrap_or_default();
        let mut fields = Vec::new();
        let mut elements = Vec::new();
        for (name, property) in properties {
            if property.get("writeOnly").and_then(Value::as_bool) == Some(true)
                || property.get("format").and_then(Value::as_str) == Some("password")
            {
                elements.push(Element::Text(format!(
                    "{name}: secret (stored separately; not displayed)"
                )));
                continue;
            }
            let id = format!("f{}", fields.len());
            let label = crate::ui::text::one_line(
                property
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(name),
            );
            let kind = property
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("string")
                .to_owned();
            let current = values.get(name).or_else(|| property.get("default"));
            let choices = property
                .get("enum")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| (kind == "boolean").then(|| vec![json!(false), json!(true)]));
            let element = if let Some(choices) = &choices {
                ensure!(
                    !choices.is_empty() && choices.len() <= 32,
                    "setting {name} has too many or no choices"
                );
                Element::Select {
                    id: id.clone(),
                    label,
                    options: choices
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .collect(),
                    selected: current
                        .and_then(|v| choices.iter().position(|c| c == v))
                        .unwrap_or(0),
                }
            } else {
                Element::Input {
                    id: id.clone(),
                    label,
                    value: current
                        .map(|v| {
                            if kind == "string" {
                                v.as_str().unwrap_or_default().into()
                            } else {
                                v.to_string()
                            }
                        })
                        .unwrap_or_default(),
                }
            };
            fields.push(Field {
                id,
                name: name.clone(),
                kind,
                choices,
            });
            elements.push(element);
        }
        elements.push(Element::Button {
            id: "save".into(),
            label: "Save settings".into(),
            primary: true,
        });
        let panel = Panel {
            title: format!("{plugin} settings"),
            generation: 1,
            elements,
        };
        panel.validate()?;
        Ok(Self {
            panel,
            fields,
            values,
        })
    }
    fn decode(&self, inputs: &BTreeMap<String, InputValue>) -> Result<Value> {
        self.panel.check_values(inputs)?;
        let mut values = self.values.clone();
        for field in &self.fields {
            let value = match inputs.get(&field.id) {
                Some(InputValue::Choice(index)) => field
                    .choices
                    .as_ref()
                    .and_then(|choices| choices.get(*index))
                    .cloned()
                    .context("invalid settings choice")?,
                Some(InputValue::Text(text)) if field.kind == "string" => json!(text),
                Some(InputValue::Text(text)) if text.trim().is_empty() => {
                    values.remove(&field.name);
                    continue;
                }
                Some(InputValue::Text(text)) => serde_json::from_str(text)
                    .with_context(|| format!("{} needs a valid {}", field.name, field.kind))?,
                _ => bail!("missing setting {}", field.name),
            };
            values.insert(field.name.clone(), value);
        }
        Ok(Value::Object(values))
    }
}
impl Shell {
    pub(in crate::app) fn extension_settings_command(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        args: &str,
    ) -> Result<String> {
        let host = agent.extension_host().context("no extensions loaded")?;
        let mut words = args.split_whitespace();
        let Some(plugin) = words.next().filter(|s| *s != "list") else {
            return Ok(host
                .loaded_names()
                .into_iter()
                .filter(|name| {
                    host.contributions(name)
                        .is_some_and(|c| c.settings.get("properties").is_some())
                })
                .collect::<Vec<_>>()
                .join("\n"));
        };
        let contributions = host
            .contributions(plugin)
            .context("extension is not loaded")?;
        let scope = match words.next().unwrap_or("global") {
            "global" => SettingsScope::Global,
            "workspace" => SettingsScope::Workspace,
            _ => bail!("expected global or workspace scope"),
        };
        let action = words.next().unwrap_or("edit");
        ensure!(
            words.next().is_none(),
            "usage: /extension-settings owner [global|workspace] [edit|export|reset]"
        );
        let store = SettingsStore::new(&app.layout);
        store.register_schema(plugin, &contributions.settings)?;
        if action == "reset" {
            store.reset(plugin, scope)?;
            return Ok(format!("Reset {plugin} settings in selected scope."));
        }
        if action == "export" {
            return Ok(serde_json::to_string_pretty(
                &store.export(plugin, &contributions.settings)?,
            )?);
        }
        ensure!(action == "edit", "expected edit, export or reset");
        let mut form = Form::new(plugin, &contributions.settings, store.layer(plugin, scope)?)?;
        loop {
            let outcome = {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                bridge.run_panel(&form.panel, &app.interrupt)
            };
            match outcome {
                Outcome::Cancelled => return Ok(String::new()),
                Outcome::Unsupported(reason) => bail!("settings form unavailable: {reason}"),
                Outcome::Submitted { values, .. } => {
                    let result = form.decode(&values).and_then(|values| {
                        store.set(plugin, &contributions.settings, scope, values.clone())?;
                        Ok(values)
                    });
                    match result {
                        Ok(_) => return Ok(format!("Saved {plugin} settings.")),
                        Err(error) => {
                            // Keep the submitted draft when validation fails, never silently coerce.
                            for element in &mut form.panel.elements {
                                match element {
                                    Element::Input { id, value, .. } => {
                                        if let Some(InputValue::Text(text)) = values.get(id) {
                                            value.clone_from(text);
                                        }
                                    }
                                    Element::Select { id, selected, .. } => {
                                        if let Some(InputValue::Choice(index)) = values.get(id) {
                                            *selected = *index;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            self.note("Extension settings", format!("{error:#}"));
                        }
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
