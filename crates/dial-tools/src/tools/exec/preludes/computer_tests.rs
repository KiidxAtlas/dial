use super::*;

fn constant(value: i64) -> PreludeHandler {
    Arc::new(move |_: Value, _: &mut PreludeCall<'_>| -> anyhow::Result<Value> { Ok(json!(value)) })
}

fn computer_no_dispatch(_: ToolCallback) -> Result<ToolOutput, String> {
    Err("no dispatch in this test".into())
}

fn computer_mutation_dispatch(
    approve: bool,
) -> impl FnMut(ToolCallback) -> Result<ToolOutput, String> {
    move |callback| match callback {
        ToolCallback::Call { name, .. } if name == "computer_test_mutate" => Ok(if approve {
            ToolOutput {
                execution: Default::default(),
                text: "done".into(),
                is_error: false,
                data: Some(json!("done")),
            }
        } else {
            ToolOutput {
                execution: Default::default(),
                text: "denied by confirm".into(),
                is_error: true,
                data: None,
            }
        }),
        other => Err(format!("unexpected callback {other:?}")),
    }
}

#[test]
fn computer_prelude_missing_is_explicit() {
    let error = invoke(
        "computer_test_missing_prelude",
        json!({}),
        &mut computer_no_dispatch,
    )
    .unwrap_err();
    assert_eq!(
        error,
        "Eval prelude \"computer_test_missing_prelude\" is not enabled in the current session."
    );
}

#[test]
fn computer_prelude_registration_replaces_by_name() {
    register_prelude(
        PreludeDef {
            name: "computer_test_replace",
            js: "var a = 1;",
            py: "a = 1",
        },
        constant(1),
    );
    register_prelude(
        PreludeDef {
            name: "computer_test_replace",
            js: "var a = 2;",
            py: "a = 2",
        },
        constant(2),
    );
    assert_eq!(
        invoke(
            "computer_test_replace",
            json!({}),
            &mut computer_no_dispatch
        )
        .unwrap(),
        json!(2)
    );
    assert_eq!(
        prelude_source_for(PreludeLanguage::Python)
            .matches("a = 2")
            .count(),
        1
    );
}

#[test]
fn computer_prelude_mutation_follows_dispatch_approval() {
    register_prelude(
        PreludeDef {
            name: "computer_test_mutate_prelude",
            js: "",
            py: "",
        },
        Arc::new(|_: Value, call: &mut PreludeCall<'_>| {
            call.tool("computer_test_mutate", json!({"action": "click"}))
                .map_err(anyhow::Error::msg)
        }),
    );
    let mut approved = computer_mutation_dispatch(true);
    assert_eq!(
        invoke("computer_test_mutate_prelude", json!({}), &mut approved).unwrap(),
        json!("done")
    );
    let mut denied = computer_mutation_dispatch(false);
    assert_eq!(
        invoke("computer_test_mutate_prelude", json!({}), &mut denied).unwrap_err(),
        "denied by confirm"
    );
}

#[test]
fn computer_display_appends_lines() {
    let display = DisplayBuffer::default();
    display.push("first");
    display.push("second\n");
    assert_eq!(display.take(), "first\nsecond\n");
    assert_eq!(display.take(), "");
}
