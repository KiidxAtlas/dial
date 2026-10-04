//! The `todo` tool: a small task list the agent keeps for itself while working on a multi-step job.
//!
//! State machine: tasks are `pending -> in_progress -> completed | abandoned`. At most one task is
//! `in_progress` at a time, so the list always says what the agent is doing right now. The list is
//! session state; `snapshot`/`restore` let the session store persist it.
use crate::tools::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_TASKS: usize = 50;
pub const MAX_CONTENT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Abandoned,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub content: String,
    pub status: Status,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoList {
    tasks: Vec<Task>,
}

fn fail(text: impl Into<String>) -> ToolOutput {
    ToolOutput { text: text.into(), is_error: true }
}

/// One line, control characters dropped, whitespace collapsed. Task text is model-written and is
/// rendered back into prompts and terminals, so it must not carry newlines or escape sequences.
fn clean_content(raw: &str) -> Result<String, String> {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    if text.is_empty() {
        return Err("task content must not be empty".into());
    }
    if text.chars().count() > MAX_CONTENT_CHARS {
        return Err(format!("task content is {} chars; the limit is {MAX_CONTENT_CHARS}. Shorten it", text.chars().count()));
    }
    Ok(text)
}

fn check_tasks(tasks: &[Task]) -> Result<(), String> {
    if tasks.len() > MAX_TASKS {
        return Err(format!("{} tasks; the limit is {MAX_TASKS}. Merge or drop finished work", tasks.len()));
    }
    let active = tasks.iter().filter(|t| t.status == Status::InProgress).count();
    if active > 1 {
        return Err(format!("{active} tasks are in_progress; at most one task may be in_progress at a time"));
    }
    Ok(())
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

    /// Checkbox list, one numbered task per line: `[ ]` pending, `[~]` in progress, `[x]` done,
    /// `[-]` abandoned. Empty string for an empty list.
    pub fn render(&self) -> String {
        self.tasks
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{} {}. {}", t.status.marker(), i + 1, t.content))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn snapshot(&self) -> Value {
        serde_json::to_value(&self.tasks).unwrap_or_else(|_| json!([]))
    }

    /// Replace the list from a previously persisted snapshot. The snapshot is validated like model
    /// input (it comes from disk); on error the current list is left untouched.
    pub fn restore(&mut self, snapshot: &Value) -> Result<(), String> {
        let mut tasks: Vec<Task> = serde_json::from_value(snapshot.clone()).map_err(|e| format!("invalid todo snapshot: {e}"))?;
        for task in &mut tasks {
            task.content = clean_content(&task.content)?;
        }
        check_tasks(&tasks)?;
        self.tasks = tasks;
        Ok(())
    }

    pub fn call(&mut self, args: &Value) -> ToolOutput {
        let Some(op) = args.get("op").and_then(Value::as_str) else {
            return fail("missing \"op\": use \"set\", \"update\" or \"view\"");
        };
        match op {
            "view" => self.view(),
            "set" => self.set(args),
            "update" => self.update(args),
            other => fail(format!("unknown op {other:?}: use \"set\", \"update\" or \"view\"")),
        }
    }

    fn view(&self) -> ToolOutput {
        let text = if self.tasks.is_empty() { "No tasks.".to_string() } else { self.render() };
        ToolOutput { text, is_error: false }
    }

    fn set(&mut self, args: &Value) -> ToolOutput {
        let Some(items) = args.get("tasks").and_then(Value::as_array) else {
            return fail("op \"set\" needs \"tasks\": [{\"content\": ..., \"status\": ...}]");
        };
        let mut tasks = Vec::with_capacity(items.len().min(MAX_TASKS + 1));
        for (i, item) in items.iter().enumerate() {
            let n = i + 1;
            let Some(raw) = item.get("content").and_then(Value::as_str) else {
                return fail(format!("task {n}: missing string \"content\""));
            };
            let content = match clean_content(raw) {
                Ok(c) => c,
                Err(e) => return fail(format!("task {n}: {e}")),
            };
            let status = match item.get("status") {
                None | Some(Value::Null) => Status::Pending,
                Some(v) => match v.as_str().and_then(Status::parse) {
                    Some(s) => s,
                    None => return fail(format!("task {n}: status must be pending, in_progress, completed or abandoned")),
                },
            };
            tasks.push(Task { content, status });
        }
        if let Err(e) = check_tasks(&tasks) {
            return fail(e);
        }
        self.tasks = tasks;
        ToolOutput { text: self.summary(), is_error: false }
    }

    fn update(&mut self, args: &Value) -> ToolOutput {
        let Some(index) = args.get("index").and_then(Value::as_u64) else {
            return fail("op \"update\" needs a positive integer \"index\" (the number shown in the list)");
        };
        let Some(status) = args.get("status").and_then(Value::as_str).and_then(Status::parse) else {
            return fail("op \"update\" needs \"status\": pending, in_progress, completed or abandoned");
        };
        let slot = usize::try_from(index).ok().and_then(|i| i.checked_sub(1)).filter(|i| *i < self.tasks.len());
        let Some(slot) = slot else {
            return fail(format!("no task {index}: the list has {} task(s)", self.tasks.len()));
        };
        if status == Status::InProgress {
            if let Some(other) = self.tasks.iter().position(|t| t.status == Status::InProgress).filter(|p| *p != slot) {
                return fail(format!("task {} is already in_progress; complete or abandon it first", other + 1));
            }
        }
        self.tasks[slot].status = status;
        ToolOutput { text: self.summary(), is_error: false }
    }

    fn summary(&self) -> String {
        let done = self.tasks.iter().filter(|t| matches!(t.status, Status::Completed | Status::Abandoned)).count();
        if self.tasks.is_empty() {
            return "Todo list cleared.".to_string();
        }
        format!("{done}/{} done\n{}", self.tasks.len(), self.render())
    }
}

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "todo",
            "description": "Track a multi-step task. op=set replaces the whole list; op=update changes one task's status by its number; op=view shows the list. Statuses: pending, in_progress, completed, abandoned. At most one task may be in_progress. Max 50 tasks, 200 chars each. Skip it for trivial one-step requests.",
            "parameters": {
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["set", "update", "view"]},
                    "tasks": {
                        "type": "array",
                        "description": "For op=set: the full list, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "abandoned"]}
                            },
                            "required": ["content"],
                            "additionalProperties": false
                        }
                    },
                    "index": {"type": "integer", "minimum": 1, "description": "For op=update: 1-based task number."},
                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "abandoned"], "description": "For op=update: the new status."}
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

    fn set(list: &mut TodoList, tasks: Value) -> ToolOutput {
        list.call(&json!({"op": "set", "tasks": tasks}))
    }

    #[test]
    fn set_defaults_to_pending_and_renders_checkboxes() {
        let mut list = TodoList::new();
        let out = set(&mut list, json!([
            {"content": "read code", "status": "completed"},
            {"content": "write fix", "status": "in_progress"},
            {"content": "run tests"},
            {"content": "old idea", "status": "abandoned"}
        ]));
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(list.render(), "[x] 1. read code\n[~] 2. write fix\n[ ] 3. run tests\n[-] 4. old idea");
        assert!(out.text.starts_with("2/4 done"));
    }

    #[test]
    fn at_most_one_in_progress() {
        let mut list = TodoList::new();
        let out = set(&mut list, json!([{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}]));
        assert!(out.is_error && out.text.contains("at most one"));
        assert!(list.is_empty(), "a rejected set must not change the list");

        set(&mut list, json!([{"content": "a", "status": "in_progress"}, {"content": "b"}]));
        let out = list.call(&json!({"op": "update", "index": 2, "status": "in_progress"}));
        assert!(out.is_error && out.text.contains("task 1"), "{}", out.text);
        assert_eq!(list.tasks()[1].status, Status::Pending);

        // Completing the active task frees the slot.
        assert!(!list.call(&json!({"op": "update", "index": 1, "status": "completed"})).is_error);
        assert!(!list.call(&json!({"op": "update", "index": 2, "status": "in_progress"})).is_error);
        // Re-asserting the active task is not a conflict with itself.
        assert!(!list.call(&json!({"op": "update", "index": 2, "status": "in_progress"})).is_error);
    }

    #[test]
    fn caps_and_bad_input_are_errors_that_leave_state_alone() {
        let mut list = TodoList::new();
        set(&mut list, json!([{"content": "keep me"}]));
        let many: Vec<Value> = (0..=MAX_TASKS).map(|i| json!({"content": format!("t{i}")})).collect();
        assert!(set(&mut list, Value::Array(many)).is_error);
        let exactly: Vec<Value> = (0..MAX_TASKS).map(|i| json!({"content": format!("t{i}")})).collect();
        assert!(!set(&mut list, Value::Array(exactly)).is_error);

        set(&mut list, json!([{"content": "keep me"}]));
        assert!(set(&mut list, json!([{"content": "x".repeat(MAX_CONTENT_CHARS + 1)}])).is_error);
        assert!(!set(&mut list, json!([{"content": "x".repeat(MAX_CONTENT_CHARS)}])).is_error);
        set(&mut list, json!([{"content": "keep me"}]));
        for bad in [
            json!([{"content": "   "}]),
            json!([{"status": "pending"}]),
            json!([{"content": "a", "status": "done"}]),
            json!([{"content": 5}]),
        ] {
            assert!(set(&mut list, bad).is_error);
            assert_eq!(list.render(), "[ ] 1. keep me");
        }
        assert!(list.call(&json!({"op": "update", "index": 0, "status": "completed"})).is_error);
        assert!(list.call(&json!({"op": "update", "index": 2, "status": "completed"})).is_error);
        assert!(list.call(&json!({"op": "update", "index": 1, "status": "finished"})).is_error);
        assert!(list.call(&json!({"op": "update", "index": 1})).is_error);
        assert!(list.call(&json!({"op": "nope"})).is_error);
        assert!(list.call(&json!({})).is_error);
    }

    #[test]
    fn content_is_flattened_to_one_clean_line() {
        let mut list = TodoList::new();
        set(&mut list, json!([{"content": "a\n[x] 9. forged\u{1b}[31m  line"}]));
        assert_eq!(list.render(), "[ ] 1. a [x] 9. forged[31m line");
    }

    #[test]
    fn snapshot_restore_round_trips_and_validates() {
        let mut list = TodoList::new();
        set(&mut list, json!([{"content": "a", "status": "completed"}, {"content": "b", "status": "in_progress"}]));
        let snap = list.snapshot();
        let mut other = TodoList::new();
        other.restore(&snap).unwrap();
        assert_eq!(other, list);

        let before = other.render();
        assert!(other.restore(&json!([{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}])).is_err());
        assert!(other.restore(&json!("garbage")).is_err());
        assert!(other.restore(&json!([{"content": "", "status": "pending"}])).is_err());
        assert_eq!(other.render(), before);
    }

    #[test]
    fn view_and_clear() {
        let mut list = TodoList::new();
        assert_eq!(list.call(&json!({"op": "view"})).text, "No tasks.");
        set(&mut list, json!([{"content": "a"}]));
        assert_eq!(list.call(&json!({"op": "view"})).text, "[ ] 1. a");
        let out = set(&mut list, json!([]));
        assert!(!out.is_error && list.is_empty());
    }

    #[test]
    fn schema_validates_with_the_shared_checker() {
        let s = schema();
        let params = &s["function"]["parameters"];
        assert!(crate::agent::calls::validate(params, &json!({"op": "update", "index": 1, "status": "completed"})).is_ok());
        assert!(crate::agent::calls::validate(params, &json!({"op": "update", "index": 0})).is_err());
        assert!(crate::agent::calls::validate(params, &json!({"tasks": []})).is_err());
    }
}
