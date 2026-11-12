//! A toolkit-neutral conversation view built from the engine's messages and events.
//!
//! [`Client`](crate::Client) deliberately renders nothing. [`Conversation`] is the shared step
//! every frontend would otherwise write itself: it folds persisted history (`messages`,
//! `replay_message`) and live events (`text`, `reasoning`, `tool_*`, `approval`, `question`,
//! `notice`, `todo`, …) into an ordered list of [`Entry`] values plus the pending approvals,
//! the open question and the todo list. A GUI draws `entries()` however it likes.
//!
//! The view is a projection, not a source of truth. When it cannot be trusted (an event sequence
//! gap, see [`Conversation::needs_reload`]), reload history with `messages` and call
//! [`Conversation::load_history`].
use crate::{ApprovalRequest, Event, Message};
use serde_json::Value;

/// One row of the conversation, in display order.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A user prompt.
    User(UserEntry),
    /// Assistant text, with any reasoning streamed before it.
    Assistant(AssistantEntry),
    /// One tool call and, once it settles, its result.
    Tool(Box<ToolEntry>),
    /// An engine notice (`notice`, or an interrupted turn).
    Notice(NoticeEntry),
    /// A failure: a refused or failed turn, a `fatal` event or a lost connection.
    Error(String),
}

/// A user prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserEntry {
    /// The prompt text.
    pub text: String,
    /// Number of attached images.
    pub images: usize,
}

/// Assistant output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssistantEntry {
    /// Visible answer text.
    pub text: String,
    /// Reasoning text, when the model streams it.
    pub reasoning: String,
    /// Still receiving deltas.
    pub streaming: bool,
}

/// A tool call.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolEntry {
    /// Engine call id.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// One-line summary from `tool_call`.
    pub summary: String,
    /// Arguments as streamed (`tool_arguments`) or recorded in history (JSON text).
    pub arguments: String,
    /// Live output streamed while the call runs (`tool_output`).
    pub output: String,
    /// The engine-built card; render it instead of parsing `summary`.
    pub card: Option<ToolCard>,
    /// The settled result; `None` while the call runs.
    pub result: Option<ToolOutcome>,
}

/// The card the terminal renders for a call (`tool_card`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCard {
    /// Short action label.
    pub label: String,
    /// What the call acts on.
    pub target: String,
    /// Colour family (`tools`, `shell`, `edit`, `connect`, `workers`).
    pub tone: Option<String>,
    /// Body style (`text`, `quiet`, `diff`, `write`, `plan`, `workers`).
    pub body: Option<String>,
    /// Key facts as sent.
    pub facts: Vec<Value>,
    /// Compact preview text.
    pub preview: Option<String>,
}

/// A settled tool call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolOutcome {
    /// Result text (clipped in events; complete in history).
    pub text: String,
    /// The call failed.
    pub is_error: bool,
    /// `ok`, `error`, `blocked`, `skipped`, `timeout` or `aborted`, when the engine says.
    pub status: Option<String>,
    /// Absolute paths the call changed.
    pub changed_paths: Vec<String>,
}

/// An engine notice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoticeEntry {
    /// Readable text.
    pub message: String,
    /// Typed kind, when the engine sends one (`interrupted` for an interrupted turn).
    pub kind: Option<String>,
    /// `context`, `divider` or `alert`, when sent.
    pub lane: Option<String>,
}

/// A pending approval with the call it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// The request to answer with [`Client::approve`](crate::Client::approve).
    pub request: ApprovalRequest,
    /// The tool call waiting, when the engine names it.
    pub call_id: Option<String>,
}

/// Methods whose response ends a turn.
const TURN_METHODS: &[&str] = &["send", "retry", "live_listen"];

/// The conversation as a frontend shows it. Feed it every [`Message`] the client polls.
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    session_id: Option<String>,
    entries: Vec<Entry>,
    approvals: Vec<PendingApproval>,
    question: Option<Value>,
    todo: Vec<Value>,
    turn_running: bool,
    needs_reload: bool,
    last_seq: Option<u64>,
}

impl Conversation {
    /// An empty view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rows in display order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Session the view shows, once known.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Approvals waiting for an answer, oldest first.
    pub fn pending_approvals(&self) -> &[PendingApproval] {
        &self.approvals
    }

    /// The open question's `request` object, if one is waiting.
    pub fn question(&self) -> Option<&Value> {
        self.question.as_ref()
    }

    /// The latest todo list (`todo` event `tasks`).
    pub fn todo(&self) -> &[Value] {
        &self.todo
    }

    /// A turn started by [`Conversation::push_user`] has not answered yet.
    pub fn turn_running(&self) -> bool {
        self.turn_running
    }

    /// Events were lost (a `seq` gap): reload `messages` and call [`Conversation::load_history`].
    pub fn needs_reload(&self) -> bool {
        self.needs_reload
    }

    /// Replace the rows with persisted history (the `messages` result, or `messages_page` pages
    /// concatenated in order). Clears [`Conversation::needs_reload`]; pending approvals, the open
    /// question and the todo list are live state and are kept.
    pub fn load_history(&mut self, session_id: Option<&str>, messages: &[Value]) {
        if let Some(id) = session_id {
            self.session_id = Some(id.to_owned());
        }
        self.entries.clear();
        for message in messages {
            self.push_message(message);
        }
        self.needs_reload = false;
    }

    /// Show a prompt the frontend is about to `send`, and mark the turn running.
    pub fn push_user(&mut self, text: &str, images: usize) {
        self.finish_streaming();
        self.entries.push(Entry::User(UserEntry {
            text: text.to_owned(),
            images,
        }));
        self.turn_running = true;
    }

    /// Fold one polled message in. Returns whether the view changed.
    pub fn apply(&mut self, message: &Message) -> bool {
        match message {
            Message::Event(event) => self.apply_event(event),
            Message::Response { method, result, .. } if TURN_METHODS.contains(&method.as_str()) => {
                self.settle_turn(result.as_ref().map_err(ToString::to_string));
                true
            }
            Message::Response { .. } => false,
            Message::ProtocolError(error) => {
                self.entries.push(Entry::Error(error.to_string()));
                true
            }
            Message::Disconnected { .. } => {
                self.finish_streaming();
                self.turn_running = false;
                self.approvals.clear();
                self.question = None;
                self.entries
                    .push(Entry::Error("the engine connection ended".into()));
                true
            }
        }
    }

    /// Fold one event in. Unknown events are ignored. Returns whether the view changed.
    pub fn apply_event(&mut self, event: &Event) -> bool {
        if let Some(seq) = event.seq() {
            if self.last_seq.is_some_and(|last| seq != last + 1) {
                self.needs_reload = true;
            }
            self.last_seq = Some(seq);
        }
        let body = &event.body;
        let text = |key: &str| body[key].as_str().unwrap_or_default();
        match event.kind.as_str() {
            "ready" => {
                self.session_id = body["session_id"].as_str().map(str::to_owned);
                false
            }
            "text" => {
                self.assistant().text.push_str(text("delta"));
                true
            }
            "reasoning" => {
                self.assistant().reasoning.push_str(text("delta"));
                true
            }
            "tool_arguments" => {
                let tool = self.tool(text("id"));
                if tool.name.is_empty() {
                    text("name").clone_into(&mut tool.name);
                }
                tool.arguments.push_str(text("delta"));
                true
            }
            "tool_call" => {
                let tool = self.tool(text("call_id"));
                text("name").clone_into(&mut tool.name);
                text("summary").clone_into(&mut tool.summary);
                true
            }
            "tool_card" => {
                self.tool(text("call_id")).card = Some(ToolCard {
                    label: text("label").to_owned(),
                    target: text("target").to_owned(),
                    tone: body["tone"].as_str().map(str::to_owned),
                    body: body["body"].as_str().map(str::to_owned),
                    facts: body["facts"].as_array().cloned().unwrap_or_default(),
                    preview: body["preview"].as_str().map(str::to_owned),
                });
                true
            }
            "tool_output" => {
                // Output names the tool, not the call: it belongs to the newest running call.
                let name = text("name");
                let running = self.entries.iter_mut().rev().find_map(|entry| match entry {
                    Entry::Tool(tool) if tool.result.is_none() && tool.name == name => Some(tool),
                    _ => None,
                });
                match running {
                    Some(tool) => {
                        tool.output.push_str(text("delta"));
                        true
                    }
                    None => false,
                }
            }
            "tool_result" => {
                let outcome = ToolOutcome {
                    text: text("text").to_owned(),
                    is_error: body["is_error"].as_bool().unwrap_or(false),
                    status: body["status"].as_str().map(str::to_owned),
                    changed_paths: strings(&body["changed_paths"]),
                };
                let tool = self.tool(text("call_id"));
                if tool.name.is_empty() {
                    text("name").clone_into(&mut tool.name);
                }
                tool.result = Some(outcome);
                true
            }
            "approval" => {
                let Some(request) = event.approval() else {
                    return false;
                };
                self.approvals
                    .retain(|pending| pending.request.approval_id != request.approval_id);
                self.approvals.push(PendingApproval {
                    request,
                    call_id: body["call_id"].as_str().map(str::to_owned),
                });
                true
            }
            "approval_resolved" => {
                let id = text("approval_id");
                let before = self.approvals.len();
                self.approvals
                    .retain(|pending| pending.request.approval_id != id);
                before != self.approvals.len()
            }
            "question" => {
                self.question = Some(body["request"].clone());
                true
            }
            "question_closed" => {
                let open = self
                    .question
                    .as_ref()
                    .is_some_and(|question| question["question_id"] == body["question_id"]);
                if open {
                    self.question = None;
                }
                open
            }
            "notice" => {
                self.finish_streaming();
                self.entries.push(Entry::Notice(NoticeEntry {
                    message: text("message").to_owned(),
                    kind: body["kind"].as_str().map(str::to_owned),
                    lane: body["lane"].as_str().map(str::to_owned),
                }));
                true
            }
            "todo" => {
                self.todo = body["tasks"].as_array().cloned().unwrap_or_default();
                true
            }
            "fatal" => {
                self.finish_streaming();
                self.turn_running = false;
                self.entries.push(Entry::Error(text("message").to_owned()));
                true
            }
            "active_session" => {
                // A session switch: the engine replays the new session's history next.
                let id = text("session_id");
                if self.session_id.as_deref() == Some(id) {
                    return false;
                }
                self.session_id = Some(id.to_owned());
                self.entries.clear();
                self.approvals.clear();
                self.question = None;
                self.todo.clear();
                true
            }
            "replay_message" => {
                self.push_message(&body["message"]);
                true
            }
            _ => false,
        }
    }

    /// The streaming assistant row at the end, started when the last row is anything else.
    fn assistant(&mut self) -> &mut AssistantEntry {
        if !matches!(self.entries.last(), Some(Entry::Assistant(a)) if a.streaming) {
            self.entries.push(Entry::Assistant(AssistantEntry {
                streaming: true,
                ..AssistantEntry::default()
            }));
        }
        match self.entries.last_mut() {
            Some(Entry::Assistant(assistant)) => assistant,
            _ => unreachable!("an assistant row was just ensured"),
        }
    }

    /// The tool row for `call_id`, appended when it is new.
    fn tool(&mut self, call_id: &str) -> &mut ToolEntry {
        let found = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::Tool(tool) if tool.call_id == call_id));
        let index = found.unwrap_or_else(|| {
            self.finish_streaming();
            self.entries.push(Entry::Tool(Box::new(ToolEntry {
                call_id: call_id.to_owned(),
                ..ToolEntry::default()
            })));
            self.entries.len() - 1
        });
        match &mut self.entries[index] {
            Entry::Tool(tool) => tool,
            _ => unreachable!("index points at a tool row"),
        }
    }

    fn finish_streaming(&mut self) {
        if let Some(Entry::Assistant(assistant)) = self.entries.last_mut() {
            assistant.streaming = false;
        }
    }

    /// A turn's response: keep its final text when nothing was streamed, report failures.
    fn settle_turn(&mut self, result: Result<&Value, String>) {
        self.finish_streaming();
        self.turn_running = false;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.entries.push(Entry::Error(error));
                return;
            }
        };
        let answered = self
            .entries
            .iter()
            .rev()
            .take_while(|entry| !matches!(entry, Entry::User(_)))
            .any(|entry| matches!(entry, Entry::Assistant(a) if !a.text.is_empty()));
        let text = result["text"].as_str().unwrap_or_default();
        if !answered && !text.is_empty() {
            self.entries.push(Entry::Assistant(AssistantEntry {
                text: text.to_owned(),
                ..AssistantEntry::default()
            }));
        }
        if result["interrupted"] == true {
            self.entries.push(Entry::Notice(NoticeEntry {
                message: "Turn interrupted".into(),
                kind: Some("interrupted".into()),
                lane: Some("alert".into()),
            }));
        }
    }

    /// One persisted chat message (OpenAI-style `role`, `content`, `tool_calls`, `tool_call_id`).
    fn push_message(&mut self, message: &Value) {
        match message["role"].as_str() {
            Some("user") => {
                let (text, images) = content_text(&message["content"]);
                self.entries.push(Entry::User(UserEntry { text, images }));
            }
            Some("assistant") => {
                let (text, _) = content_text(&message["content"]);
                let reasoning = message["reasoning_content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if !text.is_empty() || !reasoning.is_empty() {
                    self.entries.push(Entry::Assistant(AssistantEntry {
                        text,
                        reasoning,
                        streaming: false,
                    }));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let function = &call["function"];
                    let arguments = match &function["arguments"] {
                        Value::String(text) => text.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    self.entries.push(Entry::Tool(Box::new(ToolEntry {
                        call_id: call["id"].as_str().unwrap_or_default().to_owned(),
                        name: function["name"].as_str().unwrap_or_default().to_owned(),
                        arguments,
                        ..ToolEntry::default()
                    })));
                }
            }
            Some("tool") => {
                let (text, _) = content_text(&message["content"]);
                let tool = self.tool(message["tool_call_id"].as_str().unwrap_or_default());
                tool.result = Some(ToolOutcome {
                    text,
                    ..ToolOutcome::default()
                });
            }
            _ => {}
        }
    }
}

/// Text of a string or content-part array, and the number of image parts.
fn content_text(content: &Value) -> (String, usize) {
    match content {
        Value::String(text) => (text.clone(), 0),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut images = 0;
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => text.push_str(part["text"].as_str().unwrap_or_default()),
                    Some("image_url" | "image") => images += 1,
                    _ => {}
                }
            }
            (text, images)
        }
        _ => (String::new(), 0),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests;
