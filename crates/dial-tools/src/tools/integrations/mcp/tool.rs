//! Remote MCP tools exposed as dial [`Tool`]s, the resource/prompt surfaces, and rendering of
//! their results.

use super::Session;
use crate::runtime::execution::{Completeness, Effects, OutputIntegrity};
use crate::runtime::integration::{
    DispatchState, IntegrationExecution, IntegrationFailure, Settlement,
};
use crate::runtime::tool_output::ToolOutput;
use crate::tools::Workspace;
use crate::tools::integrations::ext::{
    Concurrency, EXTENSION_API_VERSION, LifecycleEvent, Tool, ToolContext,
};
use crate::tools::surface::resources::Access;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Longest result handed to the model: the agent's per-result cap, so the agent never cuts a
/// result this module already bounded and labelled.
const MAX_RESULT: usize = crate::tools::MODEL_OUTPUT_LIMIT;
/// Server-supplied tool descriptions are untrusted prompt text; keep them short.
const MAX_DESCRIPTION: usize = 2048;
/// Server-supplied input schemas are sent with every request that offers the tool; bound them
/// like descriptions ([`bounded_input_schema`]).
const MAX_INPUT_SCHEMA: usize = 16 * 1024;
/// Base64 of the PNG signature: only PNG image blocks reach the shared image-attachment path.
const PNG_BASE64_PREFIX: &str = "iVBORw0KGgo";

/// What `tools/list` says about one tool.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// `readOnlyHint`, masked by the operator's `trust_read_only_hint` (approval exemption).
    pub read_only_hint: bool,
    /// The server declares the tool effect-free: `readOnlyHint: true` and not `destructiveHint:
    /// true`. Never masked by trust: it selects the scheduling class only, never permission.
    pub declared_read_only: bool,
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
        let hint = |name: &str| {
            entry
                .pointer(&format!("/annotations/{name}"))
                .and_then(Value::as_bool)
        };
        let read_only_hint = hint("readOnlyHint") == Some(true);
        Some(RemoteTool {
            name,
            description,
            input_schema,
            read_only_hint,
            // A contradictory `destructiveHint: true` keeps the conservative exclusive class.
            declared_read_only: read_only_hint && hint("destructiveHint") != Some(true),
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

    fn effect_free(&self) -> bool {
        self.remote.declared_read_only && self.session.catalog_current()
    }

    fn description(&self) -> String {
        let text = if self.remote.description.trim().is_empty() {
            format!("MCP tool `{}`", self.remote.name)
        } else {
            self.remote.description.trim().to_owned()
        };
        let text: String = text.chars().take(MAX_DESCRIPTION).collect();
        if bounded_input_schema(&self.remote.input_schema).1 {
            return format!(
                "{}: {text} (Input schema shortened for size; the server validates arguments.)",
                self.server
            );
        }
        format!("{}: {text}", self.server)
    }

    fn call_request(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &dyn Fn() -> bool,
        progress: &dyn Fn(&str),
    ) -> ToolOutput {
        let output = if !args.is_object() {
            let mut output = ToolOutput::err("MCP tool arguments must be a JSON object")
                .with_effects(Effects::None);
            output.execution.integration = Some(IntegrationExecution {
                dispatch: DispatchState::NotDispatched,
                failure: Some(IntegrationFailure::Admission),
                ..Default::default()
            });
            output
        } else {
            let params = json!({"name": self.remote.name, "arguments": args});
            match self.session.client_until(cancel) {
                Err(error) => integration_error(error, IntegrationFailure::Connection),
                Ok(client) => {
                    if let Err(error) = self.session.admit_tool(&client, &self.remote, cancel) {
                        return attach_health(
                            integration_error(error, IntegrationFailure::Admission),
                            &self.session,
                        );
                    }
                    match client.request_with(
                        "tools/call",
                        params,
                        self.timeout,
                        cancel,
                        Some(progress),
                    ) {
                        Ok(result) => {
                            let mut output = retain_result(ctx, render_result(&result), &result);
                            output.execution.integration = Some(IntegrationExecution {
                                dispatch: DispatchState::Dispatched,
                                settlement: Settlement::Reply,
                                failure: output.is_error.then_some(IntegrationFailure::Remote),
                                ..Default::default()
                            });
                            output
                        }
                        Err(error) => integration_error(error, IntegrationFailure::Connection),
                    }
                }
            }
        };
        attach_health(output, &self.session)
    }
}

fn attach_health(mut output: ToolOutput, session: &Session) -> ToolOutput {
    let health = session.health();
    let facts = output
        .execution
        .integration
        .get_or_insert_with(Default::default);
    facts.state_changes = session.observe_health(health.transitions);
    facts.health = health;
    output
}

fn integration_error(error: anyhow::Error, fallback: IntegrationFailure) -> ToolOutput {
    let effects = error
        .downcast_ref::<super::client::RequestEffects>()
        .map_or(Effects::None, |failure| failure.0);
    let mut output = ToolOutput::err(format!("{error:#}")).with_effects(effects);
    let facts = if let Some(failure) = error.downcast_ref::<super::client::RequestFailure>() {
        IntegrationExecution {
            dispatch: if failure.dispatched {
                DispatchState::Dispatched
            } else {
                DispatchState::NotDispatched
            },
            settlement: failure.settlement,
            failure: Some(failure.failure),
            ..Default::default()
        }
    } else {
        IntegrationExecution {
            dispatch: if effects == Effects::None {
                DispatchState::NotDispatched
            } else {
                DispatchState::Unknown
            },
            failure: Some(fallback),
            ..Default::default()
        }
    };
    output.execution.integration = Some(facts);
    output
}

/// Retain the protocol representation with explicit bounded-storage integrity.
fn retain_result(ctx: &ToolContext, mut output: ToolOutput, result: &Value) -> ToolOutput {
    if let Some(store) = ctx.workspace.artifacts() {
        let presentation = output
            .execution
            .output
            .as_ref()
            .map_or(Completeness::Unknown, |facts| facts.presentation);
        let note = match store.retain_with_integrity(&result.to_string()) {
            Ok((uri, mut facts)) => {
                facts.capture = Completeness::Complete;
                facts.presentation = presentation;
                let label = if facts.retention == Completeness::Complete {
                    "Complete"
                } else {
                    "Partial"
                };
                output.execution.output = Some(facts);
                format!("\n[{label} MCP protocol result: {uri}]")
            }
            Err(error) => {
                if let Some(facts) = output.execution.output.as_mut() {
                    facts.retention = Completeness::Unknown;
                    facts.retained_bytes = None;
                    facts.omitted_bytes = None;
                }
                format!("\n[MCP protocol result could not be retained: {error:#}]")
            }
        };
        output.text.push_str(&note);
    }
    output
}

/// Protocol bytes remain in `data`; the text is a projection, not the original payload.
fn rendered(result: &Value, text: String, is_error: bool, omitted: bool) -> ToolOutput {
    let clean = strip_controls(&text);
    let visible = bound(&clean);
    let partial = omitted || clean != text || visible != clean;
    let bytes = result.to_string().len() as u64;
    let mut output = ToolOutput {
        execution: Default::default(),
        text: visible,
        is_error,
        data: Some(result.clone()),
    };
    output.execution.output = Some(OutputIntegrity {
        capture: Completeness::Complete,
        retention: Completeness::Complete,
        presentation: if partial {
            Completeness::Partial
        } else {
            Completeness::Complete
        },
        original_bytes: Some(bytes),
        retained_bytes: Some(bytes),
        omitted_bytes: Some(0),
        ..Default::default()
    });
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
                "parameters": bounded_input_schema(&self.remote.input_schema).0,
            }
        })
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        !self.remote.read_only_hint || !self.session.catalog_current()
    }

    fn read_only_hint(&self) -> bool {
        self.remote.read_only_hint && self.session.catalog_current()
    }

    /// A server-declared read-only tool is an effect-free external read: shared batch slot and no
    /// workspace footprint, so authorized calls run together. Approval is unchanged
    /// ([`needs_approval`](Self::needs_approval)); a stale catalog revokes the class with the hint.
    fn concurrency(&self, _args: &Value) -> Concurrency {
        if self.effect_free() {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }

    fn resources(&self, _workspace: &Workspace, _args: &Value) -> Option<Access> {
        self.effect_free().then(Access::default)
    }

    fn may_modify_workspace(&self, _args: &Value) -> bool {
        !self.effect_free()
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_request(ctx, args, &|| false, &|_| {})
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutput {
        self.call_request(
            ctx,
            args,
            &|| cancel.load(std::sync::atomic::Ordering::Acquire),
            &|text| {
                on_event(crate::tools::integrations::ext::ToolEvent {
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
        let output = match output {
            Ok((output, result)) => {
                let mut output = retain_result(ctx, output, &result);
                output.execution.integration = Some(IntegrationExecution {
                    // Listings may have been served from a validated cache.
                    dispatch: DispatchState::Unknown,
                    settlement: Settlement::Reply,
                    ..Default::default()
                });
                output
            }
            Err(error) => integration_error(error, IntegrationFailure::Protocol),
        };
        attach_health(output, &self.session)
    }

    fn read_resource(
        &self,
        args: &Value,
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<(ToolOutput, Value)> {
        match args.get("uri").and_then(Value::as_str) {
            None => {
                let listing = self.session.resources_until(cancel)?;
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
                let listing = self.session.prompts_until(cancel)?;
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
        _on_event: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
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

/// An input schema within [`MAX_INPUT_SCHEMA`] bytes, and whether it had to be shortened. Steps,
/// each tried only when the previous one is still too large: drop annotations (descriptions,
/// titles, examples); keep only top-level parameter names with their type, enum and required
/// status; accept any object. The server still validates every call against its own schema.
fn bounded_input_schema(schema: &Value) -> (Value, bool) {
    let fits = |schema: &Value| schema.to_string().len() <= MAX_INPUT_SCHEMA;
    if fits(schema) {
        return (schema.clone(), false);
    }
    let mut lean = schema.clone();
    strip_schema_annotations(&mut lean);
    if fits(&lean) {
        return (lean, true);
    }
    let mut flat = json!({"type": "object"});
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        let shapes: serde_json::Map<String, Value> = properties
            .iter()
            .map(|(name, property)| {
                let mut shape = serde_json::Map::new();
                for keyword in ["type", "enum"] {
                    if let Some(value) = property.get(keyword) {
                        shape.insert(keyword.into(), value.clone());
                    }
                }
                (name.clone(), Value::Object(shape))
            })
            .collect();
        flat["properties"] = Value::Object(shapes);
    }
    if let Some(required) = schema.get("required").filter(|value| value.is_array()) {
        flat["required"] = required.clone();
    }
    if fits(&flat) {
        return (flat, true);
    }
    (json!({"type": "object"}), true)
}

/// Remove annotation keywords from every schema node. Property names and literal values are data:
/// a parameter named `description` keeps its schema.
fn strip_schema_annotations(node: &mut Value) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    for annotation in ["description", "title", "examples", "$comment"] {
        object.remove(annotation);
    }
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(Value::Object(children)) = object.get_mut(keyword) {
            children.values_mut().for_each(strip_schema_annotations);
        }
    }
    for keyword in [
        "items",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "allOf",
        "anyOf",
        "oneOf",
        "prefixItems",
    ] {
        match object.get_mut(keyword) {
            Some(Value::Array(children)) => children.iter_mut().for_each(strip_schema_annotations),
            Some(child) => strip_schema_annotations(child),
            None => {}
        }
    }
}

/// The first PNG image block, as the shared tool-image envelope (`data.artifact`) that the agent
/// attaches for a model route that accepts images. Other image types and audio stay noted only.
fn png_attachment(result: &Value) -> Option<Value> {
    result
        .get("content")?
        .as_array()?
        .iter()
        .find(|block| png_block(block))
        .map(
            |block| json!({"data": block["data"], "encoding": "base64", "media_type": "image/png"}),
        )
}

fn png_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("image")
        && block.get("mimeType").and_then(Value::as_str) == Some("image/png")
        && block
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|data| data.starts_with(PNG_BASE64_PREFIX))
}

/// Flattens a `tools/call` result: text blocks verbatim, other block kinds noted by type. The text
/// is the server's, so control characters are stripped and the length bounded. The first PNG image
/// travels in `data.artifact` for vision routes. `structuredContent` is appended only when no text
/// block already carries the same JSON (servers are asked to send both).
pub fn render_result(result: &Value) -> ToolOutput {
    let blocks = result.get("content").and_then(Value::as_array);
    let attachment = png_attachment(result);
    let mut attached = attachment.is_some();
    let mut parts: Vec<String> = blocks
        .map(|blocks| {
            blocks
                .iter()
                .map(|block| {
                    if attached && png_block(block) {
                        attached = false;
                        "[image content, image/png, attached for models that accept images]"
                            .to_owned()
                    } else {
                        render_block(block)
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(structured) = result.get("structuredContent") {
        let duplicated = blocks.into_iter().flatten().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .is_some_and(|parsed| parsed == *structured)
        });
        if !duplicated {
            parts.push(structured.to_string());
        }
    }
    let text = if parts.is_empty() {
        "(the tool returned no content)".to_owned()
    } else {
        parts.join("\n")
    };
    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    let omitted = result
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(block_omitted));
    let mut output = rendered(result, text, is_error, omitted);
    if let (Some(artifact), Some(data)) = (attachment, output.data.as_mut())
        && !is_error
    {
        data["artifact"] = artifact;
    }
    output
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
    let omitted = result
        .get("contents")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("text").and_then(Value::as_str).is_none())
        });
    rendered(result, text, false, omitted)
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
    let omitted = result
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("content"))
                .any(block_omitted)
        });
    rendered(result, text, false, omitted)
}

/// Renders a resource or prompt listing as pretty JSON, bounded like any server output.
fn listing_output(listing: &Value) -> ToolOutput {
    let text = serde_json::to_string_pretty(listing).unwrap_or_else(|_| listing.to_string());
    rendered(listing, text, false, false)
}

fn block_omitted(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => false,
        Some("resource") => block
            .pointer("/resource/text")
            .and_then(Value::as_str)
            .is_none(),
        Some("resource_link") => false,
        _ => true,
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
