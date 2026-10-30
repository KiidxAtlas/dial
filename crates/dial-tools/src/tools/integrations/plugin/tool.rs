//! Plugin tool declarations and adapters over the ordinary approved tool registry.
use super::Host;
use crate::tools::{
    ToolOutput,
    integrations::ext::{Tool, ToolCallback, ToolContext, ToolEvent},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// A plugin-local tool declaration. Revisions describe behavior, not the SDK version.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default = "initial_revision")]
    pub revision: u32,
}
fn initial_revision() -> u32 {
    1
}

pub(super) fn definitions(reply: &Value) -> Result<Vec<Definition>> {
    let Some(value) = reply.get("tools") else {
        return Ok(Vec::new());
    };
    let definitions: Vec<Definition> = serde_json::from_value(value.clone())?;
    ensure!(
        definitions.len() <= 64,
        "a plugin may declare at most 64 tools"
    );
    let mut names = BTreeSet::new();
    for definition in &definitions {
        ensure!(definition.revision > 0, "tool revision must be positive");
        ensure!(
            definition.name.len() <= 48,
            "plugin-local tool name exceeds 48 bytes"
        );
        ensure!(
            definition.description.len() <= 8192,
            "plugin tool description exceeds 8 KiB"
        );
        ensure!(
            names.insert(&definition.name),
            "duplicate plugin tool {}",
            definition.name
        );
        crate::tools::surface::catalog::validate_definition(&json!({
            "type":"function", "function": {
                "name":definition.name, "description":definition.description,
                "parameters":definition.parameters,
            }
        }))?;
    }
    Ok(definitions)
}

pub(super) struct PluginTool {
    host: Arc<Host>,
    owner: String,
    target: Arc<dyn super::Plugin>,
    name: String,
    definition: Definition,
}
impl PluginTool {
    pub(super) fn new(
        host: Arc<Host>,
        target: Arc<dyn super::Plugin>,
        definition: Definition,
    ) -> Self {
        let owner = target.name();
        // Escape underscores before dots so distinct extension names never share a namespace.
        let namespace = owner.replace('_', "_u").replace('.', "_d");
        let name = format!("ext__{namespace}__{}", definition.name);
        Self {
            host,
            owner: owner.into(),
            target,
            name,
            definition,
        }
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: &Value, cancel: &AtomicBool) -> ToolOutput {
        self.execute_with_host(ctx, args, cancel, &mut super::unsupported_host)
    }
    fn execute_with_host(
        &self,
        ctx: &ToolContext<'_>,
        args: &Value,
        cancel: &AtomicBool,
        host: &mut super::HostCall<'_>,
    ) -> ToolOutput {
        if cancel.load(Ordering::Acquire) {
            return ToolOutput::err("extension tool cancelled before dispatch");
        }
        if let Err(error) = crate::tools::surface::catalog::validate_arguments(&self.schema(), args)
        {
            return ToolOutput::err(error);
        }
        let reply = self.host.request_tool_with_host(
            &self.target,
            json!({
                "name":self.definition.name, "arguments":args, "workspace":ctx.workspace.root(),
            }),
            cancel,
            host,
        );
        match reply {
            Ok(value) => {
                let (Some(text), Some(is_error)) = (
                    value.get("text").and_then(Value::as_str),
                    value.get("is_error").and_then(Value::as_bool),
                ) else {
                    return ToolOutput::err(
                        "invalid extension tool result: expected text and is_error",
                    )
                    .with_effects(crate::runtime::execution::Effects::Unknown);
                };
                let mut output = if is_error {
                    ToolOutput::err(text)
                } else {
                    ToolOutput::ok(text.into())
                };
                output.data = Some(value);
                output
            }
            Err(error) => ToolOutput::err(format!("extension {}: {error:#}", self.owner))
                .with_effects(crate::runtime::execution::Effects::Unknown),
        }
    }
}
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn schema(&self) -> Value {
        json!({"type":"function", "function": {
            "name":self.name,
            "description":format!("[Extension {}] {}", self.owner, self.definition.description),
            "parameters":self.definition.parameters,
        }})
    }
    fn contract_revision(&self) -> u32 {
        self.definition.revision
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn render_result(
        &self,
        args: &Value,
        output: &ToolOutput,
    ) -> crate::tools::surface::presentation::Presentation {
        let mut presentation =
            crate::tools::surface::presentation::render_result(self.name(), args, output);
        let Some(envelope) = output.data.as_ref() else {
            return presentation;
        };
        let Some(renderer_id) = envelope.get("renderer") else {
            return presentation;
        };
        let Some(id) = renderer_id.as_str() else {
            presentation
                .facts
                .push("Invalid extension renderer: expected a string id".into());
            return presentation;
        };
        // Bind presentation to this adapter's original registration, not a replacement by name.
        let contributions = self.target.contributions();
        let Some(renderer) = contributions
            .renderers
            .iter()
            .find(|renderer| renderer.id == id)
        else {
            presentation.facts.push(format!(
                "Unknown extension renderer: {}",
                display_text(id, 256)
            ));
            return presentation;
        };
        let data = envelope.get("data").unwrap_or(&Value::Null);
        presentation.label = display_text(&renderer.title, 256);
        let mut lines = Vec::new();
        if let Some(markdown) = &renderer.markdown {
            lines.push(display_text(markdown, 4096));
        }
        for field in renderer.fields.iter().take(64) {
            let Some(value) = data.pointer(&field.path) else {
                continue;
            };
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            let line = format!(
                "{}: {}",
                display_text(&field.label, 256),
                display_text(&text, 1024)
            );
            presentation.facts.push(line.clone());
            lines.push(line);
        }
        if !lines.is_empty() {
            presentation.preview = Some(crate::runtime::presentation::truncate(
                &lines.join("\n"),
                8192,
            ));
        }
        presentation
    }
    fn supports_tool_dispatch(&self) -> bool {
        true
    }
    fn call_with_dispatch(
        &self,
        ctx: &ToolContext<'_>,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
        _: &dyn Fn(
            crate::tools::exec::background::AdoptedJob,
        ) -> std::result::Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    ) -> ToolOutput {
        self.execute_with_host(ctx, args, cancel, &mut |method, params| {
            ensure!(!cancel.load(Ordering::Acquire), "extension tool cancelled");
            match method {
                "tools.list" => {
                    let output = dispatch(ToolCallback::Schemas);
                    ensure!(!output.is_error, "{}", output.text);
                    output
                        .data
                        .ok_or_else(|| anyhow::anyhow!("controller returned no tool schemas"))
                }
                "tools.call" => {
                    let name = params
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("tools.call requires name"))?;
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    ensure!(
                        arguments.is_object(),
                        "tools.call arguments must be an object"
                    );
                    let output = dispatch(ToolCallback::Call { name: name.into(), arguments });
                    Ok(json!({"text":output.text, "is_error":output.is_error, "data":output.data, "execution":output.execution}))
                }
                "progress" | "events.emit" => {
                    let text = params
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("progress requires text"))?;
                    ensure!(text.len() <= 64 * 1024, "progress text exceeds 64 KiB");
                    on_event(ToolEvent {
                        kind: params
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("progress")
                            .into(),
                        name: params
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&self.name)
                            .into(),
                        text: text.into(),
                        artifact: None,
                    });
                    Ok(Value::Null)
                }
                _ => super::unsupported_host(method, params),
            }
        })
    }
    fn call(&self, ctx: &ToolContext<'_>, args: &Value) -> ToolOutput {
        self.execute(ctx, args, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext<'_>,
        args: &Value,
        _: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        self.execute(ctx, args, cancel)
    }
}

/// Plain display fragments cannot introduce terminal controls or unbounded previews.
fn display_text(text: &str, limit: usize) -> String {
    let text = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect::<String>();
    crate::runtime::presentation::truncate(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        limit,
    )
}

#[cfg(test)]
mod tests;
