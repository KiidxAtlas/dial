use super::*;

#[test]
fn variables_and_if_blocks_render() {
    let context = Value::map([("name", Value::text("dial")), ("extra", Value::text(""))]);
    let out = render(
        "hi {{name}}\n{{#if extra}}\nno\n{{/if}}\n{{#if name}}\nyes\n{{/if}}\n",
        &context,
    )
    .unwrap();
    assert_eq!(out, "hi dial\nyes\n");
}

#[test]
fn each_and_list_render_nested_records() {
    let context = Value::map([(
        "entries",
        Value::List(vec![Value::map([
            ("path", Value::text("CHANGELOG.md")),
            (
                "sections",
                Value::List(vec![Value::map([
                    ("name", Value::text("Added")),
                    (
                        "items",
                        Value::List(vec![Value::text("one"), Value::text("two")]),
                    ),
                ])]),
            ),
        ])]),
    )]);
    let template = "{{#each entries}}\n### {{path}}\n{{#each sections}}\n{{name}}:\n{{#list items prefix=\"- \" join=\"\\n\"}}{{this}}{{/list}}\n{{/each}}\n{{/each}}\n";
    let out = render(template, &context).unwrap();
    assert_eq!(out, "### CHANGELOG.md\nAdded:\n- one\n- two\n");
}

#[test]
fn else_branch_renders_when_falsy() {
    let context = Value::map([("goal", Value::text(""))]);
    let out = render("{{#if goal}}\nset\n{{else}}\ndefault\n{{/if}}\n", &context).unwrap();
    assert_eq!(out, "default\n");
}

#[test]
fn unclosed_block_is_an_error() {
    assert!(render("{{#if x}}", &Value::map([])).is_err());
}
