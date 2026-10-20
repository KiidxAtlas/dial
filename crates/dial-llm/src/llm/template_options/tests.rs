use super::*;

fn option(name: &str, kind: OptionKind, default: Option<&str>) -> TemplateOption {
    TemplateOption {
        name: name.into(),
        kind,
        default: default.map(str::to_owned),
    }
}

#[test]
fn defined_checks_become_options_with_kinds_from_their_use() {
    let template = r#"
{%- if enable_thinking is defined and enable_thinking is false %}<think></think>{% endif -%}
{%- if not reasoning_effort is defined %}{%- set reasoning_effort = "medium" %}{% endif %}
Reasoning: {{ reasoning_effort }}
{%- if mode is defined and mode == "fast" %}f{% elif mode == 'slow' %}s{% endif %}
{%- if date_string is not defined %}{{ date_string }}{% endif %}
{%- if preserve_thinking is defined -%}{%- if preserve_thinking -%}p{%- endif -%}{%- endif -%}
"#;
    assert_eq!(
        parse(template),
        vec![
            option("enable_thinking", OptionKind::Flag, None),
            option("reasoning_effort", OptionKind::Text, Some("medium")),
            option(
                "mode",
                OptionKind::Choice(vec!["fast".into(), "slow".into()]),
                None
            ),
            option("date_string", OptionKind::Text, None),
            option("preserve_thinking", OptionKind::Flag, None),
        ]
    );
}

#[test]
fn renderer_variables_attributes_loop_and_macro_names_and_text_are_not_options() {
    let template = r#"
{{ bos_token }}{% if tools is defined %}{% endif %}
{%- for message in messages %}{% if message.content is defined %}{% endif %}{% endfor %}
{%- for key, value in params %}{% if value is defined %}{% endif %}{% endfor %}
{%- macro render(item, extra=1) %}{% if item is defined or extra is defined %}{% endif %}{% endmacro %}
{%- if tool_call.function is defined %}{% endif %}
{# commented is defined #} plain text, not code: shown is defined
"#;
    assert_eq!(parse(template), Vec::new());
}

#[test]
fn only_dial_served_mlx_models_are_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("chat_template.jinja"),
        "{% if enable_thinking is defined %}{% endif %}",
    )
    .unwrap();
    let model = dir.path().to_str().unwrap();
    assert_eq!(detect("mlx", model).len(), 1);
    assert!(detect("ollama", model).is_empty());
    assert!(detect("mlx", "org/not-a-directory").is_empty());
}
