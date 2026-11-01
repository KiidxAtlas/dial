use super::*;
use serde_json::json;

fn reply(text: &str) -> Reply {
    Reply {
        text: text.to_string(),
        tool_calls: vec![],
        finish_reason: "stop".into(),
        prompt_tokens: None,
        completion_tokens: None,
        ..Default::default()
    }
}

fn applied(text: &str) -> Reply {
    let mut r = reply(text);
    apply(&mut r);
    r
}

#[test]
fn single_tagged_call() {
    let r = applied(
        "<tool_call>\n{\"name\": \"read\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>",
    );
    assert_eq!(r.text, "");
    assert_eq!(r.finish_reason, "tool_calls");
    assert_eq!(
        r.tool_calls,
        vec![ToolCall {
            id: "call_0".into(),
            name: "read".into(),
            arguments: json!({"path": "a.rs"}),
            malformed_arguments: false
        }]
    );
}

#[test]
fn text_before_between_and_after_is_kept_in_order() {
    let r = applied(
        "Let me look.\n<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\nthen\n\
             <tool_call>{\"name\":\"b\",\"arguments\":{\"x\":1}}</tool_call> done",
    );
    assert_eq!(r.text, "Let me look.\nthen\ndone");
    let names: Vec<_> = r
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str()))
        .collect();
    assert_eq!(names, [("call_0", "a"), ("call_1", "b")]);
    assert_eq!(r.tool_calls[1].arguments, json!({"x": 1}));
}

#[test]
fn braces_and_closing_tag_inside_argument_strings() {
    let content = "fn main() { println!(\"}}\"); } </tool_call> {\"name\": 1}";
    let block = json!({"name": "write", "arguments": {"content": content}});
    let r = applied(&format!("<tool_call>{block}</tool_call>trailing"));
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].arguments["content"], content);
    assert_eq!(r.text, "trailing");
}

#[test]
fn arguments_as_json_string_or_missing_or_parameters_alias() {
    let r = applied(
        "<tool_call>{\"name\":\"a\",\"arguments\":\"{\\\"k\\\": 2}\"}</tool_call>\
             <tool_call>{\"name\":\"b\"}</tool_call>\
             <tool_call>{\"name\":\"c\",\"parameters\":{\"p\":true}}</tool_call>\
             <tool_call>{\"name\":\"d\",\"arguments\":[1]}</tool_call>",
    );
    assert_eq!(r.tool_calls[0].arguments, json!({"k": 2}));
    assert_eq!(r.tool_calls[1].arguments, json!({}));
    assert!(!r.tool_calls[1].malformed_arguments);
    assert_eq!(r.tool_calls[2].arguments, json!({"p": true}));
    assert!(r.tool_calls[3].malformed_arguments);
    assert_eq!(r.tool_calls[3].name, "d");
}

#[test]
fn malformed_blocks_become_malformed_calls_with_best_effort_name() {
    let r = applied("<tool_call>{\"name\": \"bash\", \"arguments\": {\"cmd\": </tool_call>after");
    assert_eq!(r.tool_calls.len(), 1);
    assert!(r.tool_calls[0].malformed_arguments);
    assert_eq!(r.tool_calls[0].name, "bash");
    assert!(r.tool_calls[0].arguments.is_null());
    assert_eq!(r.text, "after");
    assert_eq!(r.finish_reason, "tool_calls");

    let nameless = applied("<tool_call>{\"arguments\": {}}</tool_call>");
    assert!(nameless.tool_calls[0].malformed_arguments && nameless.tool_calls[0].name.is_empty());
    let scalar = applied("<tool_call>42</tool_call>");
    assert!(scalar.tool_calls[0].malformed_arguments);
}

#[test]
fn unterminated_block_is_still_a_call() {
    // Generation stopped at max_tokens before the closing tag.
    let ok = applied("<tool_call>{\"name\":\"a\",\"arguments\":{}}");
    assert!(!ok.tool_calls[0].malformed_arguments);
    let cut = applied("hi <tool_call>{\"name\":\"a\",\"arguments\":{\"p\":\"abc");
    assert!(cut.tool_calls[0].malformed_arguments);
    assert_eq!(cut.tool_calls[0].name, "a");
    assert_eq!(cut.text, "hi");
}

#[test]
fn partial_tags_are_plain_text() {
    for text in [
        "<tool_cal",
        "x <tool_call",
        "</tool_call>",
        "use the <tool> tag",
        "tool_call",
    ] {
        let r = applied(text);
        assert_eq!(r.text, text);
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.finish_reason, "stop");
    }
}

#[test]
fn structured_calls_win_over_text() {
    let mut r = reply("<tool_call>{\"name\":\"x\",\"arguments\":{}}</tool_call>");
    r.tool_calls.push(ToolCall {
        id: "s".into(),
        name: "real".into(),
        arguments: json!({}),
        malformed_arguments: false,
    });
    apply(&mut r);
    assert_eq!(r.tool_calls.len(), 1);
    assert!(r.text.contains("<tool_call>"));
}

#[test]
fn fenced_call_requires_exactly_name_and_arguments() {
    let r = applied("Sure:\n```json\n{\"name\": \"ls\", \"arguments\": {\"dir\": \".\"}}\n```\nok");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].arguments, json!({"dir": "."}));
    assert_eq!(r.text, "Sure:\nok");
    assert_eq!(r.finish_reason, "tool_calls");

    let bare = applied("```\n{\"name\": \"ls\", \"arguments\": {}}\n```");
    assert_eq!(bare.tool_calls.len(), 1);

    for body in [
        "{\"name\": \"ls\", \"arguments\": {}, \"extra\": 1}",
        "{\"name\": \"ls\"}",
        "{\"arguments\": {}}",
        "{\"name\": \"\", \"arguments\": {}}",
        "{\"name\": 3, \"arguments\": {}}",
        "[{\"name\": \"ls\", \"arguments\": {}}]",
        "not json",
    ] {
        let text = format!("```json\n{body}\n```");
        let r = applied(&text);
        assert!(r.tool_calls.is_empty(), "{body}");
        assert_eq!(r.text, text);
    }
}

#[test]
fn fenced_other_languages_and_pairing() {
    let text = "```rust\nlet a = 1;\n```\n```json\n{\"name\":\"x\",\"arguments\":{}}\n```";
    let r = applied(text);
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.text, "```rust\nlet a = 1;\n```");
    // An unclosed fence is not a call.
    assert!(
        applied("```json\n{\"name\":\"x\",\"arguments\":{}}")
            .tool_calls
            .is_empty()
    );
}

#[test]
fn tagged_form_takes_precedence_over_fences() {
    let r = applied(
        "```json\n{\"name\":\"fenced\",\"arguments\":{}}\n```<tool_call>{\"name\":\"tagged\",\"arguments\":{}}</tool_call>",
    );
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].name, "tagged");
    assert!(r.text.contains("fenced"));
}

fn feed(deltas: &[&str]) -> (String, String) {
    let mut out = String::new();
    let mut sink = |s: &str| {
        out.push_str(s);
        true
    };
    let mut hidden = String::new();
    let mut hidden_sink = |s: &str| {
        hidden.push_str(s);
        true
    };
    let mut hider = Hider::new(&mut sink, &mut hidden_sink, true);
    let mut all = String::new();
    for d in deltas {
        all.push_str(d);
        assert!(hider.push(d));
    }
    assert!(hider.finish());
    drop(hider);
    assert_eq!(format!("{out}{hidden}"), all);
    (out, all)
}

#[test]
fn hider_withholds_tag_even_when_split_across_deltas() {
    let (out, _) = feed(&[
        "Let me ",
        "check <tool",
        "_ca",
        "ll>{\"name\":",
        "\"a\"}</tool_call>",
    ]);
    assert_eq!(out, "Let me check ");
    let (out, _) = feed(&["<", "tool_call>", "{}"]);
    assert_eq!(out, "");
}

#[test]
fn hider_reports_split_opener_and_later_hidden_chunks_exactly_once() {
    let mut visible = String::new();
    let mut hidden = Vec::new();
    let mut sink = |s: &str| {
        visible.push_str(s);
        true
    };
    let mut hidden_sink = |s: &str| {
        hidden.push(s.to_string());
        true
    };
    let mut hider = Hider::new(&mut sink, &mut hidden_sink, true);
    assert!(hider.push("Checking <tool"));
    assert!(hider.push("_ca"));
    assert!(hider.push("ll>{\"name\":"));
    assert!(hider.push("\"read\",\"arguments\":{\"path\":\"é.rs\"}}"));
    assert!(hider.push(""));
    assert!(hider.push("</tool_call>"));
    assert!(hider.finish());
    assert!(hider.finish());
    drop(hider);
    assert_eq!(visible, "Checking ");
    assert_eq!(
        hidden,
        [
            "<tool_call>{\"name\":",
            "\"read\",\"arguments\":{\"path\":\"é.rs\"}}",
            "</tool_call>",
        ]
    );
}

#[test]
fn hider_honors_hidden_cancellation_on_recognition_and_later_suffix() {
    for cancel_on in [1, 2] {
        let mut visible = String::new();
        let mut hidden = Vec::new();
        let mut sink = |s: &str| {
            visible.push_str(s);
            true
        };
        let mut hidden_sink = |s: &str| {
            hidden.push(s.to_string());
            hidden.len() != cancel_on
        };
        let mut hider = Hider::new(&mut sink, &mut hidden_sink, true);
        assert!(hider.push("Checking <tool"));
        let mut stopped = false;
        for delta in ["_call>{", "\"name\":\"read\"}", "</tool_call>"] {
            if !hider.push(delta) {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "hidden callback must cancel on event {cancel_on}");
        drop(hider);
        assert_eq!(visible, "Checking ");
        assert_eq!(hidden.len(), cancel_on);
        assert_eq!(hidden[0], "<tool_call>{");
        if cancel_on == 2 {
            assert_eq!(hidden[1], "\"name\":\"read\"}");
        }
    }
}

#[test]
fn hider_stops_when_consumer_declines_and_can_be_disabled() {
    let mut calls = 0;
    let mut sink = |_: &str| {
        calls += 1;
        false
    };
    let mut hidden_sink = |_: &str| true;
    let mut hider = Hider::new(&mut sink, &mut hidden_sink, true);
    assert!(!hider.push("hello"));
    drop(hider);
    assert_eq!(calls, 1);

    let mut out = String::new();
    let mut sink = |s: &str| {
        out.push_str(s);
        true
    };
    let mut hidden_sink = |_: &str| panic!("disabled hiding must not report hidden output");
    let mut hider = Hider::new(&mut sink, &mut hidden_sink, false);
    assert!(hider.push("<tool_call>x"));
    assert!(hider.finish());
    drop(hider);
    assert_eq!(out, "<tool_call>x");
}

#[test]
fn hider_releases_lookalike_prefixes() {
    let (out, all) = feed(&["a <tool", " is a thing <", "b> and <tool_c"]);
    assert_eq!(out, all, "text that is not a tag must be delivered in full");
}

#[test]
fn qwen_xml_calls_with_and_without_wrapper() {
    let r = applied(
        "Checking.\n<tool_call>\n<function=read_file>\n<parameter=path>\na.rs\n</parameter>\n<parameter=limit>\n10\n</parameter>\n</function>\n</tool_call>\nDone.",
    );
    assert_eq!(r.text, "Checking.\nDone.");
    assert_eq!(r.finish_reason, "tool_calls");
    assert_eq!(
        r.tool_calls,
        vec![ToolCall {
            id: "call_0".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.rs", "limit": "10"}),
            malformed_arguments: false
        }]
    );
    let bare = applied("<function=ls>\n<parameter=dir>\n.\n</parameter>\n</function>");
    assert_eq!(bare.tool_calls[0].name, "ls");
    assert_eq!(bare.tool_calls[0].arguments, json!({"dir": "."}));
    assert_eq!(bare.text, "");
}

#[test]
fn qwen_values_are_raw_text_and_only_objects_or_arrays_are_parsed() {
    let r = applied(
        "<function=edit>\n<parameter=text>\n  two\nlines  \n</parameter>\n<parameter=opts>\n{\"force\": true}\n</parameter>\n<parameter=ids>\n[1, 2]\n</parameter>\n<parameter=flag>\ntrue\n</parameter>\n<parameter=empty>\n</parameter>\n</function>",
    );
    assert_eq!(
        r.tool_calls[0].arguments,
        json!({"text": "  two\nlines  ", "opts": {"force": true}, "ids": [1, 2], "flag": "true", "empty": ""})
    );
}

#[test]
fn qwen_malformed_blocks_are_not_executed() {
    let cut = applied("<tool_call>\n<function=bash>\n<parameter=cmd>\nrm -rf /");
    assert_eq!(cut.tool_calls.len(), 1);
    assert!(cut.tool_calls[0].malformed_arguments);
    assert_eq!(cut.tool_calls[0].name, "bash");
    assert!(cut.tool_calls[0].arguments.is_null());
    let open_param = applied("<function=bash><parameter=cmd>ls</function>");
    assert!(open_param.tool_calls[0].malformed_arguments);
    let nameless = applied("<function=>\n</function>");
    assert!(nameless.tool_calls[0].malformed_arguments && nameless.tool_calls[0].name.is_empty());
}

#[test]
fn glm_arg_pairs_become_a_call_with_raw_text_values() {
    let r = applied(
        "Looking.\n<tool_call>edit_file\n<arg_key>path</arg_key>\n<arg_value>a.rs</arg_value>\n<arg_key>text</arg_key>\n<arg_value>fn f() { \"x\" }</arg_value>\n<arg_key>opts</arg_key>\n<arg_value>{\"force\": true}</arg_value>\n<arg_key>limit</arg_key>\n<arg_value>10</arg_value>\n</tool_call>\nDone.",
    );
    assert_eq!(r.text, "Looking.\nDone.");
    assert_eq!(r.finish_reason, "tool_calls");
    assert_eq!(
        r.tool_calls,
        vec![ToolCall {
            id: "call_0".into(),
            name: "edit_file".into(),
            arguments: json!({"path": "a.rs", "text": "fn f() { \"x\" }", "opts": {"force": true}, "limit": "10"}),
            malformed_arguments: false
        }]
    );
}

#[test]
fn glm_calls_without_arguments_and_several_in_a_row() {
    let r = applied(
        "<tool_call>git_status</tool_call><tool_call>read\n<arg_key>path</arg_key><arg_value>b.rs</arg_value></tool_call>",
    );
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(r.tool_calls[0].name, "git_status");
    assert_eq!(r.tool_calls[0].arguments, json!({}));
    assert!(!r.tool_calls[0].malformed_arguments);
    assert_eq!(r.tool_calls[1].id, "call_1");
    assert_eq!(r.tool_calls[1].arguments, json!({"path": "b.rs"}));
    assert_eq!(r.text, "");
}

#[test]
fn glm_unterminated_blocks_are_only_calls_when_every_pair_is_closed() {
    let closed = applied("<tool_call>read\n<arg_key>path</arg_key><arg_value>a.rs</arg_value>");
    assert!(!closed.tool_calls[0].malformed_arguments);
    assert_eq!(closed.tool_calls[0].arguments, json!({"path": "a.rs"}));
    for cut in [
        "<tool_call>bash\n<arg_key>cmd</arg_key><arg_value>rm -rf /",
        "<tool_call>bash\n<arg_key>cmd</arg_key>",
        "<tool_call>bash\n<arg_key></arg_key><arg_value>x</arg_value></tool_call>after",
        "<tool_call>bash\n<arg_key>cmd</arg_key>oops<arg_value>x</arg_value></tool_call>after",
    ] {
        let r = applied(cut);
        assert_eq!(r.tool_calls.len(), 1, "{cut}");
        assert!(r.tool_calls[0].malformed_arguments, "{cut}");
        assert!(r.tool_calls[0].arguments.is_null(), "{cut}");
        assert_eq!(r.tool_calls[0].name, "bash", "{cut}");
    }
}

#[test]
fn glm_form_needs_a_name_followed_by_pairs_or_the_closing_tag() {
    // Prose or a bare number inside the tag is not a GLM call.
    for text in [
        "<tool_call>42</tool_call>",
        "<tool_call>read the file</tool_call>",
    ] {
        let r = applied(text);
        assert!(r.tool_calls[0].malformed_arguments, "{text}");
        assert!(r.tool_calls[0].name.is_empty(), "{text}");
    }
}

#[test]
fn mistral_json_array_and_name_args_forms() {
    let array = applied(
        "Let me.[TOOL_CALLS][{\"name\": \"a\", \"arguments\": {\"x\": 1}}, {\"name\": \"b\", \"arguments\": {}}] ok",
    );
    assert_eq!(array.text, "Let me.\nok");
    assert_eq!(array.finish_reason, "tool_calls");
    let names: Vec<_> = array
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str()))
        .collect();
    assert_eq!(names, [("call_0", "a"), ("call_1", "b")]);
    assert_eq!(array.tool_calls[0].arguments, json!({"x": 1}));

    let named = applied("[TOOL_CALLS]get_weather[ARGS]{\"city\": \"Paris\"}");
    assert_eq!(named.tool_calls[0].name, "get_weather");
    assert_eq!(named.tool_calls[0].arguments, json!({"city": "Paris"}));
    assert_eq!(named.text, "");

    let two = applied("[TOOL_CALLS]a[ARGS]{}[TOOL_CALLS]b[ARGS]{\"n\": 2}");
    assert_eq!(two.tool_calls.len(), 2);
    assert_eq!(two.tool_calls[1].name, "b");
    assert_eq!(two.tool_calls[1].arguments, json!({"n": 2}));
}

#[test]
fn mistral_malformed_is_not_executed() {
    let cut = applied("[TOOL_CALLS][{\"name\": \"rm\", \"arguments\": {\"p\": ");
    assert_eq!(cut.tool_calls.len(), 1);
    assert!(cut.tool_calls[0].malformed_arguments);
    let named = applied("[TOOL_CALLS]bash[ARGS]{\"cmd\": ");
    assert!(named.tool_calls[0].malformed_arguments);
    assert_eq!(named.tool_calls[0].name, "bash");
    let bare = applied("[TOOL_CALLS][ARGS]{}");
    assert!(bare.tool_calls[0].malformed_arguments);
}

#[test]
fn llama_python_tag_call_with_eom_or_eot() {
    let eom = applied(
        "Sure.<|python_tag|>{\"name\": \"search\", \"parameters\": {\"q\": \"rust\"}}<|eom_id|>",
    );
    assert_eq!(eom.text, "Sure.");
    assert_eq!(eom.tool_calls[0].name, "search");
    assert_eq!(eom.tool_calls[0].arguments, json!({"q": "rust"}));
    let eot = applied("<|python_tag|>{\"name\": \"t\", \"arguments\": {}}<|eot_id|>");
    assert_eq!(eot.tool_calls[0].name, "t");
    assert_eq!(eot.text, "");
    let cut = applied("<|python_tag|>{\"name\": \"rm\", \"arguments\": {\"p\":");
    assert!(cut.tool_calls[0].malformed_arguments);
    assert_eq!(cut.tool_calls[0].name, "rm");
}

#[test]
fn harmony_commentary_function_call_in_both_name_positions() {
    let full = applied(
        "<|start|>assistant<|channel|>commentary to=functions.get_weather <|constrain|>json<|message|>{\"city\": \"Paris\"}<|call|>",
    );
    assert_eq!(full.tool_calls.len(), 1);
    assert_eq!(full.tool_calls[0].name, "get_weather");
    assert_eq!(full.tool_calls[0].arguments, json!({"city": "Paris"}));
    assert_eq!(full.text, "");
    let late =
        applied("<|start|>assistant to=functions.ls<|channel|>commentary<|message|>{}<|call|>");
    assert_eq!(late.tool_calls[0].name, "ls");
    let bare = applied(
        "Let me check. to=functions.ls<|channel|>commentary json<|message|>{\"dir\": \".\"}<|call|>\nThanks",
    );
    assert_eq!(bare.tool_calls[0].arguments, json!({"dir": "."}));
    assert_eq!(bare.text, "Let me check.\nThanks");
}

#[test]
fn harmony_malformed_and_non_call_channels() {
    let bad = applied("<|channel|>commentary to=functions.rm<|message|>{\"p\": <|call|>after");
    assert!(bad.tool_calls[0].malformed_arguments);
    assert_eq!(bad.tool_calls[0].name, "rm");
    assert_eq!(bad.text, "after");
    let text = "<|channel|>analysis<|message|>Think.<|end|><|start|>assistant<|channel|>final<|message|>Hi";
    let r = applied(text);
    assert!(r.tool_calls.is_empty());
    assert_eq!(r.text, text);
}

#[test]
fn hider_withholds_new_formats_split_across_deltas() {
    let (out, _) = feed(&["Plan: ", "[TOOL", "_CALLS][{}]"]);
    assert_eq!(out, "Plan: ");
    let (out, _) = feed(&["Go. <|pyth", "on_tag|>{}"]);
    assert_eq!(out, "Go. ");
    let (out, _) = feed(&["Run <func", "tion=ls>"]);
    assert_eq!(out, "Run ");
    let (out, _) = feed(&[
        "<|sta",
        "rt|>assistant<|chan",
        "nel|>commentary to=func",
        "tions.ls<|message|>{}<|call|>",
    ]);
    assert_eq!(out, "");
}

#[test]
fn hider_releases_harmony_headers_that_are_not_calls() {
    let text = "<|start|>assistant<|channel|>final<|message|>Hi there";
    let (out, all) = feed(&[
        "<|sta",
        "rt|>assistant<|channel|>fin",
        "al<|message|>Hi ",
        "there",
    ]);
    assert_eq!(all, text);
    assert_eq!(out, all);
}

const DS_BEGIN: &str = "<｜tool▁calls▁begin｜>";
const DS_END: &str = "<｜tool▁calls▁end｜>";

fn deepseek(name: &str, args: &str) -> String {
    format!(
        "<｜tool▁call▁begin｜>function<｜tool▁sep｜>{name}\n```json\n{args}\n```<｜tool▁call▁end｜>"
    )
}

#[test]
fn deepseek_calls_in_a_section_become_calls_and_the_markers_are_removed() {
    let text = format!(
        "I will look.\n{DS_BEGIN}{}\n{}{DS_END}",
        deepseek("read", "{\"path\": \"a.rs\"}"),
        deepseek("bash", "{\"command\": \"echo \\\"```\\\"\"}"),
    );
    let r = applied(&text);
    assert_eq!(r.text, "I will look.");
    assert_eq!(r.finish_reason, "tool_calls");
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(r.tool_calls[0].name, "read");
    assert_eq!(r.tool_calls[0].arguments, json!({"path": "a.rs"}));
    assert_eq!(r.tool_calls[1].id, "call_1");
    assert_eq!(
        r.tool_calls[1].arguments,
        json!({"command": "echo \"```\""})
    );
}

#[test]
fn deepseek_unreadable_calls_are_malformed_and_never_executed() {
    for text in [
        // Arguments cut off inside the string.
        "<｜tool▁call▁begin｜>function<｜tool▁sep｜>bash\n```json\n{\"command\": \"rm -rf",
        // No fence.
        "<｜tool▁call▁begin｜>function<｜tool▁sep｜>bash\n{\"command\": \"ls\"}<｜tool▁call▁end｜>",
        // Arguments that are not an object.
        "<｜tool▁call▁begin｜>function<｜tool▁sep｜>bash\n```json\n[1]\n```<｜tool▁call▁end｜>",
    ] {
        let r = applied(text);
        assert_eq!(r.tool_calls.len(), 1, "{text}");
        assert!(r.tool_calls[0].malformed_arguments, "{text}");
        assert_eq!(r.tool_calls[0].name, "bash", "{text}");
    }
    let nameless = applied("<｜tool▁call▁begin｜>function<｜tool▁call▁end｜>");
    assert!(nameless.tool_calls[0].malformed_arguments && nameless.tool_calls[0].name.is_empty());
}

#[test]
fn kimi_calls_parse_the_function_name_from_the_id() {
    let r = applied(
        "Checking.<|tool_calls_section_begin|><|tool_call_begin|>functions.read:0<|tool_call_argument_begin|>{\"path\": \"a.rs\"}<|tool_call_end|><|tool_call_begin|>functions.bash:1<|tool_call_argument_begin|>{\"command\": \"ls <|tool_call_end|>\"}<|tool_call_end|><|tool_calls_section_end|>",
    );
    assert_eq!(r.text, "Checking.");
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(r.tool_calls[0].name, "read");
    assert_eq!(r.tool_calls[0].arguments, json!({"path": "a.rs"}));
    assert_eq!(r.tool_calls[1].name, "bash");
    assert_eq!(
        r.tool_calls[1].arguments,
        json!({"command": "ls <|tool_call_end|>"})
    );
}

#[test]
fn kimi_unreadable_calls_are_malformed_and_never_executed() {
    for text in [
        "<|tool_call_begin|>functions.bash:0<|tool_call_argument_begin|>{\"command\": \"rm",
        "<|tool_call_begin|>functions.bash:0{\"command\": \"ls\"}<|tool_call_end|>",
    ] {
        let r = applied(text);
        assert_eq!(r.tool_calls.len(), 1, "{text}");
        assert!(r.tool_calls[0].malformed_arguments, "{text}");
    }
    // A name without the `functions.` prefix or the index is still read.
    let bare = applied("<|tool_call_begin|>ls<|tool_call_argument_begin|>{}<|tool_call_end|>");
    assert_eq!(bare.tool_calls[0].name, "ls");
    assert!(!bare.tool_calls[0].malformed_arguments);
}

#[test]
fn section_markers_alone_are_not_calls() {
    let text = format!("{DS_BEGIN}{DS_END}");
    let r = applied(&text);
    assert!(r.tool_calls.is_empty());
    assert_eq!(r.text, text);
}

#[test]
fn hider_withholds_deepseek_and_kimi_markers_split_across_deltas() {
    let (out, _) = feed(&[
        "Plan. <｜tool▁calls",
        "▁begin｜><｜tool▁call▁be",
        "gin｜>function",
    ]);
    assert_eq!(out, "Plan. ");
    let (out, _) = feed(&[
        "Go. <|tool_calls_sec",
        "tion_begin|><|tool_call_",
        "begin|>functions.ls:0",
    ]);
    assert_eq!(out, "Go. ");
    let (out, all) = feed(&["use the <|tool", " list"]);
    assert_eq!(out, all);
}

fn reasoning_only(reasoning: &str) -> Reply {
    let mut r = reply("");
    r.reasoning = reasoning.into();
    apply_from_reasoning(&mut r);
    r
}

#[test]
fn a_call_that_closes_the_reasoning_is_lifted_when_there_is_no_answer_text() {
    let r = reasoning_only(
        "I should read it.\n<tool_call>\n{\"name\": \"read\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>\n",
    );
    assert_eq!(r.finish_reason, "tool_calls");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].arguments, json!({"path": "a.rs"}));
    assert!(r.reasoning.starts_with("I should read it."));
    let kimi = reasoning_only(
        "<|tool_call_begin|>functions.ls:0<|tool_call_argument_begin|>{}<|tool_call_end|>",
    );
    assert_eq!(kimi.tool_calls[0].name, "ls");
}

#[test]
fn reasoning_calls_are_ignored_unless_they_are_complete_and_last() {
    for reasoning in [
        // Text follows the call.
        "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call> Actually, no.",
        // Cut off, so malformed.
        "<tool_call>{\"name\":\"a\",\"arguments\":{\"p\":\"x",
        // Only fenced JSON, which reasoning uses for examples.
        "```json\n{\"name\":\"a\",\"arguments\":{}}\n```",
        "no call here",
    ] {
        let r = reasoning_only(reasoning);
        assert!(r.tool_calls.is_empty(), "{reasoning}");
        assert_eq!(r.finish_reason, "stop", "{reasoning}");
    }
}

#[test]
fn reasoning_calls_never_override_answer_text_or_structured_calls() {
    let call = "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>";
    let mut with_text = reply("Here is the answer.");
    with_text.reasoning = call.into();
    apply_from_reasoning(&mut with_text);
    assert!(with_text.tool_calls.is_empty());
    let mut structured = reply("");
    structured.reasoning = call.into();
    structured.tool_calls = vec![ToolCall {
        id: "x".into(),
        name: "b".into(),
        arguments: json!({}),
        malformed_arguments: false,
    }];
    apply_from_reasoning(&mut structured);
    assert_eq!(structured.tool_calls.len(), 1);
    assert_eq!(structured.tool_calls[0].name, "b");
}

#[test]
fn pythonic_calls_are_lifted_only_for_offered_tools_and_whole_replies() {
    let offered = vec!["read".to_string()];
    let lift = |text: &str| {
        let mut r = reply(text);
        apply_pythonic(&mut r, &offered);
        r
    };
    let r = lift("[read(path=\"a.rs\")]");
    assert_eq!(r.finish_reason, "tool_calls");
    assert_eq!(r.text, "");
    assert_eq!(r.tool_calls[0].name, "read");
    assert_eq!(r.tool_calls[0].arguments, json!({"path": "a.rs"}));
    for text in [
        "[other(path=\"a\")]",
        "I will [read(path=\"a\")]",
        "[1, 2, 3]",
    ] {
        let r = lift(text);
        assert!(r.tool_calls.is_empty(), "{text}");
        assert_eq!(r.text, text);
    }
    let mut structured = reply("[read(path=\"a\")]");
    structured.tool_calls = vec![ToolCall {
        id: "x".into(),
        name: "read".into(),
        arguments: json!({}),
        malformed_arguments: false,
    }];
    apply_pythonic(&mut structured, &offered);
    assert_eq!(structured.tool_calls[0].id, "x");
    let mut none_offered = reply("[read(path=\"a\")]");
    apply_pythonic(&mut none_offered, &[]);
    assert!(none_offered.tool_calls.is_empty());
}
