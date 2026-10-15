//! Per-source and per-function breakpoint bookkeeping (setBreakpoint, removeBreakpoint,
//! setFunctionBreakpoint). DAP `setBreakpoints` replaces the whole list for a
//! source, so incremental add/remove must merge with what the adapter already holds.
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn line(entry: &Value) -> i64 {
    entry.get("line").and_then(Value::as_i64).unwrap_or(0)
}

fn name(entry: &Value) -> &str {
    entry.get("name").and_then(Value::as_str).unwrap_or("")
}

/// The session-book key for a DAP source: its path, falling back to its name.
pub fn source_key(source: &Value) -> Option<String> {
    ["path", "name"]
        .into_iter()
        .find_map(|key| source.get(key).and_then(Value::as_str))
        .map(str::to_owned)
}

/// Replace any breakpoint on the same line with `entry`, keeping the list sorted by line.
pub fn add_source(current: &[Value], entry: Value) -> Vec<Value> {
    let target = line(&entry);
    let mut next: Vec<Value> = current
        .iter()
        .filter(|bp| line(bp) != target)
        .cloned()
        .collect();
    next.push(entry);
    next.sort_by_key(line);
    next
}

/// Drop the breakpoint on `target`; other lines are untouched.
pub fn remove_source(current: &[Value], target: i64) -> Vec<Value> {
    current
        .iter()
        .filter(|bp| line(bp) != target)
        .cloned()
        .collect()
}

/// Replace any function breakpoint with the same name, keeping the list sorted by name.
pub fn add_function(current: &[Value], entry: Value) -> Vec<Value> {
    let target = name(&entry).to_owned();
    let mut next: Vec<Value> = current
        .iter()
        .filter(|bp| name(bp) != target)
        .cloned()
        .collect();
    next.push(entry);
    next.sort_by(|a, b| name(a).cmp(name(b)));
    next
}

pub fn remove_function(current: &[Value], target: &str) -> Vec<Value> {
    current
        .iter()
        .filter(|bp| name(bp) != target)
        .cloned()
        .collect()
}

/// Requested entries annotated with the adapter's verification result, index-aligned as the
/// DAP spec requires.
pub fn annotate(requested: &[Value], response: &Value) -> Vec<Value> {
    let reported = response.get("breakpoints").and_then(Value::as_array);
    requested
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut merged = entry.clone();
            if let (Some(object), Some(result)) = (
                merged.as_object_mut(),
                reported.and_then(|items| items.get(index)),
            ) {
                for key in ["verified", "id", "message", "line"] {
                    if let Some(value) = result.get(key) {
                        let key = if key == "line" { "actualLine" } else { key };
                        object.insert(key.into(), value.clone());
                    }
                }
            }
            merged
        })
        .collect()
}

/// Strip bookkeeping fields before sending entries back to the adapter.
pub fn request_entries(entries: &[Value]) -> Vec<Value> {
    entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            if let Some(object) = entry.as_object_mut() {
                for key in ["verified", "id", "message", "actualLine"] {
                    object.remove(key);
                }
            }
            entry
        })
        .collect()
}

/// Session-wide breakpoint memory.
#[derive(Default)]
pub struct Book {
    pub sources: BTreeMap<String, Vec<Value>>,
    pub functions: Vec<Value>,
}

impl Book {
    pub fn snapshot(&self) -> Value {
        json!({"sources": self.sources, "functions": self.functions})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_breakpoints_merge_per_line_and_stay_sorted() {
        let list = add_source(&[], json!({"line": 20}));
        let list = add_source(&list, json!({"line": 5}));
        let list = add_source(&list, json!({"line": 20, "condition": "x > 1"}));
        assert_eq!(
            list,
            vec![
                json!({"line": 5}),
                json!({"line": 20, "condition": "x > 1"})
            ]
        );
        let list = remove_source(&list, 5);
        assert_eq!(list, vec![json!({"line": 20, "condition": "x > 1"})]);
        assert!(remove_source(&list, 20).is_empty());
    }

    #[test]
    fn function_breakpoints_merge_by_name() {
        let list = add_function(&[], json!({"name": "main"}));
        let list = add_function(&list, json!({"name": "alpha"}));
        let list = add_function(&list, json!({"name": "main", "condition": "argc > 1"}));
        assert_eq!(
            list,
            vec![
                json!({"name": "alpha"}),
                json!({"name": "main", "condition": "argc > 1"})
            ]
        );
        assert_eq!(
            remove_function(&list, "alpha"),
            vec![json!({"name": "main", "condition": "argc > 1"})]
        );
    }

    #[test]
    fn annotations_round_trip_without_leaking_into_requests() {
        let requested = vec![json!({"line": 3}), json!({"line": 9})];
        let response = json!({"breakpoints": [{"verified": true, "id": 1, "line": 4}, {"verified": false, "message": "no code"}]});
        let annotated = annotate(&requested, &response);
        assert_eq!(
            annotated[0],
            json!({"line": 3, "verified": true, "id": 1, "actualLine": 4})
        );
        assert_eq!(annotated[1]["message"], "no code");
        assert_eq!(request_entries(&annotated), requested);
    }
}
