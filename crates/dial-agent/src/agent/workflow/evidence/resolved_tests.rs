use super::*;

fn exchange(id: &str, name: &str, arguments: Value, failed: bool) -> Vec<Value> {
    vec![
        json!({"role":"assistant","tool_calls":[{"id":id,"function":{"name":name,"arguments":arguments.to_string()}}]}),
        json!({"role":"tool","tool_call_id":id,"content":"diagnostic details ".repeat(100),"dial_evidence":{"artifact":format!("artifact://{id}"),"is_error":failed},"dial_execution":{"status":if failed {"error"} else {"ok"},"effects":"none"}}),
    ]
}

fn policy() -> Policy {
    Policy {
        pressure: true,
        protect_tokens: 0,
        ..Policy::calm()
    }
}

#[test]
fn only_exact_read_only_retries_with_positive_success_resolve_diagnostics() {
    for (name, changed_arguments, unknown, error_metadata, missing_execution, expected) in [
        ("lsp", false, false, Some(false), 0, true),
        ("lsp", true, false, Some(false), 0, false),
        ("lsp", false, true, Some(false), 0, false),
        ("lsp", false, false, None, 0, false),
        ("lsp", false, false, Some(true), 0, false),
        ("lsp", false, false, Some(false), 1, false),
        ("lsp", false, false, Some(false), 2, false),
        ("bash", false, false, Some(false), 0, false),
        ("write", false, false, Some(false), 0, false),
    ] {
        let arguments = json!({"action":"diagnostics","path":"src/a.rs"});
        let mut messages = exchange("failure", name, arguments.clone(), true);
        let retry = if changed_arguments {
            json!({"action":"diagnostics","path":"src/b.rs"})
        } else {
            arguments
        };
        messages.extend(exchange("retry", name, retry, false));
        messages[3]["dial_evidence"]["is_error"] = json!(error_metadata);
        if unknown {
            messages[3]["dial_execution"] = json!({"effects":"unknown"});
        }
        match missing_execution {
            1 => {
                messages[3]["dial_execution"]
                    .as_object_mut()
                    .unwrap()
                    .remove("status");
            }
            2 => {
                messages[3]["dial_execution"]
                    .as_object_mut()
                    .unwrap()
                    .remove("effects");
            }
            _ => {}
        }
        let original = messages.clone();
        let mut memo = Supersession::default();
        project(&mut messages, &mut memo, &policy());
        assert_eq!(
            !memo.holds_full("failure"),
            expected,
            "{name} changed={changed_arguments} unknown={unknown} metadata={error_metadata:?}"
        );
        // Projection finishes with the shared provider-wire canonicalizer; controller-only
        // dial_evidence is intentionally removed. Preserve the tool identity and all wire fields
        // except the condensed text, with the diagnostic artifact asserted below.
        let mut expected_wire = original[1].clone();
        expected_wire["content"] = messages[1]["content"].clone();
        crate::llm::messages::canonicalize(&mut expected_wire);
        assert_eq!(messages[1], expected_wire);
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "failure");
        if expected {
            let text = messages[1]["content"].as_str().unwrap();
            assert!(text.contains("retry") && text.contains("artifact://failure"));
            assert!(text.contains("does not verify other work"));
            assert!(text.len() < original[1]["content"].as_str().unwrap().len());
        } else {
            assert_eq!(messages[1]["content"], original[1]["content"]);
        }
    }
}

#[test]
fn resolved_projection_is_stable_and_protected_failures_remain_full() {
    let source = || {
        let mut messages = exchange("old", "read", json!({"path":"a.rs"}), true);
        messages.extend(exchange("success", "read", json!({"path":"a.rs"}), false));
        messages
    };
    let mut protected = source();
    project(
        &mut protected,
        &mut Supersession::default(),
        &Policy {
            protect_tokens: usize::MAX,
            ..policy()
        },
    );
    assert_eq!(protected[1]["content"], source()[1]["content"]);
    let mut memo = Supersession::default();
    let mut first = source();
    project(&mut first, &mut memo, &policy());
    let mut later = source();
    later.extend(exchange("another", "read", json!({"path":"a.rs"}), false));
    project(&mut later, &mut memo, &Policy::calm());
    assert_eq!(first[1]["content"], later[1]["content"]);
}
