//! Remote MCP tools exposed as dial [`Tool`]s, the resource/prompt surfaces, and rendering of
//! their results.

use super::Session;
use crate::runtime::execution::Effects;
use crate::runtime::tool_output::ToolOutput;
use crate::tools::ext::{EXTENSION_API_VERSION, LifecycleEvent, Tool, ToolContext};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Longest result handed to the model.
const MAX_RESULT: usize = 64 * 1024;
/// Server-supplied tool descriptions are untrusted prompt text; keep them short.
const MAX_DESCRIPTION: usize = 2048;

/// What `tools/list` says about one tool.
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only_hint: bool,
}

impl RemoteTool {
    pub fn parse(entry: &Value) -> Option<RemoteTool> {
        let name = entry
            .get("name")?
            .as_str()
            .filter(|name| !name.is_empty())?
            .to_owned();
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let input_schema = match entry.get("inputSchema") {
            Some(Value::Object(schema)) => {
                let mut schema = schema.clone();
                schema.entry("type").or_insert_with(|| json!("object"));
                Value::Object(schema)
            }
            _ => json!({"type": "object"}),
        };
        Some(RemoteTool {
            name,
            description,
            input_schema,
            read_only_hint: entry
                .pointer("/annotations/readOnlyHint")
                .and_then(Value::as_bool)
                == Some(true),
        })
    }
}

pub struct McpTool {
    name: String,
    server: String,
    remote: RemoteTool,
    session: Arc<Session>,
    timeout: Duration,
    lifecycle_hooks: Vec<String>,
}

impl McpTool {
    pub fn with_lifecycle_hooks(
        name: String,
        server: &str,
        remote: RemoteTool,
        session: Arc<Session>,
        timeout: Duration,
        lifecycle_hooks: Vec<String>,
    ) -> Self {
        McpTool {
            name,
            server: server.to_owned(),
            remote,
            session,
            timeout,
            lifecycle_hooks,
        }
    }

    fn description(&self) -> String {
        let text = if self.remote.description.trim().is_empty() {
            format!("MCP tool `{}`", self.remote.name)
        } else {
            self.remote.description.trim().to_owned()
        };
        let text: String = text.chars().take(MAX_DESCRIPTION).collect();
        format!("{}: {text}", self.server)
    }

    fn call_request(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &dyn Fn() -> bool,
        progress: &dyn Fn(&str),
    ) -> ToolOutput {
        let arguments = if args.is_object() {
            args.clone()
        } else {
            json!({})
        };
        let params = json!({"name": self.remote.name, "arguments": arguments});
        let client = match self.session.client_until(cancel) {
            Ok(client) => client,
            Err(error) => return ToolOutput::err(format!("{error:#}")).with_effects(Effects::None),
        };
        match client.request_with("tools/call", params, self.timeout, cancel, Some(progress)) {
            Ok(result) => retain_result(ctx, render_result(&result), &result),
            Err(error) => {
                let effects = error
                    .downcast_ref::<super::client::RequestEffects>()
                    .map_or(Effects::None, |failure| failure.0);
                ToolOutput::err(format!("{error:#}")).with_effects(effects)
            }
        }
    }
}

/// Appends a pointer to the complete protocol result when the workspace can store artifacts.
fn retain_result(ctx: &ToolContext, mut output: ToolOutput, result: &Value) -> ToolOutput {
    if let Some(store) = ctx.workspace.artifacts() {
        match store.retain(&result.to_string()) {
            Ok(uri) => output
                .text
                .push_str(&format!("\n[Complete MCP protocol result: {uri}]")),
            Err(e) => return ToolOutput::err(format!("retaining MCP result: {e:#}")),
        }
    }
    output
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description(),
                "parameters": self.remote.input_schema,
            }
        })
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        !self.remote.read_only_hint
    }

    fn read_only_hint(&self) -> bool {
        self.remote.read_only_hint
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_request(ctx, args, &|| false, &|_| {})
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(crate::tools::ext::ToolEvent) + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutput {
        self.call_request(
            ctx,
            args,
            &|| cancel.load(std::sync::atomic::Ordering::Acquire),
            &|text| {
                on_event(crate::tools::ext::ToolEvent {
                    kind: "progress".into(),
                    name: self.name.clone(),
                    text: text.to_owned(),
                    artifact: None,
                })
            },
        )
    }

    fn lifecycle_hooks(&self) -> &[String] {
        &self.lifecycle_hooks
    }

    fn lifecycle_channel(&self) -> Option<&str> {
        Some(self.session.name())
    }

    fn on_lifecycle_event(&self, event: &LifecycleEvent) {
        forward_lifecycle(&self.session, &self.lifecycle_hooks, event);
    }
}

/// Which server capability a [`McpSurface`] exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    /// `read_resource`: `resources/read` by `uri`, or the listing when `uri` is omitted.
    Resource,
    /// `get_prompt`: `prompts/get` by `name` with `arguments`, or the listing when `name` is omitted.
    Prompt,
}

/// A model-facing tool over a server's resources or prompts. Always needs approval: the content a
/// server returns is untrusted and the read may subscribe the session to change notices.
pub struct McpSurface {
    name: String,
    server: String,
    kind: SurfaceKind,
    session: Arc<Session>,
    lifecycle_hooks: Vec<String>,
}

impl McpSurface {
    pub fn new(
        name: String,
        server: &str,
        kind: SurfaceKind,
        session: Arc<Session>,
        lifecycle_hooks: Vec<String>,
    ) -> Self {
        McpSurface {
            name,
            server: server.to_owned(),
            kind,
            session,
            lifecycle_hooks,
        }
    }

    fn description(&self) -> String {
        let text = match self.kind {
            SurfaceKind::Resource => {
                "read a resource by uri; omit uri to list resources and resource templates"
            }
            SurfaceKind::Prompt => "get a prompt by name with arguments; omit name to list prompts",
        };
        format!("{}: {text}", self.server)
    }

    fn run(&self, ctx: &ToolContext, args: &Value, cancel: &dyn Fn() -> bool) -> ToolOutput {
        let output = match self.kind {
            SurfaceKind::Resource => self.read_resource(args, cancel),
            SurfaceKind::Prompt => self.get_prompt(args, cancel),
        };
        match output {
            Ok((output, result)) => retain_result(ctx, output, &result),
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }

    fn read_resource(
        &self,
        args: &Value,
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<(ToolOutput, Value)> {
        match args.get("uri").and_then(Value::as_str) {
            None => {
                let listing = self.session.resources()?;
                Ok((listing_output(&listing), listing))
            }
            Some(uri) => {
                let (result, note) = self.session.read_resource(uri, cancel)?;
                let mut output = render_resource(&result);
                if let Some(note) = note {
                    output.text.push('\n');
                    output.text.push_str(&note);
                }
                Ok((output, result))
            }
        }
    }

    fn get_prompt(
        &self,
        args: &Value,
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<(ToolOutput, Value)> {
        match args.get("name").and_then(Value::as_str) {
            None => {
                let listing = self.session.prompts()?;
                Ok((listing_output(&listing), listing))
            }
            Some(name) => {
                let arguments = args
                    .get("arguments")
                    .filter(|value| !value.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let result = self.session.get_prompt(name, arguments, cancel)?;
                Ok((render_prompt(&result), result))
            }
        }
    }
}

impl Tool for McpSurface {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Value {
        let parameters = match self.kind {
            SurfaceKind::Resource => json!({
                "type": "object",
                "properties": {
                    "uri": {"type": "string", "description": "Resource URI to read; omit to list."},
                },
            }),
            SurfaceKind::Prompt => json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Prompt name; omit to list."},
                    "arguments": {"type": "object", "description": "Prompt arguments."},
                },
            }),
        };
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description(),
                "parameters": parameters,
            }
        })
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn read_only_hint(&self) -> bool {
        false
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.run(ctx, args, &|| false)
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(crate::tools::ext::ToolEvent) + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutput {
        self.run(ctx, args, &|| {
            cancel.load(std::sync::atomic::Ordering::Acquire)
        })
    }

    fn lifecycle_hooks(&self) -> &[String] {
        &self.lifecycle_hooks
    }

    fn lifecycle_channel(&self) -> Option<&str> {
        Some(self.session.name())
    }

    fn on_lifecycle_event(&self, event: &LifecycleEvent) {
        forward_lifecycle(&self.session, &self.lifecycle_hooks, event);
    }
}

/// Sends an opted-in lifecycle event to the server. Hooks are observational: delivery failures
/// must never affect the tool call or agent.
fn forward_lifecycle(session: &Session, hooks: &[String], event: &LifecycleEvent) {
    let Some(params) = lifecycle_payload(hooks, event) else {
        return;
    };
    let _ = session.current().notify("notifications/dial/event", params);
}

fn lifecycle_payload(hooks: &[String], event: &LifecycleEvent) -> Option<Value> {
    if !hooks.iter().any(|hook| hook == &event.event) {
        return None;
    }
    let mut params = json!({
        "apiVersion": EXTENSION_API_VERSION,
        "event": &event.event,
    });
    if let Some(session_id) = &event.session_id {
        params["sessionId"] = json!(session_id);
    }
    if let Some(turn_id) = &event.turn_id {
        params["turnId"] = json!(turn_id);
    }
    if let Some(tool_name) = &event.tool_name {
        params["toolName"] = json!(tool_name);
    }
    if let Some(is_error) = event.is_error {
        params["isError"] = json!(is_error);
    }
    Some(params)
}

#[cfg(test)]
mod lifecycle_tests;

/// Flattens a `tools/call` result: text blocks verbatim, other block kinds noted by type. The text
/// is the server's, so control characters are stripped and the length bounded.
pub fn render_result(result: &Value) -> ToolOutput {
    let mut parts: Vec<String> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().map(render_block).collect())
        .unwrap_or_default();
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(structured.to_string());
    }
    let text = if parts.is_empty() {
        "(the tool returned no content)".to_owned()
    } else {
        parts.join("\n")
    };
    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    let text = bound(&strip_controls(&text));
    ToolOutput {
        execution: Default::default(),
        text,
        is_error,
        data: Some(result.clone()),
    }
}

/// Renders a `resources/read` result: each content item's text verbatim, blobs noted.
fn render_resource(result: &Value) -> ToolOutput {
    let parts: Vec<String> = result
        .get("contents")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let field = |name: &str| item.get(name).and_then(Value::as_str);
                    let uri = field("uri").unwrap_or("?");
                    match field("text") {
                        Some(text) => format!("[resource: {uri}]\n{text}"),
                        None => format!(
                            "[resource: {uri}, {}, binary content not shown]",
                            field("mimeType").unwrap_or("unknown type")
                        ),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let text = if parts.is_empty() {
        "(the resource has no content)".to_owned()
    } else {
        parts.join("\n")
    };
    ToolOutput {
        execution: Default::default(),
        text: bound(&strip_controls(&text)),
        is_error: false,
        data: Some(result.clone()),
    }
}

/// Renders a `prompts/get` result: the description, then each message as `role: content`.
fn render_prompt(result: &Value) -> ToolOutput {
    let mut parts: Vec<String> = Vec::new();
    if let Some(description) = result.get("description").and_then(Value::as_str) {
        parts.push(description.to_owned());
    }
    parts.extend(
        result
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|message| {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
                let content = message
                    .get("content")
                    .map_or_else(String::new, render_block);
                format!("{role}: {content}")
            }),
    );
    let text = if parts.is_empty() {
        "(the prompt returned no messages)".to_owned()
    } else {
        parts.join("\n")
    };
    ToolOutput {
        execution: Default::default(),
        text: bound(&strip_controls(&text)),
        is_error: false,
        data: Some(result.clone()),
    }
}

/// Renders a resource or prompt listing as pretty JSON, bounded like any server output.
fn listing_output(listing: &Value) -> ToolOutput {
    let text = serde_json::to_string_pretty(listing).unwrap_or_else(|_| listing.to_string());
    ToolOutput {
        execution: Default::default(),
        text: bound(&strip_controls(&text)),
        is_error: false,
        data: Some(listing.clone()),
    }
}

fn render_block(block: &Value) -> String {
    let field = |name: &str| block.get(name).and_then(Value::as_str);
    match field("type").unwrap_or("unknown") {
        "text" => field("text").unwrap_or("").to_owned(),
        kind @ ("image" | "audio") => format!(
            "[{kind} content, {}, not shown]",
            field("mimeType").unwrap_or("unknown type")
        ),
        "resource_link" => format!("[resource link: {}]", field("uri").unwrap_or("?")),
        "resource" => {
            let resource = block.get("resource");
            let inner = |name: &str| resource.and_then(|r| r.get(name)).and_then(Value::as_str);
            let uri = inner("uri").unwrap_or("?");
            match inner("text") {
                Some(text) => format!("[resource: {uri}]\n{text}"),
                None => format!(
                    "[resource: {uri}, {}, binary content not shown]",
                    inner("mimeType").unwrap_or("unknown type")
                ),
            }
        }
        other => format!("[{other} content not shown]"),
    }
}

/// Drops terminal escapes and other control characters (keeping newline and tab).
pub(super) fn strip_controls(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn bound(text: &str) -> String {
    if text.len() <= MAX_RESULT {
        return text.to_owned();
    }
    let mut end = MAX_RESULT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output truncated: {} of {} bytes shown]",
        &text[..end],
        end,
        text.len()
    )
}

#[cfg(test)]
mod tests;
