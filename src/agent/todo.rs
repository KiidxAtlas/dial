//! The `todo` tool: the live task list the agent keeps while working on a multi-step job.
//!
//! State machine: `pending -> in_progress -> completed | abandoned`; any open task may instead be
//! `blocked` with a reason. Whenever a task is pending, exactly one task is `in_progress`: closing
//! the active task starts the earliest pending one, so the list always says what the agent is doing
//! now and the common step ("done, next") is a single call. Tasks may carry a phase label that
//! groups them in the rendered list. The list is session state; `snapshot`/`restore` persist it.
//! Resolved work does not pile up: every mutation prunes closed tasks beyond a short recent tail.
use crate::tools::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_TASKS: usize = 50;
pub const MAX_CONTENT_CHARS: usize = 200;
pub const MAX_PHASE_CHARS: usize = 60;
pub const MAX_FINDING_CHARS: usize = 4_000;
pub const MAX_REASON_CHARS: usize = 4_000;

/// How many completed or abandoned tasks stay listed as context; older closed tasks are pruned.
pub const KEEP_CLOSED: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Abandoned,
    Blocked,
}

impl Status {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
            Self::Abandoned => "[-]",
            Self::Blocked => "[!]",
        }
    }

    /// Work the agent still owes: everything not completed or abandoned.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress | Self::Blocked)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub content: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Why a blocked task is blocked or why a task was abandoned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// A concrete question to resolve before implementation, rather than an implementation step.
    #[serde(default)]
    pub investigation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

impl Task {
    /// `N. content`, with the blocker or abandonment reason when there is one.
    pub fn label(&self, number: usize) -> String {
        match (&self.note, self.status) {
            (Some(note), Status::Blocked) => {
                format!("{number}. {} (blocked: {note})", self.content)
            }
            (Some(note), Status::Abandoned) => {
                format!("{number}. {} (dropped: {note})", self.content)
            }
            _ => format!("{number}. {}", self.content),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoList {
    tasks: Vec<Task>,
}

fn fail(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        execution: Default::default(),
        data: None,
        text: text.into(),
        is_error: true,
    }
}

/// One line, control characters dropped, whitespace collapsed. Task text is model-written and is
/// rendered back into prompts and terminals, so it must not carry newlines or escape sequences.
fn clean_text(raw: &str, limit: usize, what: &str) -> Result<String, String> {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    if text.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if text.chars().count() > limit {
        return Err(format!(
            "{what} is {} chars; the limit is {limit}. Shorten it",
            text.chars().count()
        ));
    }
    Ok(text)
}

fn check_tasks(tasks: &[Task]) -> Result<(), String> {
    if tasks.len() > MAX_TASKS {
        return Err(format!(
            "{} tasks; the limit is {MAX_TASKS}. Merge or drop finished work",
            tasks.len()
        ));
    }
    let active = tasks
        .iter()
        .filter(|t| t.status == Status::InProgress)
        .count();
    if active > 1 {
        return Err(format!(
            "{active} tasks are in_progress; at most one task may be in_progress at a time"
        ));
    }
    Ok(())
}

const OPS_HINT: &str = "Use one call such as {\"op\":\"set\",\"tasks\":[{\"content\":\"...\"}]}, {\"op\":\"done\"}, or {\"op\":\"view\"}.";
const TOP_KEYS: &[&str] = &["op", "tasks", "index", "reason", "finding", "evidence"];
const ITEM_KEYS: &[&str] = &["content", "status", "phase", "investigation"];

/// Parse `tasks` for `set`/`append`. A task without `phase` inherits the phase of the task before it
/// (`inherited` for the first), so a phase is named once at the head of its group.
fn parse_items(args: &Value, op: &str, mut inherited: Option<String>) -> Result<Vec<Task>, String> {
    let items = args.get("tasks").and_then(Value::as_array).ok_or_else(|| {
        format!("op \"{op}\" needs \"tasks\": [{{\"content\": ..., \"phase\": ...}}]")
    })?;
    let mut tasks = Vec::with_capacity(items.len().min(MAX_TASKS + 1));
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        if let Some(key) = item
            .as_object()
            .and_then(|obj| obj.keys().find(|key| !ITEM_KEYS.contains(&key.as_str())))
        {
            return Err(format!(
                "task {n}: unknown key {key:?}. Allowed keys: content, status, phase, investigation. Example: {{\"content\":\"...\"}}"
            ));
        }
        let raw = item
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("task {n}: missing string \"content\""))?;
        let content = clean_text(raw, MAX_CONTENT_CHARS, "task content")
            .map_err(|e| format!("task {n}: {e}"))?;
        let status = match item.get("status") {
            None | Some(Value::Null) => Status::Pending,
            Some(v) => v.as_str().and_then(Status::parse)
                .ok_or_else(|| format!("task {n}: status must be pending, in_progress, completed or abandoned (use op=block for blockers)"))?,
        };
        if let Some(raw) = item.get("phase").and_then(Value::as_str) {
            inherited = Some(
                clean_text(raw, MAX_PHASE_CHARS, "phase").map_err(|e| format!("task {n}: {e}"))?,
            );
        }
        let investigation = match item.get("investigation") {
            None | Some(Value::Null) => false,
            Some(value) => value.as_bool().ok_or("investigation must be a boolean")?,
        };
        tasks.push(Task {
            content,
            status,
            phase: inherited.clone(),
            note: None,
            investigation,
            finding: None,
            evidence: Vec::new(),
        });
    }
    Ok(tasks)
}

/// The 0-based slot `index` names, or the active task when `index` is absent and `active` allows it.
fn target(args: &Value, tasks: &[Task], op: &str, active: bool) -> Result<usize, String> {
    match args.get("index") {
        None | Some(Value::Null) if active => {
            if let Some(slot) = tasks.iter().position(|t| t.status == Status::InProgress) {
                return Ok(slot);
            }
            let mut open = tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.status.is_open())
                .map(|(slot, _)| slot);
            match (open.next(), open.next()) {
                (Some(slot), None) => Ok(slot),
                _ => Err(format!(
                    "no task is in_progress; give op \"{op}\" an \"index\" (example: {{\"op\":\"{op}\",\"index\":1}})"
                )),
            }
        }
        None | Some(Value::Null) => Err(format!(
            "op \"{op}\" needs \"index\": the task number shown in the list"
        )),
        Some(value) => {
            let index = value
                .as_u64()
                .ok_or("\"index\" must be a positive integer (the number shown in the list)")?;
            usize::try_from(index)
                .ok()
                .and_then(|i| i.checked_sub(1))
                .filter(|i| *i < tasks.len())
                .ok_or_else(|| format!("no task {index}: the list has {} task(s)", tasks.len()))
        }
    }
}

fn reason(args: &Value) -> Result<Option<String>, String> {
    match args.get("reason").and_then(Value::as_str) {
        None => Ok(None),
        Some(raw) => clean_text(raw, MAX_REASON_CHARS, "reason").map(Some),
    }
}

fn finding(args: &Value) -> Result<(Option<String>, Vec<String>), String> {
    let answer = args
        .get("finding")
        .filter(|value| !value.is_null())
        .map(|value| {
            clean_text(
                value.as_str().ok_or("finding must be a string")?,
                MAX_FINDING_CHARS,
                "finding",
            )
        })
        .transpose()?;
    let evidence = match args.get("evidence").filter(|value| !value.is_null()) {
        None => Vec::new(),
        Some(value) => {
            let refs = value
                .as_array()
                .filter(|refs| !refs.is_empty() && refs.len() <= 4)
                .ok_or("evidence must contain 1-4 string references")?;
            refs.iter()
                .map(|reference| {
                    clean_text(
                        reference
                            .as_str()
                            .ok_or("evidence references must be strings")?,
                        240,
                        "evidence reference",
                    )
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok((answer, evidence))
}

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Whether any task is still owed (pending, in progress or blocked).
    pub fn has_open(&self) -> bool {
        self.tasks.iter().any(|t| t.status.is_open())
    }

    /// `(closed, total)`: completed or abandoned tasks against all tasks.
    pub fn progress(&self) -> (usize, usize) {
        (
            self.tasks.iter().filter(|t| !t.status.is_open()).count(),
            self.tasks.len(),
        )
    }

    /// Checkbox list, one numbered task per line: `[ ]` pending, `[~]` in progress, `[x]` done,
    /// `[-]` abandoned, `[!]` blocked. Phased tasks are grouped under `Phase:` headings and indented;
    /// numbering is global so it always matches `index`. Empty string for an empty list.
    pub fn render(&self) -> String {
        let phased = self.tasks.iter().any(|t| t.phase.is_some());
        let mut out = Vec::with_capacity(self.tasks.len() * 2);
        let mut current: Option<&str> = None;
        for (i, task) in self.tasks.iter().enumerate() {
            if phased && (i == 0 || task.phase.as_deref() != current) {
                current = task.phase.as_deref();
                out.push(format!("{}:", current.unwrap_or("Other")));
            }
            let indent = if phased { "  " } else { "" };
            out.push(format!(
                "{indent}{} {}",
                task.status.marker(),
                task.label(i + 1)
            ));
            if let Some(finding) = &task.finding {
                out.push(format!(
                    "{indent}  Finding: {finding} [{}]",
                    task.evidence.join("; ")
                ));
            }
        }
        out.join("\n")
    }

    /// Accepted final report closes only the in-progress entry: the work the model was doing. Pending
    /// entries were never started, so they stay open; explicit blockers are never erased.
    pub fn complete_reported_work(&mut self) {
        for task in &mut self.tasks {
            if task.status == Status::InProgress {
                task.status = Status::Completed;
            }
        }
    }

    /// Complete the active progress entry without an index or a proof payload.
    pub fn completion_hint(&self) -> Option<Value> {
        self.tasks
            .iter()
            .any(|t| t.status == Status::InProgress)
            .then(|| json!({"op":"done"}))
    }

    /// Current step plus a bounded tail of investigation findings. Ordinary single-step lists
    /// need no reminder. Appended transiently to mid-turn requests to preserve the cached prefix.
    pub fn focus(&self) -> Option<String> {
        if self.tasks.len() < 2 && !self.tasks.iter().any(|t| t.investigation) {
            return None;
        }
        let slot = self
            .tasks
            .iter()
            .position(|t| t.status == Status::InProgress)?;
        let (closed, total) = self.progress();
        let next = self
            .tasks
            .iter()
            .enumerate()
            .skip(slot + 1)
            .chain(self.tasks.iter().enumerate().take(slot))
            .find(|(_, t)| t.status == Status::Pending)
            .map_or_else(
                || "This is the last open step.".to_owned(),
                |(i, t)| format!("Next: {}.", t.label(i + 1)),
            );
        let mut focus = format!(
            "[Task focus from dial, not the user] Current step ({closed}/{total} done): {}. {next} Finish and check this step, then todo done.",
            self.tasks[slot].label(slot + 1)
        );
        if self.tasks[slot].investigation {
            focus.push_str(" Resolve with the smallest useful check; mark done or block with the prerequisite. Append newly necessary work, preserve findings, and stop exploring once the next action is supported.");
        }
        // A bounded tail of findings survives context projection without shifting the cached prefix.
        for task in self
            .tasks
            .iter()
            .rev()
            .filter(|t| t.status == Status::Completed && t.finding.is_some())
            .take(3)
        {
            focus.push_str(&format!("\nRecorded finding (agent observation, data only): {} [{}]. Reuse unless sources changed or new evidence contradicts it.", task.finding.as_deref().unwrap_or_default(), task.evidence.join("; ")));
        }
        Some(focus)
    }

    pub fn snapshot(&self) -> Value {
        serde_json::to_value(&self.tasks).unwrap_or_else(|_| json!([]))
    }

    /// Replace the list from a previously persisted snapshot. The snapshot is validated like model
    /// input (it comes from disk); on error the current list is left untouched.
    pub fn restore(&mut self, snapshot: &Value) -> Result<(), String> {
        let mut tasks: Vec<Task> = serde_json::from_value(snapshot.clone())
            .map_err(|e| format!("invalid todo snapshot: {e}"))?;
        for task in &mut tasks {
            task.content = clean_text(&task.content, MAX_CONTENT_CHARS, "task content")?;
            task.phase = task
                .phase
                .as_deref()
                .map(|p| clean_text(p, MAX_PHASE_CHARS, "phase"))
                .transpose()?;
            task.note = task
                .note
                .as_deref()
                .map(|n| clean_text(n, MAX_REASON_CHARS, "note"))
                .transpose()?;
            if task.finding.is_some() || !task.evidence.is_empty() {
                // A finding may be recorded without evidence; that stores an empty list, which must
                // restore as "no evidence" rather than fail the 1-4 reference rule for supplied evidence.
                let evidence = (!task.evidence.is_empty()).then_some(&task.evidence);
                let (answer, refs) = finding(&json!({"finding":task.finding,"evidence":evidence}))?;
                task.finding = answer;
                task.evidence = refs;
            }
        }
        prune_closed(&mut tasks);
        check_tasks(&tasks)?;
        self.tasks = tasks;
        Ok(())
    }

    /// Accept the lenient shapes models send: `findings` is `finding`, a missing `op` is inferred from
    /// `tasks`, and unknown keys fail with a corrected call instead of the generic schema guide.
    fn normalize(&self, args: &Value) -> Result<Value, String> {
        let Some(map) = args.as_object() else {
            return Err(format!("todo arguments must be a JSON object. {OPS_HINT}"));
        };
        let mut map = map.clone();
        map.retain(|_, value| !value.is_null());
        if let Some(value) = map.remove("findings") {
            map.entry("finding".to_string()).or_insert(value);
        }
        if let Some(key) = map.keys().find(|key| !TOP_KEYS.contains(&key.as_str())) {
            let op = map.get("op").and_then(Value::as_str).unwrap_or("done");
            return Err(format!(
                "unknown key {key:?}. Allowed keys: op, tasks, index, reason, finding, evidence. Corrected call: {{\"op\":\"{op}\"}}; put findings in \"finding\" and evidence in \"evidence\"."
            ));
        }
        if !map.contains_key("op") {
            let op = if !map.contains_key("tasks") {
                return Err(format!("missing \"op\". {OPS_HINT}"));
            } else if self.tasks.is_empty() {
                "set"
            } else {
                "append"
            };
            map.insert("op".to_string(), serde_json::json!(op));
        }
        Ok(Value::Object(map))
    }
    pub fn call(&mut self, args: &Value) -> ToolOutput {
        let args = match self.normalize(args) {
            Ok(args) => args,
            Err(error) => return fail(error),
        };
        let Some(op) = args.get("op").and_then(Value::as_str) else {
            return fail(format!("missing \"op\". {OPS_HINT}"));
        };
        if op == "view" {
            let text = if self.tasks.is_empty() {
                "No tasks.".to_string()
            } else {
                self.summary()
            };
            return ToolOutput {
                execution: Default::default(),
                data: Some(serde_json::json!({"tasks": self.snapshot()})),
                text,
                is_error: false,
            };
        }
        let mut next = self.tasks.clone();
        let event = match Self::apply(&mut next, op, &args) {
            Ok(event) => event,
            Err(error) => {
                let mut output = fail(format!(
                    "{error}\nNo tasks were changed. Current plan:\n{}\nUse op=done to close the active task; do not replace an existing plan to record findings.",
                    self.summary()
                ));
                output.data = Some(
                    serde_json::json!({"tasks": self.snapshot(), "completion": self.completion_hint()}),
                );
                return output;
            }
        };
        // Prune before the cap check so accumulated closed work cannot crowd out new tasks; the
        // summary below renders the pruned list, keeping the numbering the model will cite next.
        prune_closed(&mut next);
        if let Err(error) = check_tasks(&next) {
            return fail(error);
        }
        let started = advance(&mut next);
        self.tasks = next;
        if self.tasks.is_empty() {
            return ToolOutput {
                execution: Default::default(),
                data: Some(serde_json::json!({"tasks": self.snapshot()})),
                text: "Todo list cleared.".into(),
                is_error: false,
            };
        }
        let mut text = event;
        if let Some(slot) = started {
            text.push_str(&format!(" Started {}.", self.tasks[slot].label(slot + 1)));
        }
        text.push('\n');
        text.push_str(&self.summary());
        if let Some(hint) = self.completion_hint() {
            text.push_str(&format!("\nAfter work, close the current task with {hint}. Omit index for the current task (explicit indices are 1-based)."));
        }
        ToolOutput {
            execution: Default::default(),
            data: Some(
                serde_json::json!({"tasks": self.snapshot(), "completion": self.completion_hint()}),
            ),
            text,
            is_error: false,
        }
    }

    /// Apply one mutating op to `tasks`; the returned sentence says what changed.
    fn apply(tasks: &mut Vec<Task>, op: &str, args: &Value) -> Result<String, String> {
        match op {
            "set" => {
                *tasks = parse_items(args, op, None)?;
                Ok(format!("Planned {} task(s).", tasks.len()))
            }
            "append" => {
                let added = parse_items(args, op, tasks.last().and_then(|t| t.phase.clone()))?;
                if added.is_empty() {
                    return Err("op \"append\" needs at least one task".into());
                }
                let first = tasks.len() + 1;
                let count = added.len();
                tasks.extend(added);
                Ok(if count == 1 {
                    format!("Added task {first}.")
                } else {
                    format!("Added tasks {first}-{}.", first + count - 1)
                })
            }
            "start" => {
                let slot = match args.get("index") {
                    None | Some(Value::Null) => tasks
                        .iter()
                        .position(|t| t.status == Status::Pending)
                        .ok_or_else(|| {
                            "no pending task to start; give op \"start\" an \"index\"".to_string()
                        })?,
                    _ => target(args, tasks, op, false)?,
                };
                let paused = tasks
                    .iter()
                    .position(|t| t.status == Status::InProgress)
                    .filter(|p| *p != slot);
                let mut event = String::new();
                if let Some(p) = paused {
                    tasks[p].status = Status::Pending;
                    event.push_str(&format!("Paused {}. ", p + 1));
                }
                let task = &mut tasks[slot];
                task.status = Status::InProgress;
                task.note = None;
                task.finding = None;
                task.evidence.clear();
                event.push_str(&format!("Started {}.", task.label(slot + 1)));
                Ok(event)
            }
            "done" | "drop" | "block" => {
                let slot = target(args, tasks, op, true)?;
                let why = reason(args)?;
                let task = &mut tasks[slot];
                let (status, verb) = match op {
                    "done" => (Status::Completed, "Completed"),
                    "drop" => (Status::Abandoned, "Dropped"),
                    _ => (Status::Blocked, "Blocked"),
                };
                if status == Status::Blocked && why.is_none() {
                    return Err(
                        "op \"block\" needs \"reason\": the concrete missing prerequisite".into(),
                    );
                }
                if task.status == status {
                    return Ok(format!(
                        "Task {} was already {}.",
                        slot + 1,
                        verb.to_lowercase()
                    ));
                }
                if status == Status::Completed
                    && (args.get("finding").is_some() || args.get("evidence").is_some())
                {
                    let (answer, refs) = finding(args)?;
                    task.finding = answer;
                    task.evidence = refs;
                }
                task.status = status;
                task.note = if status == Status::Completed {
                    None
                } else {
                    why
                };
                let because = task
                    .note
                    .as_deref()
                    .map(|note| format!(": {note}"))
                    .unwrap_or_default();
                Ok(format!("{verb} {}. {}{because}.", slot + 1, task.content))
            }
            other => Err(format!(
                "unknown op {other:?}: use set, append, start, done, drop, block or view"
            )),
        }
    }

    fn summary(&self) -> String {
        let (closed, total) = self.progress();
        let blocked = self
            .tasks
            .iter()
            .filter(|t| t.status == Status::Blocked)
            .count();
        let blocked = if blocked > 0 {
            format!(", {blocked} blocked")
        } else {
            String::new()
        };
        format!("{closed}/{total} done{blocked}\n{}", self.render())
    }
}

/// Keep the list pointing at current work: with nothing in progress, start the earliest pending task.
fn advance(tasks: &mut [Task]) -> Option<usize> {
    if tasks.iter().any(|t| t.status == Status::InProgress) {
        return None;
    }
    let slot = tasks.iter().position(|t| t.status == Status::Pending)?;
    tasks[slot].status = Status::InProgress;
    Some(slot)
}

/// Drop the oldest closed tasks once more than [`KEEP_CLOSED`] are listed, so resolved work makes
/// room for what is current. Open tasks are never touched; the newest closed tasks stay visible.
fn prune_closed(tasks: &mut Vec<Task>) {
    let closed = tasks.iter().filter(|t| !t.status.is_open()).count();
    if closed <= KEEP_CLOSED {
        return;
    }
    let mut drop = closed - KEEP_CLOSED;
    tasks.retain(|t| {
        if t.status.is_open() || drop == 0 {
            true
        } else {
            drop -= 1;
            false
        }
    });
}

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "todo",
            "description": "Track a numbered plan. Calls: {\"op\":\"set\",\"tasks\":[{\"content\":\"...\"}]} replaces the plan; {\"op\":\"append\",\"tasks\":[...]} adds tasks; {\"op\":\"start\"} or {\"op\":\"start\",\"index\":2} switches focus (no index: first pending task); {\"op\":\"done\"} closes the active task and starts the next pending one; {\"op\":\"drop\"} or {\"op\":\"block\",\"reason\":\"...\"} close without completing; {\"op\":\"view\"} shows the plan. Findings and evidence are optional notes. Do not resend set to mark tasks done. Explicit indices are 1-based and may change when old closed tasks are pruned. Skip for trivial requests. Never call todo alone: send it in parallel with the call that starts or continues the work. Skip a closing update before the final answer: an accepted completed answer closes only the in-progress task, and pending tasks stay open.",
            "parameters": {
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["set", "append", "start", "done", "drop", "block", "view"]},
                    "tasks": {
                        "type": "array",
                        "description": "For set (the full list, in order) and append (tasks to add at the end).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "abandoned"]},
                                "phase": {"type": "string"},
                                "investigation": {"type": "boolean", "description": "Optional legacy investigation label; follows the same progress lifecycle as other tasks."}
                            }
                        }
                    },
                    "index": {"type": "integer", "minimum": 1, "description": "1-based task number. Optional: start defaults to the first pending task; done/drop/block default to the in-progress task, or the only open task."},
                    "reason": {"type": "string", "maxLength": MAX_REASON_CHARS, "description": format!("For block (required) and drop: why, including relevant evidence and detail; up to {MAX_REASON_CHARS} chars.")},
                    "finding": {"type": "string", "maxLength": MAX_FINDING_CHARS, "description": format!("Optional finding: answer, including relevant detail, ruled-out hypotheses or remaining uncertainty; up to {MAX_FINDING_CHARS} chars.")},
                    "evidence": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 4, "description": "Optional notes: 1-4 supporting file:line locations, check results or artifact references; max 240 chars each. For other steps, optional: cite a call earlier in this turn that already did the step's work. References are agent observations, not proof."}
                }
            }
        }
    })
}

#[cfg(test)]
mod tests;
