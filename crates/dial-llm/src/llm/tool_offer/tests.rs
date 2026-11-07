use super::*;

fn tool(name: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":name,"parameters":{"type":"object"}}})
}

fn marked(name: &str, callable: bool, deferred: bool) -> Value {
    let mut schema = tool(name);
    mark(&mut schema, Mark { callable, deferred });
    schema
}

fn chat(tools: Vec<Value>, messages: Vec<Value>) -> Chat {
    Chat {
        messages,
        tools,
        temperature: 0.0,
        max_tokens: 16,
        seed: None,
    }
}

fn names(tools: &[Value]) -> Vec<&str> {
    tools
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect()
}

fn surface() -> Vec<Value> {
    vec![
        tool("read"),
        marked("exit_plan", false, false),
        marked("lsp", true, true),
        marked("git", false, true),
    ]
}

#[test]
fn callable_mark_leaves_an_ordinary_schema_unchanged() {
    let mut schema = tool("read");
    mark(&mut schema, Mark::CALLABLE);
    assert_eq!(schema, tool("read"));
    mark(
        &mut schema,
        Mark {
            callable: false,
            deferred: true,
        },
    );
    assert!(!mark_of(&schema).callable && mark_of(&schema).deferred);
    mark(&mut schema, Mark::CALLABLE);
    assert_eq!(schema, tool("read"));
}

#[test]
fn unmarked_requests_are_borrowed_for_every_exposure() {
    let req = chat(
        vec![tool("read")],
        vec![json!({"role":"user","content":"x"})],
    );
    for exposure in [Exposure::Plain, Exposure::Deferred, Exposure::AllowedTools] {
        assert!(matches!(normalize(&req, exposure), Cow::Borrowed(_)));
    }
}

#[test]
fn plain_sends_exactly_the_callable_tools_without_marks_or_references() {
    let req = chat(
        surface(),
        vec![json!({"role":"tool","tool_call_id":"a","content":"loaded",REFERENCES:["lsp"]})],
    );
    let plain = normalize(&req, Exposure::Plain);
    assert_eq!(plain.tools, vec![tool("read"), tool("lsp")]);
    assert_eq!(plain.tools, *callable(&req.tools));
    assert!(plain.messages[0].get(REFERENCES).is_none());
    assert_eq!(plain.messages[0]["content"], "loaded");
}

#[test]
fn deferred_keeps_the_marked_surface_and_references() {
    let req = chat(
        surface(),
        vec![json!({"role":"tool","tool_call_id":"a","content":"loaded",REFERENCES:["lsp"]})],
    );
    let deferred = normalize(&req, Exposure::Deferred);
    assert!(matches!(deferred, Cow::Borrowed(_)));
    assert_eq!(references(&deferred.messages[0]), ["lsp"]);
    // A surface whose every tool is deferred is rejected by the API: send the plain form.
    let all_deferred = chat(vec![marked("lsp", true, true)], Vec::new());
    assert_eq!(
        normalize(&all_deferred, Exposure::Deferred).tools,
        vec![tool("lsp")]
    );
}

#[test]
fn allowed_tools_sends_offered_tools_and_names_the_callable_ones() {
    let req = chat(
        surface(),
        vec![json!({"role":"tool","tool_call_id":"a","content":"loaded",REFERENCES:["lsp"]})],
    );
    let allowed = normalize(&req, Exposure::AllowedTools);
    assert_eq!(names(&allowed.tools), ["read", "exit_plan", "lsp"]);
    assert_eq!(allowed_names(&allowed.tools), Some(vec!["read", "lsp"]));
    assert!(allowed.messages[0].get(REFERENCES).is_none());
    // Nothing narrowed: no `allowed_tools` at all.
    assert_eq!(allowed_names(&[tool("read"), tool("lsp")]), None);
}
