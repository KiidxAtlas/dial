//! Pure JSON contract for GitLab Duo Agent Platform workflows: start payloads,
//! server-event parsing, action extraction, and transcript rendering. No I/O.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::llm::Chat;

pub(super) const WORKFLOW_DEFINITION: &str = "ambient";
pub(super) const CLIENT_TYPE: &str = "node-websocket";
pub(super) const LANGUAGE_SERVER_VERSION: &str = "8.104.0";
const AGENT_NAME: &str = "omp_agent";
const PROMPT_ID: &str = "omp_inline_prompt";
const MCP_PREFIX: &str = "mcp__omp__";
/// Goal bytes above this are rejected before a workflow is created.
pub(super) const GOAL_HARD_LIMIT: usize = 2_000_000;
/// Client capability ids advertised in the start request.
const CLIENT_CAPABILITIES: [&str; 5] = [
    "incremental_streaming",
    "read_file_chunked",
    "shell_command",
    "command_timeout",
    "tool_call_approval",
];
/// Prompt tags the server rejects; they are escaped instead of sent raw.
const RESERVED_TAGS: [&str; 4] = [
    "goal",
    "system",
    "truncated_tool_output",
    "truncation_notice",
];

/// Workflow-level status reported by the server.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Status {
    /// `INPUT_REQUIRED` or `FINISHED`: the turn is complete.
    Complete,
    /// `FAILED` or `STOPPED`, carrying the server message.
    Failed(String),
    /// Approval request; this provider does not answer approvals in-band.
    Approval,
    /// No recognised status on this event.
    Running,
}

/// Parsed server-side tool action (`runMCPTool`).
#[derive(Debug, PartialEq)]
pub(super) struct Action {
    pub(super) request_id: String,
    pub(super) name: String,
    pub(super) arguments: Value,
}

/// Everything a start request needs besides the transcript.
pub(super) struct Start<'a> {
    pub(super) workflow_id: &'a str,
    pub(super) goal: &'a str,
    pub(super) system: &'a str,
    pub(super) tools: &'a [Value],
    pub(super) model: &'a str,
    pub(super) project_id: &'a str,
    pub(super) namespace_id: &'a str,
    pub(super) root_namespace_id: &'a str,
}

/// Classify the `status` of a server event.
pub(super) fn status_of(event: &Value) -> Status {
    let status = event
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .pointer("/workflowStatus/status")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            event
                .pointer("/newCheckpoint/status")
                .and_then(Value::as_str)
        });
    match status {
        None => Status::Running,
        Some("INPUT_REQUIRED" | "FINISHED") => Status::Complete,
        Some("PLAN_APPROVAL_REQUIRED" | "TOOL_CALL_APPROVAL_REQUIRED") => Status::Approval,
        Some(status @ ("FAILED" | "STOPPED")) => {
            let message = event
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| event.get("message").and_then(Value::as_str))
                .unwrap_or(status);
            Status::Failed(message.to_owned())
        }
        Some(_) => Status::Running,
    }
}

/// The server hit its graph-recursion ceiling on a healthy long loop.
pub(super) fn is_step_limit(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("reached its maximum step limit")
}

/// De-identified transient wrapper around upstream model faults.
pub(super) fn is_generic_error(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("error processing your request in the duo agent platform")
}

/// Visible checkpoint text, if this event carries any. Nested JSON checkpoint
/// bodies are not decoded.
pub(super) fn checkpoint_text(event: &Value) -> Option<&str> {
    let checkpoint = event
        .pointer("/action/newCheckpoint")
        .or_else(|| event.get("newCheckpoint"))
        .or_else(|| event.get("checkpoint"))?;
    ["message", "text", "content"]
        .iter()
        .find_map(|key| checkpoint.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
}

/// Extract a `runMCPTool` action. `Ok(None)` when the event is not an action.
/// An action without `requestID` is a protocol error.
pub(super) fn action_of(event: &Value) -> Result<Option<Action>> {
    let wrapped = ["action", "workflowAction", "toolCall"]
        .iter()
        .find_map(|key| event.get(*key).filter(|value| value.is_object()));
    let (name, body) = match wrapped {
        Some(wrapped) => {
            let name = wrapped
                .get("name")
                .or_else(|| wrapped.get("action"))
                .and_then(Value::as_str)
                .or_else(|| event.get("actionName").and_then(Value::as_str));
            (name, wrapped)
        }
        None => match ["runMCPTool", "run_mcp_tool"]
            .iter()
            .find(|key| event.get(**key).is_some())
        {
            Some(key) => (Some(*key), event.get(*key).unwrap_or(event)),
            None => return Ok(None),
        },
    };
    if !matches!(name, Some("runMCPTool" | "run_mcp_tool")) {
        return Ok(None);
    }
    let request_id = body
        .get("requestID")
        .or_else(|| body.get("requestId"))
        .or_else(|| event.get("requestID"))
        .or_else(|| event.get("requestId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let Some(request_id) = request_id else {
        bail!("GitLab Duo Agent action is missing requestID");
    };
    let args = body
        .get("args")
        .or_else(|| body.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    let args = match args {
        Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        other => other,
    };
    let raw_name = ["toolName", "tool_name"]
        .iter()
        .find_map(|key| body.get(*key).and_then(Value::as_str))
        .or_else(|| {
            ["toolName", "tool_name", "name"]
                .iter()
                .find_map(|key| args.get(*key).and_then(Value::as_str))
        })
        .or_else(|| {
            body.get("name")
                .and_then(Value::as_str)
                .filter(|name| !matches!(*name, "runMCPTool" | "run_mcp_tool"))
        })
        .unwrap_or_default();
    let tool = raw_name.strip_prefix(MCP_PREFIX).unwrap_or(raw_name);
    ensure!(!tool.is_empty(), "GitLab Duo Agent action has no tool name");
    Ok(Some(Action {
        request_id: request_id.to_owned(),
        name: tool.to_owned(),
        arguments: tool_arguments(tool, &args),
    }))
}

/// Tool arguments for Dial. The `edit` tool arrives with a bare `input` string.
fn tool_arguments(tool: &str, args: &Value) -> Value {
    let inner = args
        .get("args")
        .filter(|value| value.is_object())
        .unwrap_or(args);
    if tool == "edit"
        && let Some(input) = inner.get("input").and_then(Value::as_str)
    {
        return json!({ "input": input });
    }
    inner.clone()
}

/// Plain-text reply for a tool result, in the server's `actionResponse` shape.
pub(super) fn action_response(request_id: &str, content: &str) -> Value {
    json!({
        "actionResponse": {
            "requestID": request_id,
            "plainTextResponse": { "response": content },
        }
    })
}

/// Drop HTML comments and escape reserved prompt tags so the server accepts the
/// system prompt. Matching is case-insensitive; tags become `&lt;tag&gt;`.
pub(super) fn sanitize_prompt(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('<') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        if rest.starts_with("<!--") {
            rest = match rest.find("-->") {
                Some(end) => &rest[end + 3..],
                None => "",
            };
            continue;
        }
        if let Some(len) = reserved_tag_len(rest) {
            out.push_str("&lt;");
            out.push_str(&rest[1..len - 1]);
            out.push_str("&gt;");
            rest = &rest[len..];
            continue;
        }
        out.push('<');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

/// Length of a `<[/]tag>` reserved tag at the start of `text`, if any.
fn reserved_tag_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut pos = 1;
    if bytes.get(pos) == Some(&b'/') {
        pos += 1;
    }
    let start = pos;
    while pos < bytes.len() && (bytes[pos].is_ascii_alphabetic() || bytes[pos] == b'_') {
        pos += 1;
    }
    let name = text.get(start..pos)?.to_ascii_lowercase();
    if !RESERVED_TAGS.contains(&name.as_str()) || bytes.get(pos) != Some(&b'>') {
        return None;
    }
    Some(pos + 1)
}

/// Render the transcript as `(system prompt, goal)`. A lone user message is
/// sent bare; a longer conversation uses role markers so a restarted workflow
/// replays the same context.
pub(super) fn render_goal(chat: &Chat) -> Result<(String, String)> {
    let mut system = Vec::new();
    let mut turns = Vec::new();
    for message in &chat.messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = content_text(message.get("content"));
        match role {
            "system" | "developer" => system.push(content),
            _ => turns.push((role, content, message)),
        }
    }
    let goal = match turns.as_slice() {
        [(role, content, _)] if *role == "user" => content.clone(),
        _ => {
            let mut goal = String::new();
            for (role, content, message) in &turns {
                goal.push_str("<|im_start|>");
                goal.push_str(role);
                goal.push('\n');
                goal.push_str(content);
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = call.pointer("/function/name").and_then(Value::as_str);
                    let args = call.pointer("/function/arguments").and_then(Value::as_str);
                    goal.push_str(&format!(
                        "\n<tool_call name=\"{}\">{}</tool_call>",
                        name.unwrap_or(""),
                        args.unwrap_or("{}")
                    ));
                }
                goal.push_str("<|im_end|>\n");
            }
            goal
        }
    };
    ensure!(
        goal.len() <= GOAL_HARD_LIMIT,
        "GitLab Duo Agent goal is {} bytes; the limit is {GOAL_HARD_LIMIT}",
        goal.len()
    );
    Ok((sanitize_prompt(&system.join("\n\n")), goal))
}

/// Text of a message `content` field (string or text parts).
pub(super) fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Start request for a freshly created workflow.
pub(super) fn start_request(start: &Start<'_>) -> Result<Value> {
    let mut mcp_tools = Vec::new();
    let mut preapproved = Vec::new();
    for tool in start.tools {
        let function = tool.get("function").unwrap_or(tool);
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .context("GitLab Duo Agent tool is missing a name")?;
        let schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        mcp_tools.push(json!({
            "name": name,
            "originalToolName": name,
            "serverName": "omp",
            "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
            "inputSchema": schema.to_string(),
            "isApproved": true,
        }));
        preapproved.push(name);
    }
    let metadata = json!({
        "environment": "ide",
        "client_type": CLIENT_TYPE,
        "projectId": start.project_id,
        "namespaceId": start.namespace_id,
        "rootNamespaceId": start.root_namespace_id,
        "selectedModelIdentifier": start.model,
    });
    Ok(json!({
        "startRequest": {
            "workflowID": start.workflow_id,
            "clientVersion": "1.0",
            "workflowDefinition": WORKFLOW_DEFINITION,
            "goal": start.goal,
            "workflowMetadata": metadata.to_string(),
            "additional_context": [],
            "clientCapabilities": CLIENT_CAPABILITIES,
            "mcpTools": mcp_tools,
            "preapproved_tools": preapproved,
            "flowConfigSchemaVersion": "v1",
            "flowConfig": flow_config(start.system),
        }
    }))
}

/// Inline agent definition: one agent, one prompt, no toolset (tools are MCP).
fn flow_config(system: &str) -> Value {
    json!({
        "version": "v1",
        "environment": "ambient",
        "flow": { "entry_point": AGENT_NAME },
        "components": [{
            "name": AGENT_NAME,
            "type": "AgentComponent",
            "prompt_id": PROMPT_ID,
            "toolset": [],
            "inputs": [{ "from": "context:goal", "as": "goal" }],
        }],
        "routers": [{ "from": AGENT_NAME, "to": "end" }],
        "prompts": [{
            "name": "OMP inline prompt",
            "prompt_id": PROMPT_ID,
            "unit_primitives": ["duo_agent_platform"],
            "prompt_template": {
                "system": system,
                "user": "{{goal}}",
                "placeholder": "history",
            },
        }],
    })
}
