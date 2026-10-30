//! Plugin protocol v2: capability negotiation, declarative actions and UI contributions.
//!
//! Protocol 1 plugins keep their original behavior and never receive or emit v2 features. A
//! protocol 2 plugin declares `capabilities` in its `initialize` result; each action kind is
//! accepted only when the plugin declared it, so a plugin cannot claim authority it did not ask for.
use serde_json::Value;
use std::collections::BTreeSet;

/// Protocol version this host speaks. Plugins that reply with no version or `"1"` stay on v1.
pub const HOST_PROTOCOL_VERSION: &str = "2";
/// Protocol versions the host accepts from a plugin.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["1", "2"];

/// Events delivered on the observational queue. They never block the agent loop and cannot rewrite
/// the payload; their replies may only carry declared actions and UI contributions.
/// Names that alias blocking hooks (`tool_call`, `tool_result`, `message_end`) are not listed: the
/// hook path delivers them once.
pub const OBSERVED_EVENTS: &[&str] = &[
    "turn_start",
    "turn_end",
    "message_start",
    "mode_changed",
    "model_changed",
    "mcp_notification",
    "message_update",
    "tool_approval_requested",
    "tool_approval_resolved",
];

/// Capabilities a protocol 2 plugin may declare. Unknown names are ignored so newer plugins still
/// start on this host.
pub const CAP_UI_STATUS: &str = "ui.status";
pub const CAP_UI_HEADER: &str = "ui.header";
pub const CAP_UI_FOOTER: &str = "ui.footer";
pub const CAP_UI_TITLE: &str = "ui.title";
pub const CAP_AUTOCOMPLETE: &str = "autocomplete";
pub const CAP_SEND_USER_MESSAGE: &str = "actions.send_user_message";
pub const CAP_APPEND_ENTRY: &str = "actions.append_entry";
pub const CAP_SET_MODEL: &str = "actions.set_model";
pub const CAP_SET_THINKING: &str = "actions.set_thinking";
pub const CAP_REGISTER_PROVIDER: &str = "actions.register_provider";
pub const CAP_UNREGISTER_PROVIDER: &str = "actions.unregister_provider";

const KNOWN_CAPABILITIES: &[&str] = &[
    CAP_UI_STATUS,
    CAP_UI_HEADER,
    CAP_UI_FOOTER,
    CAP_UI_TITLE,
    CAP_AUTOCOMPLETE,
    CAP_SEND_USER_MESSAGE,
    CAP_APPEND_ENTRY,
    CAP_SET_MODEL,
    CAP_SET_THINKING,
    CAP_REGISTER_PROVIDER,
    CAP_UNREGISTER_PROVIDER,
    "host.context",
    "host.conversation",
    "host.tools",
    "host.tasks",
    "host.models",
    "host.workflows",
    "host.secrets",
    "views",
];

/// Caps on untrusted plugin text. Every string is bounded before it reaches the UI or the agent.
pub const MAX_UI_LINE_CHARS: usize = 240;
pub const MAX_UI_LINES: usize = 8;
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
pub const MAX_ACTIONS_PER_REPLY: usize = 16;
pub const MAX_AUTOCOMPLETE_ITEMS: usize = 20;
pub const MAX_NAME_CHARS: usize = 64;
/// Declarative renderer bounds for `append_entry`.
pub const MAX_RENDER_FIELDS: usize = 12;
pub const MAX_RENDER_MARKDOWN_BYTES: usize = 4 * 1024;

/// The negotiated protocol of one plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub version: u8,
    pub capabilities: BTreeSet<String>,
}

impl Negotiated {
    /// Protocol 1 plugins: no capabilities and no v2 actions.
    pub fn legacy() -> Self {
        Self {
            version: 1,
            capabilities: BTreeSet::new(),
        }
    }

    pub fn has(&self, capability: &str) -> bool {
        self.version >= 2 && self.capabilities.contains(capability)
    }
}

/// Reads the `protocolVersion` and `capabilities` of an `initialize` result. A missing version is
/// protocol 1. Unknown capability names are ignored.
pub fn negotiate(initialize: &Value) -> anyhow::Result<Negotiated> {
    let version = match initialize.get("protocolVersion") {
        None | Some(Value::Null) => "1",
        Some(Value::String(v)) => v.as_str(),
        Some(_) => anyhow::bail!("protocolVersion must be a string"),
    };
    anyhow::ensure!(
        SUPPORTED_PROTOCOL_VERSIONS.contains(&version),
        "unsupported plugin protocol version {version:?}; this host speaks 1 and 2"
    );
    if version == "1" {
        return Ok(Negotiated::legacy());
    }
    let capabilities = initialize
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| KNOWN_CAPABILITIES.contains(name))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(Negotiated {
        version: 2,
        capabilities,
    })
}

/// Declarative transcript view of an appended entry; see [`parse_renderer`] for the bounds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntryRenderer {
    pub title: Option<String>,
    pub fields: Vec<(String, String)>,
    pub markdown: Option<String>,
}

/// The `initialize` params a host sends to every plugin.
pub fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": HOST_PROTOCOL_VERSION,
        "supportedProtocolVersions": SUPPORTED_PROTOCOL_VERSIONS,
        "capabilities": {},
    })
}

/// One declarative change a plugin asked for. Applying it is the owner's job; this type only
/// proves the shape and bounds of the request.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Ui(UiSlot),
    SendUserMessage {
        text: String,
    },
    AppendEntry {
        kind: String,
        text: String,
        renderer: Option<EntryRenderer>,
    },
    SetModel {
        model: String,
        effort: Option<String>,
    },
    SetThinking {
        effort: String,
    },
    RegisterProvider {
        name: String,
        config: Value,
    },
    UnregisterProvider {
        name: String,
    },
}

impl Action {
    /// The capability that authorizes this action kind.
    pub fn capability(&self) -> &'static str {
        match self {
            Action::Ui(slot) => slot.capability(),
            Action::SendUserMessage { .. } => CAP_SEND_USER_MESSAGE,
            Action::AppendEntry { .. } => CAP_APPEND_ENTRY,
            Action::SetModel { .. } => CAP_SET_MODEL,
            Action::SetThinking { .. } => CAP_SET_THINKING,
            Action::RegisterProvider { .. } => CAP_REGISTER_PROVIDER,
            Action::UnregisterProvider { .. } => CAP_UNREGISTER_PROVIDER,
        }
    }
}

/// A sanitized, bounded UI contribution. Lines are plain text; control characters are removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSlot {
    pub slot: Slot,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    Status,
    Header,
    Footer,
    Title,
}

impl UiSlot {
    fn capability(&self) -> &'static str {
        match self.slot {
            Slot::Status => CAP_UI_STATUS,
            Slot::Header => CAP_UI_HEADER,
            Slot::Footer => CAP_UI_FOOTER,
            Slot::Title => CAP_UI_TITLE,
        }
    }
}

/// Removes control characters and clamps to [`MAX_UI_LINE_CHARS`]. Plugin text is untrusted.
pub fn sanitize_line(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_UI_LINE_CHARS)
        .collect()
}

/// Like [`sanitize_line`] but keeps newlines, for bounded multi-line bodies.
pub fn sanitize_block(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// Parses the `actions` array of one reply. Actions the plugin did not declare capability for are
/// refused, as are malformed or oversized entries. Refused entries are reported, not applied.
pub fn parse_actions(negotiated: &Negotiated, reply: &Value) -> (Vec<Action>, Vec<String>) {
    let mut actions = Vec::new();
    let mut refused = Vec::new();
    let Some(items) = reply.get("actions").and_then(Value::as_array) else {
        return (actions, refused);
    };
    for (index, item) in items.iter().enumerate() {
        if index >= MAX_ACTIONS_PER_REPLY {
            refused.push(format!(
                "more than {MAX_ACTIONS_PER_REPLY} actions in one reply; the rest were dropped"
            ));
            break;
        }
        match parse_action(item) {
            Ok(action) if negotiated.has(action.capability()) => actions.push(action),
            Ok(action) => refused.push(format!(
                "action {:?} needs capability {} which the plugin did not declare",
                action_kind(item),
                action.capability()
            )),
            Err(error) => refused.push(format!("action {index} refused: {error:#}")),
        }
    }
    (actions, refused)
}

fn action_kind(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("?")
}

fn parse_action(item: &Value) -> anyhow::Result<Action> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing type"))?;
    match kind {
        "ui" => parse_ui(item).map(Action::Ui),
        "send_user_message" => {
            let text = required_str(item, "text")?;
            anyhow::ensure!(
                text.len() <= MAX_MESSAGE_BYTES,
                "message exceeds {MAX_MESSAGE_BYTES} bytes"
            );
            anyhow::ensure!(!text.trim().is_empty(), "message is empty");
            Ok(Action::SendUserMessage {
                text: text.to_owned(),
            })
        }
        "append_entry" => {
            let kind = name_field(item, "kind")?;
            let text = required_str(item, "text")?;
            anyhow::ensure!(
                text.len() <= MAX_ENTRY_BYTES,
                "entry exceeds {MAX_ENTRY_BYTES} bytes"
            );
            let renderer = match item.get("renderer") {
                None | Some(Value::Null) => None,
                Some(value) => Some(parse_renderer(value)?),
            };
            Ok(Action::AppendEntry {
                kind,
                text: text.to_owned(),
                renderer,
            })
        }
        "set_model" => {
            let model = name_field(item, "model")?;
            let effort = optional_name(item, "effort")?;
            Ok(Action::SetModel { model, effort })
        }
        "set_thinking" => Ok(Action::SetThinking {
            effort: name_field(item, "effort")?,
        }),
        "register_provider" => {
            let name = name_field(item, "name")?;
            let config = item
                .get("config")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("config must be an object"))?;
            Ok(Action::RegisterProvider { name, config })
        }
        "unregister_provider" => Ok(Action::UnregisterProvider {
            name: name_field(item, "name")?,
        }),
        other => anyhow::bail!("unknown action type {other:?}"),
    }
}

fn parse_ui(item: &Value) -> anyhow::Result<UiSlot> {
    let slot = match required_str(item, "slot")? {
        "status" => Slot::Status,
        "header" => Slot::Header,
        "footer" => Slot::Footer,
        "title" => Slot::Title,
        other => anyhow::bail!("unknown ui slot {other:?}"),
    };
    let raw: Vec<&str> = match item.get("lines") {
        Some(Value::Array(lines)) => lines
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("ui lines must be strings"))
            })
            .collect::<anyhow::Result<_>>()?,
        Some(_) => anyhow::bail!("ui lines must be an array"),
        None => Vec::new(),
    };
    let limit = match slot {
        Slot::Status | Slot::Title => 1,
        Slot::Header | Slot::Footer => MAX_UI_LINES,
    };
    anyhow::ensure!(raw.len() <= limit, "{slot:?} takes at most {limit} line(s)");
    Ok(UiSlot {
        slot,
        lines: raw.into_iter().map(sanitize_line).collect(),
    })
}

/// Declarative view of an appended entry. Plugins send data only: every string is stripped of
/// control characters and bounded, so no layout language or escape sequence reaches the terminal.
fn parse_renderer(value: &Value) -> anyhow::Result<EntryRenderer> {
    anyhow::ensure!(value.is_object(), "renderer must be an object");
    let title = match value.get("title") {
        None | Some(Value::Null) => None,
        Some(_) => Some(sanitize_line(required_str(value, "title")?)),
    };
    let fields = match value.get("fields") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            anyhow::ensure!(
                items.len() <= MAX_RENDER_FIELDS,
                "renderer takes at most {MAX_RENDER_FIELDS} fields"
            );
            items
                .iter()
                .map(|field| {
                    Ok((
                        sanitize_line(required_str(field, "label")?),
                        sanitize_line(required_str(field, "value")?),
                    ))
                })
                .collect::<anyhow::Result<_>>()?
        }
        Some(_) => anyhow::bail!("renderer fields must be an array"),
    };
    let markdown = match value.get("markdown") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let raw = required_str(value, "markdown")?;
            anyhow::ensure!(
                raw.len() <= MAX_RENDER_MARKDOWN_BYTES,
                "renderer markdown exceeds {MAX_RENDER_MARKDOWN_BYTES} bytes"
            );
            Some(sanitize_block(raw))
        }
    };
    Ok(EntryRenderer {
        title,
        fields,
        markdown,
    })
}

fn required_str<'a>(item: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    item.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{key} must be a string"))
}

/// Names of providers, models, kinds and efforts are short identifiers.
fn name_field(item: &Value, key: &str) -> anyhow::Result<String> {
    let value = required_str(item, key)?;
    anyhow::ensure!(valid_name(value), "{key} is not a valid name");
    Ok(value.to_owned())
}

fn optional_name(item: &Value, key: &str) -> anyhow::Result<Option<String>> {
    match item.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => name_field(item, key).map(Some),
    }
}

/// Identifier rule: ASCII letters, digits and `._-/:`, nonempty, at most [`MAX_NAME_CHARS`]. `/`
/// separates non-empty segments that are neither `.` nor `..`, so a name cannot climb paths.
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_NAME_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':'))
        && value
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// One autocomplete suggestion from a plugin. Text is sanitized and clamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub value: String,
    pub label: String,
    pub description: String,
}

/// Parses an `autocomplete` reply `{"items":[{"value","label","description"}]}`. Malformed items
/// are skipped; the result is capped at [`MAX_AUTOCOMPLETE_ITEMS`].
pub fn parse_suggestions(reply: &Value) -> Vec<Suggestion> {
    reply
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let value = item.get("value").and_then(Value::as_str)?;
            let value = sanitize_line(value);
            if value.trim().is_empty() {
                return None;
            }
            let label = item
                .get("label")
                .and_then(Value::as_str)
                .map_or_else(|| value.clone(), sanitize_line);
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .map(sanitize_line)
                .unwrap_or_default();
            Some(Suggestion {
                value,
                label,
                description,
            })
        })
        .take(MAX_AUTOCOMPLETE_ITEMS)
        .collect()
}

#[cfg(test)]
mod tests;
