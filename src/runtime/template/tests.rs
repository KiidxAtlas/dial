use super::*;

fn render(source: &str, data: Value) -> String {
    let template = compile(source).expect("template compiles");
    render_system_prompt(&template, &data).expect("template renders")
}

fn render_err(source: &str, data: Value) -> String {
    let template = compile(source).expect("template compiles");
    render_system_prompt(&template, &data)
        .expect_err("template fails")
        .to_string()
}

#[test]
fn substitutes_variables_and_nested_paths() {
    let out = render(
        "Dir: {{ cwd }} / {{ project.name }} / {{ missing }}.",
        json!({"cwd": "/work", "project": {"name": "dial"}}),
    );
    assert_eq!(out, "Dir: /work / dial / .");
}

#[test]
fn if_else_and_unless_choose_branches() {
    let source = "{{#if soul}}S{{else}}N{{/if}}|{{#unless soul}}U{{/unless}}";
    assert_eq!(render(source, json!({"soul": null})), "N|U");
    assert_eq!(render(source, json!({"soul": "x"})), "S|");
    assert_eq!(
        render("{{#if tools}}T{{else}}E{{/if}}", json!({"tools": []})),
        "E"
    );
}

#[test]
fn each_exposes_index_and_parent_paths() {
    let out = render(
        "{{#each tools}}{{@index}}={{this}}@{{../cwd}}{{#unless @last}},{{/unless}}{{/each}}",
        json!({"cwd": "/w", "tools": ["read", "edit"]}),
    );
    assert_eq!(out, "0=read@/w,1=edit@/w");
}

#[test]
fn unescaped_output_keeps_markup_characters() {
    let data = json!({"x": "<a & b>"});
    assert_eq!(
        render("{{x}} {{{x}}} {{&x}}", data),
        "<a & b> <a & b> <a & b>"
    );
}

#[test]
fn standalone_block_lines_leave_no_blank_line() {
    let source = "a\n{{#if x}}\nb\n{{/if}}\nc\n";
    assert_eq!(render(source, json!({"x": true})), "a\nb\nc");
    assert_eq!(render(source, json!({"x": false})), "a\nc");
}

#[test]
fn helpers_when_join_and_pluralize() {
    let out = render(
        "{{#when n \"==\" 2}}two{{else}}other{{/when}} {{join xs \"|\"}} {{pluralize n \"item\" \"items\"}}",
        json!({"n": 2, "xs": ["a", "b"]}),
    );
    assert_eq!(out, "two a|b 2 items");
}

#[test]
fn unbalanced_block_is_an_error() {
    let error = compile("{{#if a}}x")
        .expect_err("unclosed block fails")
        .to_string();
    assert!(error.contains("Invalid system prompt template"), "{error}");
    assert!(error.contains("unclosed block if"), "{error}");
}

#[test]
fn mismatched_close_is_an_error() {
    let error = compile("{{#if a}}x{{/each}}")
        .expect_err("mismatch fails")
        .to_string();
    assert!(error.contains("mismatched"), "{error}");
}

#[test]
fn empty_template_is_an_error() {
    let error = compile(" \n\t")
        .expect_err("empty template fails")
        .to_string();
    assert_eq!(error, "System prompt template must not be empty");
}

#[test]
fn missing_helper_with_arguments_is_an_error() {
    let error = render_err("{{nope 1}}", json!({}));
    assert!(error.contains("Missing helper: \"nope\""), "{error}");
}

#[test]
fn post_render_collapses_blank_runs_and_trims_edges() {
    assert_eq!(render("\n\na  \n\n\n\nb   \n  \n", json!({})), "a\n\nb");
}

#[test]
fn arg_is_one_based_over_the_current_context_args() {
    assert_eq!(
        render(
            "{{arg 1}}|{{arg 2}}|{{arg 3}}|{{arg 0}}|{{arg \"2\"}}|{{arg}}",
            json!({"args": ["a", "b"]})
        ),
        "a|b|||b|"
    );
    assert_eq!(render("{{arg 1}}x", json!({})), "x");
    assert_eq!(
        render(
            "{{#each items}}{{arg 1}}{{/each}}",
            json!({"items": [{"args": ["x"]}, {"args": ["y"]}]})
        ),
        "xy"
    );
}

#[test]
fn escape_xml_escapes_markup_and_null_is_empty() {
    let out = render(
        "{{escapeXml x}}|{{escapeXml missing}}|",
        json!({"x": "<a & \"b\">"}),
    );
    assert_eq!(out, "&lt;a &amp; &quot;b&quot;&gt;||");
}

#[test]
fn table_renders_headers_and_trimmed_rows() {
    let source = "{{#table rows headers=\"Name|Kind\"}} {{name}} | {{kind}} {{/table}}";
    let rows = json!({"rows": [{"name": "a", "kind": "x"}, {"name": "b", "kind": "y"}]});
    assert_eq!(render(source, rows), "|Name|Kind|\n|---|---|\n|a|x|\n|b|y|");
    assert_eq!(
        render(
            "{{#table rows}}{{name}}{{/table}}",
            json!({"rows": [{"name": "a"}, {"name": "b"}]})
        ),
        "|a|\n|b|"
    );
    assert_eq!(
        render("{{#table rows}}{{name}}{{/table}}|", json!({"rows": []})),
        "|"
    );
}

#[test]
fn codeblock_fences_trimmed_content() {
    assert_eq!(
        render(
            "{{#codeblock lang=\"diff\"}}{{body}}{{/codeblock}}",
            json!({"body": "\n+ a\n"})
        ),
        "```diff\n+ a\n```"
    );
    assert_eq!(
        render("{{#codeblock}}x{{/codeblock}}", json!({})),
        "```\nx\n```"
    );
}

#[test]
fn xml_wraps_nonempty_content_only() {
    let source = "{{#xml \"note\"}}{{x}}{{/xml}}|";
    assert_eq!(render(source, json!({"x": "hi"})), "<note>\nhi\n</note>|");
    assert_eq!(render(source, json!({"x": "  "})), "|");
}

#[test]
fn registered_partials_render_and_unknown_partials_fail() {
    register_partial("dial-test-greeting", "hello {{who}}").expect("partial registers");
    assert_eq!(
        render("{{> dial-test-greeting}}!", json!({"who": "x"})),
        "hello x!"
    );
    assert!(register_partial("dial-test-broken", "{{#if a}}").is_err());
    let error = render_err("{{> dial-test-absent}}", json!({}));
    assert!(
        error.contains("The partial dial-test-absent could not be found"),
        "{error}"
    );
}
