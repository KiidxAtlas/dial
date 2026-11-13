use super::{ISOLATION_BACKENDS, MAX_BATCH};
use crate::config::subagents::AGENTS;
use crate::config::types::Config;
use serde_json::Value;
use serde_json::json;

/// The `task` tool as offered to the model.
pub fn schema(config: &Config) -> Value {
    let types: String = AGENTS
        .iter()
        .map(|a| format!("\n- {}: {}", a.name, a.description))
        .collect();
    let presets = if config.subagents.is_empty() {
        String::new()
    } else {
        format!(
            "\nCustom read-only presets (scout with extra instructions): {}.",
            config
                .subagents
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let presets = if config.agents.is_empty() {
        presets
    } else {
        let custom: String = config
            .agents
            .iter()
            .map(|(name, a)| {
                format!(
                    "\n- {name}: {}",
                    if a.description.is_empty() {
                        "custom read-only agent"
                    } else {
                        &a.description
                    }
                )
            })
            .collect();
        format!("{presets}\nCustom agents:{custom}")
    };
    let description = format!(
        "Delegate independent work: tasks[] shares context; start returns ids, wait collects results. Default: read-only on main model. source_session attaches a record to a read-only audit, never permission. Types:{types}{presets}"
    );
    let mut entry_properties = json!({
        "acceptance_checks":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1},"description":"Names of configured checks, not shell commands. A mutating worker's changes merge only when these pass."},
        "agent":{"type":"string","description":"Agent type or preset; omitted uses a read-only agent on the main model."},
        "task":{"type":"string","description":"Complete assignment with scope and acceptance criteria."},
        "solution_space":{"type":"string","description":"What is fixed versus what the child may choose; never empty. Example: \"Fixed: edit only src/parser.rs. Free: helper names.\""},
        "skills":{"type":"array","items":{"type":"string"},"description":"Skills to load first."},
        "max_steps":{"type":"integer","minimum":0,"description":"Step limit; omitted uses the agent type's default (scout 30, worker 80), 0 means unlimited."},
        "name":{"type":"string","description":"Label shown to the user."},
        "output_schema":{"type":"object","description":"JSON Schema the final answer must satisfy."},
        "isolation":{"type":"string","enum":["copy"],"description":"Workers only: work in a private copy, merged on success unless the parent changed the same files."},
        "isolation_backend":{"type":"string","enum":ISOLATION_BACKENDS,"description":"Workers only; implies isolation. Fails before starting if unavailable."}
    });
    entry_properties.as_object_mut().unwrap().insert("source_session".into(), json!({"type":"string","minLength":1,"description":"Read-only agents: session or worker id whose record is attached to the assignment."}));
    entry_properties.as_object_mut().unwrap().insert(
        "model".into(),
        json!({"type":"string","description":"seance only: answering model."}),
    );
    if config.experimental.work_packet_contracts {
        entry_properties.as_object_mut().unwrap().insert("work_packet".into(), json!({
            "type":"object",
            "description":"Scoped handoff; the child reports evidence per acceptance item.",
            "properties":{
                "scope":{"type":"string","minLength":1,"maxLength":2048},
                "interface":{"type":"string","minLength":1,"maxLength":2048},
                "acceptance":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":512}}
            },
            "required":["scope","interface","acceptance"],
            "additionalProperties":false
        }));
    }
    let entry = json!({"type":"object","properties":entry_properties,
        "required":["task","solution_space"],"additionalProperties":false});
    let mut properties = serde_json::Map::from_iter([
        (
            "question".into(),
            json!({"type":"string","minLength":1,"maxLength":8000,"description":"question only: tool-free question over an immutable session snapshot."}),
        ),
        (
            "action".into(),
            json!({"type":"string","enum":["run","start","resume","status","wait","cancel","create","push","peek","close","verify","question","question_status","question_wait","question_cancel"],"description":"run(default): await tasks; start: live ids; resume: continue worker; status/wait/cancel: jobs; create/push/peek/close: pools; verify: contract checks. question: tool-free snapshot question; question_* manages only side questions."}),
        ),
        (
            "id".into(),
            json!({"type":"string","description":"task-N/pool-N; omit on status/wait for all. side-N required for question_wait/question_cancel; optional for question_status."}),
        ),
        (
            "message".into(),
            json!({"type":"string","description":"resume only: steering for the job's next request."}),
        ),
        (
            "timeout".into(),
            json!({"type":"integer","minimum":0,"maximum":120,"description":"wait seconds(default 30,max 120); question_wait(default 5,max 30) stops early for input without cancelling."}),
        ),
        (
            "task".into(),
            json!({"type":"string","description":"Single task: complete assignment."}),
        ),
        (
            "solution_space".into(),
            json!({"type":"string","description":"Single task: fixed versus free choices."}),
        ),
        (
            "context".into(),
            json!({"type":"string","description":"Shared background for every task: goal, constraints, interfaces."}),
        ),
        (
            "skills".into(),
            json!({"type":"array","items":{"type":"string"},"description":"Single task: skills to load first."}),
        ),
        (
            "agent".into(),
            json!({"type":"string","description":"Single task: agent type or preset (default scout)."}),
        ),
        (
            "max_steps".into(),
            json!({"type":"integer","minimum":0,"description":"Single task: step limit; 0 means unlimited."}),
        ),
        (
            "concurrency".into(),
            json!({"type":"integer","minimum":1,"maximum":32,"description":"create only: most pool workers at once."}),
        ),
        (
            "name".into(),
            json!({"type":"string","description":"Single task: label."}),
        ),
        (
            "output_schema".into(),
            json!({"type":"object","description":"JSON Schema for the final answer; batch default."}),
        ),
        (
            "isolation".into(),
            json!({"type":"string","enum":["copy"],"description":"Single worker: as tasks[].isolation."}),
        ),
        (
            "isolation_backend".into(),
            json!({"type":"string","enum":ISOLATION_BACKENDS,"description":"Single worker: as tasks[].isolation_backend."}),
        ),
        (
            "tasks".into(),
            json!({"type":"array","maxItems":MAX_BATCH,"items":entry,"description":"Independent tasks instead of `task`."}),
        ),
    ]);
    properties.insert("acceptance_checks".into(), {
        // Same contract shape as the batch field; the full description is not repeated.
        let mut single = entry_properties["acceptance_checks"].clone();
        single["description"] = json!("Single task: as tasks[].acceptance_checks.");
        single
    });
    properties.insert("contract".into(), json!({
        "type":"object",
        "description":"Opt-in durable workflow node (start only, with acceptance_checks). The id binds the assignment and up to 16 accepted dependencies; use status or verify with it after restart.",
        "properties":{
            "id":{"type":"string","minLength":1,"maxLength":64},
            "depends_on":{"type":"array","maxItems":16,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}}
        },
        "required":["id"],"additionalProperties":false
    }));
    properties.insert("source_session".into(), json!({"type":"string","minLength":1,"description":"Read-only agents: session or worker id whose record is attached to the assignment."}));
    properties.insert(
        "model".into(),
        json!({"type":"string","description":"seance only: answering model."}),
    );
    if config.experimental.work_packet_contracts {
        properties.insert("work_packet".into(), json!({
            "type":"object",
            "description":"Scoped handoff contract for this task.",
            "properties":{
                "scope":{"type":"string","minLength":1,"maxLength":2048},
                "interface":{"type":"string","minLength":1,"maxLength":2048},
                "acceptance":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":512}}
            },
            "required":["scope","interface","acceptance"],
            "additionalProperties":false
        }));
    }
    if config.experimental.profile_matching && !config.model_profiles.is_empty() {
        let names = config.model_profiles.keys().cloned().collect::<Vec<_>>();
        properties.insert("model_profile".into(), json!({
            "type":"string",
            "enum":names,
            "description":"Override the role's configured model profile only when the assignment asks for it."
        }));
    }
    json!({"type":"function","function":{"name":"task","description":description,
        "parameters":{"type":"object","properties":properties,"additionalProperties":false}}})
}

pub(in crate::agent) fn nested_schema(config: &Config) -> Value {
    let mut schema = schema(config);
    let props = schema["function"]["parameters"]["properties"]
        .as_object_mut()
        .expect("task properties");
    props.insert("action".into(), json!({"type":"string","enum":["run"],"description":"Nested tasks run synchronously within this worker's spawn policy and depth limit."}));
    for name in ["id", "message", "timeout", "contract"] {
        props.remove(name);
    }
    schema
}
