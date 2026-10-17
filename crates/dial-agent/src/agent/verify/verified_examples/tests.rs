use super::*;
use crate::config::types::Outcome;
use serde_json::json;

fn call(name: &str, arguments: Value) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{
        "id":format!("call-{name}"),"type":"function",
        "function":{"name":name,"arguments":arguments.to_string()}
    }]})
}

fn record(store: &Store, domain: &str, task: &str, prompt: &str, payload: Value, score: f64) {
    let version = store.active().unwrap().id;
    let id = store
        .begin_trajectory(version, domain, task, prompt)
        .unwrap();
    store.save_messages(id, &payload).unwrap();
    store.finish_trajectory(id, Some("done"), None).unwrap();
    store
        .outcome(&Outcome {
            trajectory: id,
            score,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
}

fn turn(prompt: &str, tool: &str) -> Value {
    json!({"examples":[{"messages":[
        {"role":"user","content":prompt},
        call(tool, json!({"path":"private/secret.txt"})),
        {"role":"tool","tool_call_id":format!("call-{tool}"),"content":"SECRET_OUTPUT"},
        {"role":"assistant","content":"done"}
    ],"tools":[]}]})
}

#[test]
fn examples_are_live_project_scoped_relevant_offered_and_omit_old_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        "test-model",
        "test-usecase",
        &Default::default(),
    )
    .unwrap();
    let prompt = "Refactor the tokenizer module for unicode identifiers";
    record(
        &store,
        "coding",
        "session:a",
        prompt,
        json!({"examples":[{"messages":[
        {"role":"system","content":"rules"},
        {"role":"user","content":"Earlier unrelated request"},
        call("glob", json!({"pattern":"src/**"})),
        {"role":"assistant","content":"found"},
        {"role":"user","content":format!("{prompt}\n\nAdvisory notes")},
        call("read", json!({"path":"private/secret.txt"})),
        {"role":"tool","tool_call_id":"call-read","content":"SECRET_OUTPUT"},
        call("edit", json!({"path":"private/secret.txt","input":"SECRET_VALUE"})),
        {"role":"tool","tool_call_id":"call-edit","content":"ok"},
        call("bash", json!({"command":"cargo test SECRET_COMMAND"})),
        {"role":"tool","tool_call_id":"call-bash","content":"ok"},
        {"role":"assistant","content":"done"}
    ],"tools":[]}]}),
        100.0,
    );
    record(
        &store,
        "selfplay:files",
        "selfplay",
        "Rename the scheduler queue",
        turn("Rename the scheduler queue", "read"),
        100.0,
    );
    record(
        &store,
        "coding",
        "session:b",
        "Migrate the billing ledger",
        json!({"project_root":"/workspace/b","messages":[
            {"role":"user","content":"Migrate the billing ledger"},
            call("read", json!({"path":"ledger.rs"}))
        ]}),
        100.0,
    );
    record(
        &store,
        "coding",
        "session:c",
        "Document the cache eviction",
        turn("Document the cache eviction", "read"),
        90.0,
    );
    let offered: Vec<String> = ["read", "edit", "bash", "glob"].map(String::from).to_vec();
    let lookup = |query: &str, offered: &[String]| {
        verified_examples(&store, "/workspace/a", query, offered).unwrap()
    };

    let examples = lookup("please refactor the tokenizer for unicode", &offered);
    assert_eq!(examples.len(), 1);
    assert!(examples[0].contains(prompt) && examples[0].contains("by user"));
    assert!(
        examples[0].contains("used: read(path) -> edit(input, path) -> bash(command)."),
        "only this turn's calls, in order: {}",
        examples[0]
    );
    for leaked in [
        "secret.txt",
        "SECRET_VALUE",
        "SECRET_COMMAND",
        "SECRET_OUTPUT",
        "Advisory",
    ] {
        assert!(
            !examples[0].contains(leaked),
            "examples must not replay old values: {leaked}"
        );
    }

    // Relevance: four or more query terms need two shared terms.
    assert!(lookup("weather forecast paris", &offered).is_empty());
    assert!(lookup("tokenizer weather forecast paris london", &offered).is_empty());
    assert_eq!(
        lookup("unicode tokenizer weather forecast", &offered).len(),
        1
    );

    assert_eq!(
        lookup("refactor the tokenizer", &["read".into()]),
        [format!(
            "Verified task \"{prompt}\" (verified by user) used: read(path). Re-read and validate current values."
        )]
    );
    assert!(lookup("refactor the tokenizer", &["write".into()]).is_empty());
    assert!(
        lookup("rename the scheduler queue", &offered).is_empty(),
        "self-play is excluded"
    );
    assert!(
        lookup("migrate the billing ledger", &offered).is_empty(),
        "other projects are excluded"
    );
    assert!(
        lookup("document the cache eviction", &offered).is_empty(),
        "only perfect scores count"
    );
}
