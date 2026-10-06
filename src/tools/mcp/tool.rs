//! One remote MCP tool exposed as a dial [`Tool`], and rendering of `tools/call` results.

use super::client::Client;
use crate::tools::ToolOutput;
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
        })
    }
}

pub struct McpTool {
    name: String,
    server: String,
    remote: RemoteTool,
    client: Arc<Client>,
    timeout: Duration,
    lifecycle_hooks: Vec<String>,
}

impl McpTool {
    pub fn with_lifecycle_hooks(
        name: String,
        server: &str,
        remote: RemoteTool,
        client: Arc<Client>,
        timeout: Duration,
        lifecycle_hooks: Vec<String>,
    ) -> Self {
        McpTool {
            name,
            server: server.to_owned(),
            remote,
            client,
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

    /// Server annotations describe intent, not authorization. Arbitrary remote calls always ask.
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        let arguments = if args.is_object() {
            args.clone()
        } else {
            json!({})
        };
        let params = json!({"name": self.remote.name, "arguments": arguments});
        match self.client.request("tools/call", params, self.timeout) {
            Ok(result) => {
                let mut output = render_result(&result);
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
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }

    fn lifecycle_hooks(&self) -> &[String] {
        &self.lifecycle_hooks
    }

    fn on_lifecycle_event(&self, event: &LifecycleEvent) {
        let Some(params) = lifecycle_payload(&self.lifecycle_hooks, event) else {
            return;
        };
        // Hooks are observational: delivery failures must never affect the tool call or agent.
        let _ = self.client.notify("notifications/dial/event", params);
    }
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
mod lifecycle_tests {
    use super::*;

    fn event() -> LifecycleEvent {
        LifecycleEvent {
            event: "tool_finished".into(),
            session_id: Some("session-1".into()),
            turn_id: Some("turn-2".into()),
            tool_name: Some("mcp__example__echo".into()),
            is_error: Some(true),
        }
    }

    #[test]
    fn lifecycle_payload_is_opt_in_and_contains_only_sanitized_metadata() {
        let event = event();
        assert!(lifecycle_payload(&[], &event).is_none());
        let payload = lifecycle_payload(&["turn_started".into()], &event);
        assert!(payload.is_none(), "unsubscribed events are filtered");

        let payload = lifecycle_payload(&["tool_finished".into()], &event).unwrap();
        assert_eq!(
            payload,
            json!({
                "apiVersion": EXTENSION_API_VERSION,
                "event": "tool_finished",
                "sessionId": "session-1",
                "turnId": "turn-2",
                "toolName": "mcp__example__echo",
                "isError": true,
            })
        );
        assert_eq!(
            payload.as_object().unwrap().len(),
            6,
            "no arguments, prompts, outputs, or credentials"
        );
    }
}

/// Flattens a `tools/call` result: text blocks verbatim, other block kinds noted by type. The text
/// is the server's, so control characters are stripped and the length bounded.
pub fn render_result(result: &Value) -> ToolOutput {
    let mut parts: Vec<String> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().map(render_block).collect())
        .unwrap_or_default();
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            parts.push(structured.to_string());
        }
    }
    let text = if parts.is_empty() {
        "(the tool returned no content)".to_owned()
    } else {
        parts.join("\n")
    };
    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    let text = bound(&strip_controls(&text));
    if is_error {
        ToolOutput::err(text)
    } else {
        ToolOutput::ok(text)
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
fn strip_controls(text: &str) -> String {
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
mod tests {
    use super::*;

    #[test]
    fn text_blocks_are_joined_and_other_blocks_noted_by_type() {
        let out = render_result(&json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "body"}},
            {"type": "resource", "resource": {"uri": "file:///b.bin", "blob": "AA", "mimeType": "application/octet-stream"}},
            {"type": "resource_link", "uri": "https://x/y"},
            {"type": "hologram"},
        ]}));
        assert!(!out.is_error);
        assert_eq!(
            out.text,
            "one\n[image content, image/png, not shown]\n[resource: file:///a.txt]\nbody\n\
             [resource: file:///b.bin, application/octet-stream, binary content not shown]\n\
             [resource link: https://x/y]\n[hologram content not shown]"
        );
    }

    #[test]
    fn is_error_maps_and_empty_results_are_explained() {
        let failed =
            render_result(&json!({"isError": true, "content": [{"type": "text", "text": "boom"}]}));
        assert!(failed.is_error);
        assert_eq!(failed.text, "boom");
        assert_eq!(
            render_result(&json!({"content": []})).text,
            "(the tool returned no content)"
        );
        assert_eq!(
            render_result(&json!({"structuredContent": {"n": 1}})).text,
            r#"{"n":1}"#
        );
    }

    #[test]
    fn output_is_bounded_on_a_char_boundary_and_stripped_of_escapes() {
        let big = "é".repeat(MAX_RESULT);
        let out = render_result(&json!({"content": [{"type": "text", "text": big}]}));
        assert!(out.text.contains("[output truncated:"));
        assert!(out.text.len() < MAX_RESULT + 100);
        let escaped = render_result(
            &json!({"content": [{"type": "text", "text": "a\u{1b}[31mred\u{7}\nb"}]}),
        );
        assert_eq!(escaped.text, "a[31mred\nb");
    }

    #[test]
    fn unnamed_remote_tools_are_rejected() {
        assert!(RemoteTool::parse(&json!({"description": "no name"})).is_none());
        assert!(RemoteTool::parse(&json!({"name": ""})).is_none());
    }
}
