//! A toolkit-neutral conversation view built from the engine's messages and events.
//!
//! [`Client`](crate::Client) deliberately renders nothing. [`Conversation`] is the shared step
//! every frontend would otherwise write itself: it folds persisted history (`messages`,
//! `replay_message`) and live events (`text`, `reasoning`, `tool_*`, `approval`, `question`,
//! `notice`, `todo`, …) into an ordered list of [`Entry`] values plus the pending approvals,
//! the open question and the todo list. A GUI draws `entries()` however it likes.
//!
//! The view is a projection, not a source of truth. When events are lost, replace both history
//! and live state with [`Conversation::load_snapshot`]. History alone cannot restore prompts.
//! A new connection starts a new sequence: call [`Conversation::new_connection`] with its
//! `ready` frame before feeding its messages.
use crate::{ApprovalRequest, Event, Message, Ready, RequestId};
use serde_json::Value;
use std::collections::HashMap;

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
    /// A provider retry discarded this partial attempt. It stays in the view for display, but its
    /// text never stands in for the turn's answer.
    pub abandoned: bool,
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
    /// Latest live status of the running call (`tool_progress`); each event replaces the last.
    /// Cleared when the call settles.
    pub progress: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Result text (clipped in events; complete in history).
    pub text: String,
    /// Conservative failure flag: true for a known failure or an unknown outcome.
    /// Use [`ToolOutcome::succeeded`] to distinguish failure from missing metadata.
    pub is_error: bool,
    /// Whether the engine supplied an explicit `is_error` or a recognized settled `status`.
    /// False after a lost connection, for legacy content alone, or an unrecognized status alone.
    pub outcome_known: bool,
    /// `ok`, `error`, `blocked`, `skipped`, `timeout` or `aborted`, when the engine says.
    /// `None` does not establish success; consult `is_error` conservatively.
    pub status: Option<String>,
    /// Whether the call's side effects were `none`, `known` or `unknown`, when the engine says.
    /// A timeout or abort can leave them unknown.
    pub effects: Option<String>,
    /// Absolute paths the call changed.
    pub changed_paths: Vec<String>,
    /// Structured result data the engine attached, when present.
    pub data: Option<Value>,
}

impl Default for ToolOutcome {
    fn default() -> Self {
        Self {
            text: String::new(),
            is_error: true,
            outcome_known: false,
            status: None,
            effects: None,
            changed_paths: Vec::new(),
            data: None,
        }
    }
}

impl ToolOutcome {
    /// Whether the call succeeded, or `None` when legacy history omitted its outcome.
    /// This avoids presenting the conservative `is_error` fallback as a known failure.
    pub fn succeeded(&self) -> Option<bool> {
        self.outcome_known.then_some(!self.is_error)
    }
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
    /// The request whose response settles the running turn, when the frontend tracked one.
    turn_request: Option<RequestId>,
    /// Whether assistant text streamed during the current turn. A response's text is shown
    /// only when nothing streamed, so an earlier attempt's partial text never hides it.
    turn_streamed: bool,
    needs_reload: bool,
    last_seq: Option<u64>,
    /// Row index per tool call id; the newest row for an id wins.
    tool_rows: HashMap<String, usize>,
    /// Rows of tool calls whose result has not settled yet, oldest first.
    running_tools: Vec<usize>,
    /// Assistant fragments and argument-only drafts produced since the last attempt boundary.
    attempt_rows: Vec<usize>,
    speculative_tools: Vec<usize>,
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
    /// Events were lost: reload both history and live state with [`Conversation::load_snapshot`].
    pub fn needs_reload(&self) -> bool {
        self.needs_reload
    }

    /// Replace the rows with persisted history (the `messages` result, or `messages_page` pages
    /// concatenated in order). Does not clear [`Conversation::needs_reload`]: pending approvals,
    /// the open question and todo list require an authoritative live snapshot as well.
    pub fn load_history(&mut self, session_id: Option<&str>, messages: &[Value]) {
        if let Some(id) = session_id {
            self.session_id = Some(id.to_owned());
        }
        self.clear_rows();
        for message in messages {
            self.push_message(message);
        }
    }

    /// Replace history and live state from one authoritative, same-connection snapshot.
    ///
    /// The snapshot must contain `session_id`, `seq` (the last event covered), `messages`,
    /// `busy`, `pending_approvals`, `question` (explicit null if absent), and `todo`. Empty lists
    /// must be explicit too. Older engines' `state` replies are not complete snapshots.
    ///
    /// Apply all messages preceding the snapshot response before calling this method: use
    /// `request`/`poll`, or drain `call`'s deferred messages first. History and live state must
    /// have been captured together, not assembled from independently racing queries.
    /// Returns false without changing the view for incomplete, stale or wrong-session data.
    pub fn load_snapshot(&mut self, snapshot: &Value) -> bool {
        let Some(session) = snapshot["session_id"].as_str().filter(|id| !id.is_empty()) else {
            return false;
        };
        let Some(seq) = snapshot["seq"].as_u64() else {
            return false;
        };
        let Some(messages) = snapshot["messages"].as_array() else {
            return false;
        };
        let Some(busy) = snapshot["busy"].as_bool() else {
            return false;
        };
        let Some(approvals) = snapshot["pending_approvals"].as_array() else {
            return false;
        };
        let Some(question) = snapshot.get("question") else {
            return false;
        };
        let Some(todo) = snapshot["todo"].as_array() else {
            return false;
        };
        if self.last_seq.is_some_and(|last| seq < last)
            || self.session_id.as_deref().is_some_and(|id| id != session)
            || !(question.is_null()
                || question["question_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()))
        {
            return false;
        }
        let mut pending = Vec::new();
        for approval in approvals {
            if !approval["approval_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
                || !approval["tool"].is_string()
                || !approval["summary"].is_string()
            {
                return false;
            }
            let event = Event {
                kind: "approval".into(),
                body: approval.clone(),
            };
            pending.push(PendingApproval {
                request: event.approval().expect("approval event"),
                call_id: approval["call_id"].as_str().map(str::to_owned),
            });
        }
        self.load_history(Some(session), messages);
        self.approvals = pending;
        self.question = (!question.is_null()).then(|| question.clone());
        self.todo.clone_from(todo);
        self.turn_running = busy;
        self.turn_request = None;
        self.turn_streamed = false;
        self.last_seq = Some(seq);
        self.needs_reload = false;
        true
    }

    /// Start a new connection's event sequence, with its `ready` frame. Call it after the old
    /// connection's messages ([`Handoff::poll`](crate::Handoff::poll)) and before the new
    /// connection's.
    ///
    /// Ids from the old connection are void, so its pending approvals, open question and turn
    /// tracking are dropped, and a streaming answer is settled. A turn still running is marked
    /// with an error row, because its outcome is unknown. Rows already shown and tool results
    /// are kept. The `ready` frame's `seq` becomes the baseline, so the first event after it
    /// continues without a gap, and a gap the old connection left stays in
    /// [`Conversation::needs_reload`]. A reconnect to the same session keeps its rows, since
    /// nothing is replayed; a different session replaces them as `active_session` does.
    pub fn new_connection(&mut self, ready: &Ready) {
        self.last_seq = ready.raw["seq"].as_u64();
        self.void_connection_state();
        if ready.session_id.is_empty() {
            return;
        }
        // A view that never named a session keeps its rows: they came from this same session.
        if self.session_id.is_none() {
            self.session_id = Some(ready.session_id.clone());
        } else {
            self.switch_session(&ready.session_id);
        }
    }

    /// Drop what only the old connection could answer, and settle what it left running.
    fn void_connection_state(&mut self) {
        self.finish_streaming();
        if std::mem::take(&mut self.turn_running) {
            self.entries.push(Entry::Error(
                "the turn was still running when its connection was replaced; its outcome is unknown"
                    .into(),
            ));
        }
        self.turn_request = None;
        self.turn_streamed = false;
        self.approvals.clear();
        self.question = None;
        self.attempt_rows.clear();
        self.speculative_tools.clear();
        for index in std::mem::take(&mut self.running_tools) {
            self.settle_tool(
                index,
                ToolOutcome {
                    text: "Connection lost; the tool outcome is unknown".into(),
                    effects: Some("unknown".into()),
                    ..ToolOutcome::default()
                },
            );
        }
    }

    /// Show a prompt the frontend is about to `send`, and mark the turn running. The turn is not
    /// correlated with a request: any turn response (`send`, `retry`, `live_listen`) settles it.
    /// Prefer [`Conversation::push_turn`], which ignores replies to unrelated requests.
    pub fn push_user(&mut self, text: &str, images: usize) {
        self.finish_streaming();
        self.entries.push(Entry::User(UserEntry {
            text: text.to_owned(),
            images,
        }));
        self.turn_running = true;
        self.turn_request = None;
        self.turn_streamed = false;
        self.attempt_rows.clear();
    }

    /// Show a prompt sent with [`Client::send`](crate::Client::send) and track its request: only
    /// the response with that id settles the turn, so a refusal of an unrelated request cannot
    /// end it.
    pub fn push_turn(&mut self, text: &str, images: usize, request: RequestId) {
        self.push_user(text, images);
        self.turn_request = Some(request);
    }

    /// Track a turn started by a request other than `send` (for example `retry`), so that only its
    /// response settles the turn.
    pub fn expect_turn(&mut self, request: RequestId) {
        self.finish_streaming();
        self.turn_running = true;
        self.turn_request = Some(request);
        self.turn_streamed = false;
        self.attempt_rows.clear();
    }

    /// Fold one polled message in. Returns whether the view changed.
    pub fn apply(&mut self, message: &Message) -> bool {
        match message {
            Message::Event(event) => self.apply_event(event),
            Message::Response { id, method, result }
                if self.turn_request.map_or_else(
                    || TURN_METHODS.contains(&method.as_str()),
                    |expected| expected == *id,
                ) =>
            {
                self.settle_turn(result.as_ref().map_err(ToString::to_string));
                true
            }
            Message::Response { .. } => false,
            Message::ProtocolError(error) => {
                self.entries.push(Entry::Error(error.to_string()));
                true
            }
            Message::Disconnected { .. } => {
                self.void_connection_state();
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
            // A successful top-level model request: `state_node` from a tree-event client,
            // `inference_done` once the client turned tree events off (`Launch::tree_events`).
            "state_node"
                if body["node"]["kind"] == "inference"
                    && body["node"]["parent"].is_null()
                    && body["node"]["result"]["phase"] == "completed" =>
            {
                self.end_model_request()
            }
            "inference_done" => self.end_model_request(),
            "text" => {
                self.turn_streamed = true;
                self.assistant().text.push_str(text("delta"));
                true
            }
            "reasoning" => {
                self.assistant().reasoning.push_str(text("delta"));
                true
            }
            "tool_arguments" => {
                let new = !self.tool_rows.contains_key(text("id"));
                let index = self.tool_row(text("id"));
                if new {
                    self.speculative_tools.push(index);
                }
                let tool = self.tool_at(index);
                if tool.name.is_empty() {
                    text("name").clone_into(&mut tool.name);
                }
                tool.arguments.push_str(text("delta"));
                true
            }
            "tool_call" => {
                let index = self.tool_row(text("call_id"));
                self.speculative_tools.retain(|&draft| draft != index);
                let tool = self.tool_at(index);
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
                // Route by call id: two calls of one tool name each keep their own output. Engines
                // that predate call ids name only the tool, which belongs to the newest running call.
                let name = text("name");
                let call_id = text("call_id");
                let index = if call_id.is_empty() {
                    self.running_tools.iter().rev().copied().find(|&index| {
                        matches!(&self.entries[index], Entry::Tool(tool) if tool.name == name)
                    })
                } else {
                    Some(self.tool_row(call_id))
                };
                match index {
                    Some(index) => {
                        let tool = self.tool_at(index);
                        if tool.name.is_empty() {
                            name.clone_into(&mut tool.name);
                        }
                        tool.output.push_str(text("delta"));
                        true
                    }
                    None => false,
                }
            }
            "tool_result" => {
                let outcome = tool_outcome(body, text("text").to_owned());
                let index = self.tool_row(text("call_id"));
                let tool = self.tool_at(index);
                if tool.name.is_empty() {
                    text("name").clone_into(&mut tool.name);
                }
                self.settle_tool(index, outcome);
                true
            }
            "tool_progress" => {
                // A replace-latest snapshot for one running call; it is meaningless without an id.
                let call_id = text("call_id");
                if call_id.is_empty() {
                    return false;
                }
                let index = self.tool_row(call_id);
                let tool = self.tool_at(index);
                if tool.result.is_some() {
                    return false;
                }
                tool.progress = Some(text("text").to_owned());
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
                if body["kind"] == "retry" {
                    self.abandon_attempt();
                } else {
                    self.finish_streaming();
                }
                self.entries.push(Entry::Notice(NoticeEntry {
                    message: text("message").to_owned(),
                    kind: body["kind"].as_str().map(str::to_owned),
                    lane: body["lane"].as_str().map(str::to_owned),
                }));
                true
            }
            "collaboration_turn" => {
                let result = match text("outcome") {
                    "completed" => Ok(&body["result"]),
                    "failed" => Err(body["error"]["message"]
                        .as_str()
                        .unwrap_or("Remote turn failed")
                        .to_owned()),
                    _ => return false,
                };
                if self.turn_request.is_some() {
                    // The remote completion is not a response to the tracked local request.
                    self.show_turn_result(result, false);
                } else {
                    self.settle_turn(result);
                }
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
                self.switch_session(text("session_id"))
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
            self.attempt_rows.push(self.entries.len() - 1);
        }
        match self.entries.last_mut() {
            Some(Entry::Assistant(assistant)) => assistant,
            _ => unreachable!("an assistant row was just ensured"),
        }
    }

    /// The tool row for `call_id`, appended when it is new.
    fn tool(&mut self, call_id: &str) -> &mut ToolEntry {
        let index = self.tool_row(call_id);
        self.tool_at(index)
    }

    /// The index of the tool row for `call_id`, appended when it is new.
    fn tool_row(&mut self, call_id: &str) -> usize {
        if let Some(&index) = self.tool_rows.get(call_id) {
            return index;
        }
        self.finish_streaming();
        self.push_tool(ToolEntry {
            call_id: call_id.to_owned(),
            ..ToolEntry::default()
        })
    }

    /// Append a tool row, index it by call id and track it as running until it settles.
    fn push_tool(&mut self, tool: ToolEntry) -> usize {
        let settled = tool.result.is_some();
        let call_id = tool.call_id.clone();
        self.entries.push(Entry::Tool(Box::new(tool)));
        let index = self.entries.len() - 1;
        self.tool_rows.insert(call_id, index);
        if !settled {
            self.running_tools.push(index);
        }
        index
    }

    fn tool_at(&mut self, index: usize) -> &mut ToolEntry {
        match &mut self.entries[index] {
            Entry::Tool(tool) => tool.as_mut(),
            _ => unreachable!("index points at a tool row"),
        }
    }

    fn settle_tool(&mut self, index: usize, outcome: ToolOutcome) {
        let tool = self.tool_at(index);
        tool.result = Some(outcome);
        tool.progress = None;
        self.running_tools.retain(|&running| running != index);
        self.speculative_tools.retain(|&draft| draft != index);
    }

    /// A provider retry discarded the attempt streamed so far. Its streaming row is kept, marked
    /// abandoned, and the next attempt's text opens a fresh row. Nothing the abandoned attempt
    /// streamed may stand in for the turn's answer, so the turn's text flag is reset.
    fn abandon_attempt(&mut self) {
        for index in std::mem::take(&mut self.attempt_rows) {
            if let Entry::Assistant(assistant) = &mut self.entries[index] {
                assistant.streaming = false;
                assistant.abandoned = true;
            }
        }
        for index in std::mem::take(&mut self.speculative_tools) {
            let call_id = self.tool_at(index).call_id.clone();
            self.settle_tool(
                index,
                ToolOutcome {
                    text: "Provider retry discarded these arguments before execution".into(),
                    is_error: true,
                    outcome_known: true,
                    status: Some("skipped".into()),
                    effects: Some("none".into()),
                    ..ToolOutcome::default()
                },
            );
            // A provider may reuse a call id in its next attempt.
            self.tool_rows.remove(&call_id);
        }
        self.turn_streamed = false;
    }

    /// Drop every row and the tool routing built over them.
    fn clear_rows(&mut self) {
        self.entries.clear();
        self.tool_rows.clear();
        self.running_tools.clear();
        self.attempt_rows.clear();
        self.speculative_tools.clear();
    }

    /// Make `id` the shown session. A change drops the old session's rows and live prompts; it
    /// returns whether the session changed.
    fn switch_session(&mut self, id: &str) -> bool {
        if self.session_id.as_deref() == Some(id) {
            return false;
        }
        self.session_id = Some(id.to_owned());
        self.clear_rows();
        self.approvals.clear();
        self.question = None;
        self.todo.clear();
        true
    }

    /// A successful model request is an attempt boundary too. A retry in a later request must
    /// not abandon assistant fragments from earlier completed requests.
    fn end_model_request(&mut self) -> bool {
        self.finish_streaming();
        self.attempt_rows.clear();
        true
    }

    fn finish_streaming(&mut self) {
        for index in self.attempt_rows.iter().copied() {
            if let Entry::Assistant(assistant) = &mut self.entries[index] {
                assistant.streaming = false;
            }
        }
        if let Some(Entry::Assistant(assistant)) = self.entries.last_mut() {
            assistant.streaming = false;
        }
    }

    /// A turn's response: keep its final text when nothing streamed in this turn, report failures.
    fn settle_turn(&mut self, result: Result<&Value, String>) {
        self.finish_streaming();
        self.turn_running = false;
        self.turn_request = None;
        self.attempt_rows.clear();
        let streamed = std::mem::take(&mut self.turn_streamed);
        self.show_turn_result(result, streamed);
    }

    fn show_turn_result(&mut self, result: Result<&Value, String>, streamed: bool) {
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.entries.push(Entry::Error(error));
                return;
            }
        };
        let text = result["text"].as_str().unwrap_or_default();
        if !streamed && !text.is_empty() {
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
                        abandoned: false,
                    }));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let function = &call["function"];
                    let arguments = match &function["arguments"] {
                        Value::String(text) => text.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    self.push_tool(ToolEntry {
                        call_id: call["id"].as_str().unwrap_or_default().to_owned(),
                        name: function["name"].as_str().unwrap_or_default().to_owned(),
                        arguments,
                        ..ToolEntry::default()
                    });
                }
            }
            Some("tool") => {
                let (text, _) = content_text(&message["content"]);
                let index = self.tool_row(message["tool_call_id"].as_str().unwrap_or_default());
                self.settle_tool(index, tool_outcome(message, text));
            }
            _ => {}
        }
    }
}

/// Preserve structured outcomes when available; legacy persisted content alone cannot prove
/// that a tool succeeded. Never infer success or failure from human-readable output text.
fn tool_outcome(body: &Value, text: String) -> ToolOutcome {
    let status = body["status"].as_str().map(str::to_owned);
    let explicit_error = body["is_error"].as_bool();
    let status_error = match status.as_deref() {
        Some("ok") => Some(false),
        Some("error" | "blocked" | "skipped" | "timeout" | "aborted") => Some(true),
        _ => None,
    };
    let outcome_known = explicit_error.is_some() || status_error.is_some();
    ToolOutcome {
        text,
        outcome_known,
        is_error: explicit_error.unwrap_or(false)
            || status_error.unwrap_or(false)
            || !outcome_known,
        status,
        effects: body["effects"].as_str().map(str::to_owned),
        changed_paths: strings(&body["changed_paths"]),
        data: body.get("data").filter(|data| !data.is_null()).cloned(),
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
