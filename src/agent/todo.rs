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

/// Parse `tasks` for `set`/`append`. A task without `phase` inherits the phase of the task before it
/// (`inherited` for the first), so a phase is named once at the head of its group.
fn parse_items(args: &Value, op: &str, mut inherited: Option<String>) -> Result<Vec<Task>, String> {
    let items = args.get("tasks").and_then(Value::as_array).ok_or_else(|| {
        format!("op \"{op}\" needs \"tasks\": [{{\"content\": ..., \"phase\": ...}}]")
    })?;
    let mut tasks = Vec::with_capacity(items.len().min(MAX_TASKS + 1));
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
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
            None => false,
            Some(value) => value.as_bool().ok_or("investigation must be a boolean")?,
        };
        if investigation && status == Status::Completed {
            return Err("Investigation tasks must start open; close them with done, finding and evidence after inspecting.".into());
        }
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
        None | Some(Value::Null) if active => tasks
            .iter()
            .position(|t| t.status == Status::InProgress)
            .ok_or_else(|| format!("no task is in_progress; give op \"{op}\" an \"index\"")),
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

fn finding(args: &Value) -> Result<(String, Vec<String>), String> {
    let raw = args["finding"]
        .as_str()
        .ok_or("Investigation done needs finding: the answer or ruled-out hypothesis")?;
    let finding = clean_text(raw, MAX_FINDING_CHARS, "finding")?;
    let refs = args["evidence"].as_array().filter(|refs| !refs.is_empty() && refs.len() <= 4)
        .ok_or("Investigation done needs evidence: 1-4 source locations, check results or artifact references")?;
    let evidence = refs
        .iter()
        .map(|r| {
            clean_text(
                r.as_str().ok_or("evidence references must be strings")?,
                240,
                "evidence reference",
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((finding, evidence))
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
            "[Task focus from dial, not the user] Current step ({closed}/{total} done): {}. {next} Work only on this step; call todo done once it is finished and checked.",
            self.tasks[slot].label(slot + 1)
        );
        if self.tasks[slot].investigation {
            focus.push_str(" Resolve this question with the smallest useful check. Close it with finding and evidence, or block with the missing prerequisite. If the result uncovers necessary work not already represented, call todo append before pursuing it; use start on that item if it must precede the current step. Preserve existing tasks and findings rather than replacing the plan. Stop exploring once the answer supports the next action.");
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
            if task.finding.is_some()
                || !task.evidence.is_empty()
                || (task.investigation && task.status == Status::Completed)
            {
                let (answer, refs) =
                    finding(&json!({"finding":task.finding,"evidence":task.evidence}))?;
                task.finding = Some(answer);
                task.evidence = refs;
            }
        }
        prune_closed(&mut tasks);
        check_tasks(&tasks)?;
        self.tasks = tasks;
        Ok(())
    }

    pub fn call(&mut self, args: &Value) -> ToolOutput {
        let Some(op) = args.get("op").and_then(Value::as_str) else {
            return fail("missing \"op\": use set, append, start, done, drop, block or view");
        };
        if op == "view" {
            let text = if self.tasks.is_empty() {
                "No tasks.".to_string()
            } else {
                self.summary()
            };
            return ToolOutput {
                data: Some(serde_json::json!({"tasks": self.snapshot()})),
                text,
                is_error: false,
            };
        }
        let mut next = self.tasks.clone();
        let event = match Self::apply(&mut next, op, args) {
            Ok(event) => event,
            Err(error) => return fail(error),
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
        ToolOutput {
            data: Some(serde_json::json!({"tasks": self.snapshot()})),
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
                let slot = target(args, tasks, op, false)?;
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
                if status == Status::Completed && task.investigation {
                    if task.status != Status::InProgress {
                        return Err("Start the investigation question before closing it, so its evidence belongs to the active step.".into());
                    }
                    let (answer, refs) = finding(args)?;
                    task.finding = Some(answer);
                    task.evidence = refs;
                } else {
                    task.finding = None;
                    task.evidence.clear();
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
            "description": "Track a multi-step task as a numbered list. op=set plans the whole list (replaces it); op=append adds tasks; op=done/drop/block close the in-progress task (or the task at index); op=start switches to the task at index (reopening or unblocking it); op=view shows the list. Whenever nothing is in progress the earliest pending task starts automatically, so finishing a step and moving on is one done call. block needs reason; blocked tasks stay open until started again. phase labels a group: set it on the first task of each group and following tasks inherit it. Resolved tasks do not pile up: once more than three are closed, the oldest are removed automatically. Max 50 tasks, 200 chars each. Skip it for trivial one-step requests.",
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
                                "investigation": {"type": "boolean", "description": "True for a concrete investigation question. Start open; done requires finding and evidence. Use 1-3 focused questions before implementation, skip for trivial work."}
                            },
                            "required": ["content"],
                            "additionalProperties": false
                        }
                    },
                    "index": {"type": "integer", "minimum": 1, "description": "1-based task number. Required for start; done/drop/block default to the in-progress task."},
                    "reason": {"type": "string", "maxLength": MAX_REASON_CHARS, "description": format!("For block (required) and drop: why, including relevant evidence and detail; up to {MAX_REASON_CHARS} chars.")},
                    "finding": {"type": "string", "maxLength": MAX_FINDING_CHARS, "description": format!("Required for investigation done: answer, including relevant detail, ruled-out hypotheses or remaining uncertainty; up to {MAX_FINDING_CHARS} chars.")},
                    "evidence": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 4, "description": "Required for investigation done: 1-4 supporting file:line locations, check results or artifact references; max 240 chars each. References are agent observations, not proof."}
                },
                "required": ["op"],
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detailed_findings_close_tasks_and_survive_resume_without_truncation() {
        for answer in ["x".repeat(633), "界".repeat(4_000)] {
            let mut list = TodoList::new();
            assert!(
                !set(
                    &mut list,
                    json!([
                        {"content":"Trace the failure", "investigation":true},
                        {"content":"Implement the correction"}
                    ])
                )
                .is_error
            );
            let output =
                list.call(&json!({"op":"done","finding":answer,"evidence":["src/parser.rs:24"]}));
            assert!(!output.is_error, "{}", output.text);
            assert_eq!(list.tasks()[0].finding.as_deref(), Some(answer.as_str()));
            assert_eq!(list.tasks()[1].status, Status::InProgress);
            let mut resumed = TodoList::new();
            resumed.restore(&list.snapshot()).unwrap();
            assert_eq!(resumed, list);
            assert!(resumed.focus().unwrap().contains(&answer));
            assert!(!resumed.call(&json!({"op":"start","index":1})).is_error);
            let before = resumed.snapshot();
            assert!(resumed.call(&json!({"op":"done","finding":"x".repeat(4_001),"evidence":["src/parser.rs:24"]})).is_error);
            assert_eq!(resumed.snapshot(), before);
        }
    }

    #[test]
    fn investigation_requires_findings_and_preserves_them_through_resume() {
        let mut list = TodoList::new();
        assert!(
            !set(
                &mut list,
                json!([
                    {"content":"Where is the limit enforced?", "investigation":true},
                    {"content":"Change the limit and test"}
                ])
            )
            .is_error
        );
        let before = list.snapshot();
        for args in [
            json!({"op":"done"}),
            json!({"op":"done","finding":"In the parser","evidence":[]}),
        ] {
            assert!(list.call(&args).is_error);
            assert_eq!(list.snapshot(), before);
        }
        assert!(!list.call(&json!({"op":"done", "finding":"The parser rejects values above 5", "evidence":["src/parser.rs:24"]})).is_error);
        assert!(list.render().contains("Finding: The parser"));
        let mut resumed = TodoList::new();
        resumed.restore(&list.snapshot()).unwrap();
        assert_eq!(resumed, list);
        assert!(resumed.focus().unwrap().contains("src/parser.rs:24"));
        resumed.call(&json!({"op":"start","index":1}));
        assert!(resumed.tasks()[0].finding.is_none());
        assert!(resumed.tasks()[0].evidence.is_empty());
    }

    #[test]
    fn investigation_cannot_be_precompleted_or_closed_out_of_order() {
        let mut list = TodoList::new();
        assert!(
            set(
                &mut list,
                json!([{"content":"Why?","investigation":true,"status":"completed"}])
            )
            .is_error
        );
        set(
            &mut list,
            json!([{"content":"First?","investigation":true},{"content":"Next?","investigation":true}]),
        );
        assert!(
            list.call(&json!({"op":"done","index":2,"finding":"Claim","evidence":["x:1"]}))
                .is_error
        );
        assert!(
            !list
                .call(&json!({"op":"block","reason":"Need reproduction input"}))
                .is_error
        );
        assert_eq!(list.tasks()[0].status, Status::Blocked);
        let mut legacy = TodoList::new();
        legacy
            .restore(&json!([{"content":"old task","status":"completed"}]))
            .unwrap();
        assert!(!legacy.tasks()[0].investigation);
    }

    fn set(list: &mut TodoList, tasks: Value) -> ToolOutput {
        list.call(&json!({"op": "set", "tasks": tasks}))
    }

    fn statuses(list: &TodoList) -> Vec<Status> {
        list.tasks().iter().map(|t| t.status).collect()
    }

    #[test]
    fn focus_names_the_current_and_next_step_only_for_multi_step_plans() {
        let mut list = TodoList::new();
        set(&mut list, json!([{"content": "only step"}]));
        assert_eq!(list.focus(), None);
        set(
            &mut list,
            json!([{"content": "read", "status": "completed"}, {"content": "fix"}, {"content": "test"}]),
        );
        let focus = list.focus().unwrap();
        assert!(
            focus.contains("(1/3 done): 2. fix.") && focus.contains("Next: 3. test."),
            "{focus}"
        );
        list.call(&json!({"op": "done"}));
        assert!(list.focus().unwrap().contains("last open step"));
        list.call(&json!({"op": "block", "reason": "flaky CI"}));
        assert_eq!(
            list.focus(),
            None,
            "nothing in progress means nothing to focus on"
        );
    }

    #[test]
    fn set_keeps_explicit_statuses_and_renders_checkboxes() {
        let mut list = TodoList::new();
        let out = set(
            &mut list,
            json!([
                {"content": "read code", "status": "completed"},
                {"content": "write fix", "status": "in_progress"},
                {"content": "run tests"},
                {"content": "old idea", "status": "abandoned"}
            ]),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            list.render(),
            "[x] 1. read code\n[~] 2. write fix\n[ ] 3. run tests\n[-] 4. old idea"
        );
        assert!(out.text.contains("2/4 done"), "{}", out.text);
    }

    #[test]
    fn closing_the_active_task_starts_the_next_pending_one() {
        let mut list = TodoList::new();
        let out = set(
            &mut list,
            json!([{"content": "a"}, {"content": "b"}, {"content": "c"}]),
        );
        assert!(
            out.text.contains("Started 1. a"),
            "a plan with nothing active starts its first task: {}",
            out.text
        );
        assert_eq!(
            statuses(&list),
            [Status::InProgress, Status::Pending, Status::Pending]
        );

        let out = list.call(&json!({"op": "done"}));
        assert!(
            !out.is_error && out.text.starts_with("Completed 1. a. Started 2. b."),
            "{}",
            out.text
        );
        assert_eq!(
            statuses(&list),
            [Status::Completed, Status::InProgress, Status::Pending]
        );

        // Closing a task other than the active one leaves the active one alone.
        list.call(&json!({"op": "drop", "index": 3, "reason": "not needed"}));
        assert_eq!(
            statuses(&list),
            [Status::Completed, Status::InProgress, Status::Abandoned]
        );
        assert!(list.render().contains("3. c (dropped: not needed)"));

        list.call(&json!({"op": "done"}));
        assert!(!list.has_open());
        assert_eq!(list.progress(), (3, 3));
    }

    #[test]
    fn closed_tasks_are_pruned_to_a_recent_tail() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([
                {"content": "t1", "status": "completed"},
                {"content": "t2", "status": "completed"},
                {"content": "t3", "status": "completed"},
                {"content": "t4", "status": "completed"},
                {"content": "t5"}
            ]),
        );
        assert_eq!(
            list.render(),
            "[x] 1. t2\n[x] 2. t3\n[x] 3. t4\n[~] 4. t5",
            "the oldest closed task is pruned and numbering closes up"
        );
        assert_eq!(list.progress(), (3, 4));

        // Closing the last task keeps it listed as the newest closed one.
        list.call(&json!({"op": "done"}));
        assert_eq!(list.render(), "[x] 1. t3\n[x] 2. t4\n[x] 3. t5");
        assert!(!list.has_open());
    }

    #[test]
    fn pruning_keeps_the_findings_tail_and_resumes_clean() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([
                {"content": "q1", "investigation": true},
                {"content": "q2", "investigation": true},
                {"content": "q3", "investigation": true},
                {"content": "q4", "investigation": true},
                {"content": "step"}
            ]),
        );
        for (q, finding) in [("q1", "a1"), ("q2", "a2"), ("q3", "a3"), ("q4", "a4")] {
            let out = list.call(&json!({
                "op": "done",
                "finding": finding,
                "evidence": [format!("{q}:1")]
            }));
            assert!(!out.is_error, "{}", out.text);
        }
        assert_eq!(
            list.tasks()
                .iter()
                .map(|t| t.content.as_str())
                .collect::<Vec<_>>(),
            ["q2", "q3", "q4", "step"],
            "the oldest closed question is pruned once four are closed"
        );
        let focus = list.focus().unwrap();
        assert!(
            focus.contains("a2") && focus.contains("a3") && focus.contains("a4"),
            "{focus}"
        );
        assert!(!focus.contains("a1"), "{focus}");

        // A resumed snapshot loads pruned the same way.
        let mut resumed = TodoList::new();
        resumed
            .restore(&json!([
                {"content": "c1", "status": "completed"},
                {"content": "c2", "status": "completed"},
                {"content": "c3", "status": "completed"},
                {"content": "c4", "status": "abandoned", "note": "superseded"},
                {"content": "open", "status": "pending"}
            ]))
            .unwrap();
        assert_eq!(
            resumed
                .tasks()
                .iter()
                .map(|t| t.content.as_str())
                .collect::<Vec<_>>(),
            ["c2", "c3", "c4", "open"]
        );
    }

    #[test]
    fn block_needs_a_reason_keeps_work_open_and_start_unblocks() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([{"content": "deploy"}, {"content": "write docs"}]),
        );
        let out = list.call(&json!({"op": "block"}));
        assert!(out.is_error && out.text.contains("reason"), "{}", out.text);
        assert_eq!(
            statuses(&list),
            [Status::InProgress, Status::Pending],
            "a rejected op must not change the list"
        );

        let out = list.call(&json!({"op": "block", "reason": "no deploy credentials"}));
        assert!(
            out.text.contains("Started 2. write docs") && out.text.contains("1 blocked"),
            "{}",
            out.text
        );
        assert_eq!(statuses(&list), [Status::Blocked, Status::InProgress]);
        list.call(&json!({"op": "done"}));
        assert!(list.has_open(), "a blocked task is still owed");
        assert_eq!(
            list.tasks()[0].label(1),
            "1. deploy (blocked: no deploy credentials)"
        );

        list.call(&json!({"op": "start", "index": 1}));
        assert_eq!(list.tasks()[0].status, Status::InProgress);
        assert_eq!(list.tasks()[0].note, None);
    }

    #[test]
    fn detailed_drop_and_block_reasons_survive_resume_and_keep_state_on_overflow() {
        for op in ["drop", "block"] {
            for why in ["x".repeat(350), "𐍈".repeat(MAX_REASON_CHARS)] {
                let mut list = TodoList::new();
                set(
                    &mut list,
                    json!([{"content":"inspect baseline"},{"content":"next step"}]),
                );
                let output = list.call(&json!({"op":op,"reason":why}));
                assert!(!output.is_error, "{}", output.text);
                assert_eq!(list.tasks()[0].note.as_deref(), Some(why.as_str()));
                assert_eq!(
                    list.tasks()[0].status,
                    if op == "drop" {
                        Status::Abandoned
                    } else {
                        Status::Blocked
                    }
                );
                assert_eq!(list.tasks()[1].status, Status::InProgress);
                assert!(list.render().contains(&why));
                let mut resumed = TodoList::new();
                resumed.restore(&list.snapshot()).unwrap();
                assert_eq!(resumed, list);

                let before = resumed.snapshot();
                let oversized = "𐍈".repeat(MAX_REASON_CHARS + 1);
                let failed = resumed.call(&json!({"op":op,"index":2,"reason":oversized}));
                assert!(
                    failed.is_error && failed.text.contains("limit is 4000"),
                    "{}",
                    failed.text
                );
                assert_eq!(resumed.snapshot(), before);
                let mut bad_snapshot = before.clone();
                bad_snapshot[0]["note"] = json!(oversized);
                assert!(resumed.restore(&bad_snapshot).is_err());
                assert_eq!(resumed.snapshot(), before);
            }
        }
        assert_eq!(
            schema()["function"]["parameters"]["properties"]["reason"]["maxLength"],
            MAX_REASON_CHARS
        );
    }

    #[test]
    fn start_switches_focus_and_reopens() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([{"content": "a"}, {"content": "b", "status": "completed"}]),
        );
        let out = list.call(&json!({"op": "start", "index": 2}));
        assert!(
            out.text.starts_with("Paused 1. Started 2. b."),
            "{}",
            out.text
        );
        assert_eq!(statuses(&list), [Status::Pending, Status::InProgress]);
        assert!(
            list.call(&json!({"op": "start"})).is_error,
            "start names its task"
        );
    }

    #[test]
    fn phases_group_the_rendering_and_are_inherited() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([
                {"content": "read", "phase": "Investigate"},
                {"content": "trace"},
                {"content": "fix", "phase": "Implement"}
            ]),
        );
        list.call(&json!({"op": "append", "tasks": [{"content": "test"}]}));
        assert_eq!(
            list.render(),
            "Investigate:\n  [~] 1. read\n  [ ] 2. trace\nImplement:\n  [ ] 3. fix\n  [ ] 4. test"
        );
    }

    #[test]
    fn caps_and_bad_input_are_errors_that_leave_state_alone() {
        let mut list = TodoList::new();
        let many: Vec<Value> = (0..=MAX_TASKS)
            .map(|i| json!({"content": format!("t{i}")}))
            .collect();
        assert!(set(&mut list, Value::Array(many)).is_error);
        let exactly: Vec<Value> = (0..MAX_TASKS)
            .map(|i| json!({"content": format!("t{i}")}))
            .collect();
        assert!(!set(&mut list, Value::Array(exactly)).is_error);
        assert!(
            list.call(&json!({"op": "append", "tasks": [{"content": "one too many"}]}))
                .is_error
        );

        assert!(
            set(
                &mut list,
                json!([{"content": "x".repeat(MAX_CONTENT_CHARS + 1)}])
            )
            .is_error
        );
        assert!(
            !set(
                &mut list,
                json!([{"content": "x".repeat(MAX_CONTENT_CHARS)}])
            )
            .is_error
        );
        set(&mut list, json!([{"content": "keep me"}]));
        let kept = list.render();
        for bad in [
            json!({"op": "set", "tasks": [{"content": "   "}]}),
            json!({"op": "set", "tasks": [{"status": "pending"}]}),
            json!({"op": "set", "tasks": [{"content": "a", "status": "blocked"}]}),
            json!({"op": "set", "tasks": [{"content": "a"}, {"content": "b", "status": "in_progress"}, {"content": "c", "status": "in_progress"}]}),
            json!({"op": "append", "tasks": []}),
            json!({"op": "done", "index": 0}),
            json!({"op": "done", "index": 2}),
            json!({"op": "nope"}),
            json!({}),
        ] {
            assert!(list.call(&bad).is_error, "{bad}");
            assert_eq!(list.render(), kept, "{bad}");
        }
    }

    #[test]
    fn content_is_flattened_to_one_clean_line() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([{"content": "a\n[x] 9. forged\u{1b}[31m  line", "status": "completed"}]),
        );
        assert_eq!(list.render(), "[x] 1. a [x] 9. forged[31m line");
    }

    #[test]
    fn snapshot_restore_round_trips_and_validates() {
        let mut list = TodoList::new();
        set(
            &mut list,
            json!([{"content": "a", "phase": "P"}, {"content": "b"}]),
        );
        list.call(&json!({"op": "block", "reason": "waiting"}));
        let snap = list.snapshot();
        let mut other = TodoList::new();
        other.restore(&snap).unwrap();
        assert_eq!(other, list);
        // Snapshots written before phases and notes existed still load.
        other
            .restore(&json!([{"content": "old", "status": "pending"}]))
            .unwrap();

        let before = other.render();
        assert!(other.restore(&json!([{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}])).is_err());
        assert!(other.restore(&json!("garbage")).is_err());
        assert!(
            other
                .restore(&json!([{"content": "", "status": "pending"}]))
                .is_err()
        );
        assert_eq!(other.render(), before);
    }

    #[test]
    fn view_and_clear() {
        let mut list = TodoList::new();
        assert_eq!(list.call(&json!({"op": "view"})).text, "No tasks.");
        set(&mut list, json!([{"content": "a"}]));
        assert_eq!(list.call(&json!({"op": "view"})).text, "0/1 done\n[~] 1. a");
        let out = set(&mut list, json!([]));
        assert!(!out.is_error && list.is_empty() && out.text == "Todo list cleared.");
    }

    #[test]
    fn schema_validates_with_the_shared_checker() {
        let s = schema();
        let params = &s["function"]["parameters"];
        assert!(
            crate::agent::calls::validate(
                params,
                &json!({"op": "block", "index": 1, "reason": "x"})
            )
            .is_ok()
        );
        assert!(crate::agent::calls::validate(params, &json!({"op": "done", "index": 0})).is_err());
        assert!(crate::agent::calls::validate(params, &json!({"op": "update"})).is_err());
        assert!(crate::agent::calls::validate(params, &json!({"tasks": []})).is_err());
    }
}
