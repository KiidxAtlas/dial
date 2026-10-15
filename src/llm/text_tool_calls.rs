//! Text-embedded tool calls (`<tool_call>` blocks and strict fenced JSON calls).
//!
//! Servers that do not parse a model's tool-call syntax return it as plain
//! assistant text: `<tool_call>{"name": "...", "arguments": {...}}</tool_call>`.
//! [`apply`] lifts those blocks into structured [`ToolCall`]s and removes them
//! from the text. A second, deliberately narrow form is recognised too: a
//! fenced ```` ```json ```` block whose object has exactly the keys `name` and
//! `arguments`.
//!
//! Parsing is tolerant of model sloppiness but never invents data: a block that
//! cannot be read becomes a call with `malformed_arguments`, so the engine can
//! tell the model its call was unreadable instead of dropping it silently.

use super::{Reply, ToolCall, normalize_arguments};
use serde_json::Value;

pub(super) const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";
const FENCE: &str = "```";

/// When `reply` has no structured tool calls but its text contains textual
/// ones, move them into `reply.tool_calls`, strip them from `reply.text`, and
/// set `finish_reason` to `tool_calls`.
pub(super) fn apply(reply: &mut Reply) {
    if !reply.tool_calls.is_empty() {
        return;
    }
    let Some((text, calls)) = extract(&reply.text) else {
        return;
    };
    reply.text = text;
    reply.tool_calls = calls;
    reply.finish_reason = "tool_calls".to_string();
}

/// The remaining text and the calls found, or `None` when there are none.
pub(super) fn extract(text: &str) -> Option<(String, Vec<ToolCall>)> {
    let (segments, calls) = extract_tagged(text);
    let (segments, calls) = if calls.is_empty() {
        extract_fenced(text)
    } else {
        (segments, calls)
    };
    if calls.is_empty() {
        return None;
    }
    let kept: Vec<&str> = segments
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    Some((kept.join("\n"), calls))
}

fn extract_tagged(text: &str) -> (Vec<&str>, Vec<ToolCall>) {
    let mut segments = Vec::new();
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        segments.push(&rest[..at]);
        let after = &rest[at + OPEN.len()..];
        let (call, consumed) = parse_block(after, calls.len());
        calls.push(call);
        rest = &after[consumed..];
    }
    segments.push(rest);
    (segments, calls)
}

fn call_id(index: usize) -> String {
    format!("call_{index}")
}

fn malformed(index: usize, name: String) -> ToolCall {
    ToolCall {
        id: call_id(index),
        name,
        arguments: Value::Null,
        malformed_arguments: true,
    }
}

/// Parse the text following an opening tag. Returns the call and how many bytes
/// of `after` the block occupied (through the closing tag when present).
fn parse_block(after: &str, index: usize) -> (ToolCall, usize) {
    let lead = after.len() - after.trim_start().len();
    // A JSON parser, not brace counting or a search for the closing tag: both
    // would be fooled by braces or `</tool_call>` inside argument strings.
    let mut values = serde_json::Deserializer::from_str(&after[lead..]).into_iter::<Value>();
    if let Some(Ok(value)) = values.next() {
        let end = lead + values.byte_offset();
        let tail = &after[end..];
        let padding = tail.len() - tail.trim_start().len();
        let consumed = if tail[padding..].starts_with(CLOSE) {
            end + padding + CLOSE.len()
        } else {
            end
        };
        return (call_from_value(&value, index), consumed);
    }
    let (body, consumed) = match after.find(CLOSE) {
        Some(at) => (&after[..at], at + CLOSE.len()),
        None => (after, after.len()),
    };
    (malformed(index, guess_name(body)), consumed)
}

/// Best-effort function name from a block that is not valid JSON, so the
/// model's error message can say which call was unreadable.
fn guess_name(body: &str) -> String {
    let Some(at) = body.find("\"name\"") else {
        return String::new();
    };
    let rest = body[at + "\"name\"".len()..].trim_start();
    let Some(rest) = rest.strip_prefix(':') else {
        return String::new();
    };
    serde_json::Deserializer::from_str(rest.trim_start())
        .into_iter::<String>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn call_from_value(value: &Value, index: usize) -> ToolCall {
    let Some(object) = value.as_object() else {
        return malformed(index, String::new());
    };
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        return malformed(index, name);
    }
    // Llama-style templates say `parameters`.
    let (arguments, malformed_arguments) =
        normalize_arguments(object.get("arguments").or_else(|| object.get("parameters")));
    ToolCall {
        id: call_id(index),
        name,
        arguments,
        malformed_arguments,
    }
}

/// Fenced blocks whose JSON object has exactly `name` and `arguments`.
/// Other fences (code samples) are left in the text untouched.
fn extract_fenced(text: &str) -> (Vec<&str>, Vec<ToolCall>) {
    let mut segments = Vec::new();
    let mut calls = Vec::new();
    let mut kept_from = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(FENCE) {
        let open = cursor + found;
        let after_open = open + FENCE.len();
        let Some(line_end) = text[after_open..].find('\n') else {
            break;
        };
        let info = text[after_open..after_open + line_end].trim();
        let body_start = after_open + line_end + 1;
        let Some(close_rel) = text[body_start..].find(FENCE) else {
            break;
        };
        let body = &text[body_start..body_start + close_rel];
        let block_end = body_start + close_rel + FENCE.len();
        cursor = block_end;
        if !(info.is_empty() || info.eq_ignore_ascii_case("json")) {
            continue;
        }
        if let Some(call) = strict_fenced_call(body, calls.len()) {
            segments.push(&text[kept_from..open]);
            kept_from = block_end;
            calls.push(call);
        }
    }
    segments.push(&text[kept_from..]);
    (segments, calls)
}

fn strict_fenced_call(body: &str, index: usize) -> Option<ToolCall> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    let object = value.as_object()?;
    if object.len() != 2 || !object.contains_key("name") || !object.contains_key("arguments") {
        return None;
    }
    object
        .get("name")?
        .as_str()
        .filter(|name| !name.is_empty())?;
    Some(call_from_value(&value, index))
}

/// Forwards streamed text but withholds anything from `<tool_call>` onward, so
/// a UI never shows the raw call syntax that [`apply`] will remove.
pub(super) struct Hider<'a> {
    out: &'a mut dyn FnMut(&str) -> bool,
    held: String,
    hiding: bool,
    /// False passes every delta through untouched.
    hide_tags: bool,
}

impl<'a> Hider<'a> {
    pub(super) fn new(out: &'a mut dyn FnMut(&str) -> bool, hide_tags: bool) -> Self {
        Hider {
            out,
            held: String::new(),
            hiding: false,
            hide_tags,
        }
    }

    /// False when the consumer asked to stop.
    pub(super) fn push(&mut self, delta: &str) -> bool {
        if !self.hide_tags {
            return (self.out)(delta);
        }
        if self.hiding {
            return true;
        }
        self.held.push_str(delta);
        if let Some(at) = self.held.find(OPEN) {
            let keep_going = at == 0 || (self.out)(&self.held[..at]);
            self.held.clear();
            self.hiding = true;
            return keep_going;
        }
        // Keep back a tail that might be the start of a tag split across deltas.
        let keep = (1..OPEN.len())
            .rev()
            .find(|&n| self.held.ends_with(&OPEN[..n]))
            .unwrap_or(0);
        let emit = self.held.len() - keep;
        if emit == 0 {
            return true;
        }
        let keep_going = (self.out)(&self.held[..emit]);
        self.held.drain(..emit);
        keep_going
    }

    /// Flush a held tail that turned out not to be a tag.
    pub(super) fn finish(&mut self) -> bool {
        if self.hiding || self.held.is_empty() {
            return true;
        }
        let keep_going = (self.out)(&self.held);
        self.held.clear();
        keep_going
    }
}

#[cfg(test)]
mod tests {
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
        let r =
            applied("<tool_call>{\"name\": \"bash\", \"arguments\": {\"cmd\": </tool_call>after");
        assert_eq!(r.tool_calls.len(), 1);
        assert!(r.tool_calls[0].malformed_arguments);
        assert_eq!(r.tool_calls[0].name, "bash");
        assert!(r.tool_calls[0].arguments.is_null());
        assert_eq!(r.text, "after");
        assert_eq!(r.finish_reason, "tool_calls");

        let nameless = applied("<tool_call>{\"arguments\": {}}</tool_call>");
        assert!(
            nameless.tool_calls[0].malformed_arguments && nameless.tool_calls[0].name.is_empty()
        );
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
        let r =
            applied("Sure:\n```json\n{\"name\": \"ls\", \"arguments\": {\"dir\": \".\"}}\n```\nok");
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
        let mut hider = Hider::new(&mut sink, true);
        let mut all = String::new();
        for d in deltas {
            all.push_str(d);
            hider.push(d);
        }
        hider.finish();
        drop(hider);
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
    fn hider_stops_when_consumer_declines_and_can_be_disabled() {
        let mut calls = 0;
        let mut sink = |_: &str| {
            calls += 1;
            false
        };
        let mut hider = Hider::new(&mut sink, true);
        assert!(!hider.push("hello"));
        drop(hider);
        assert_eq!(calls, 1);

        let mut out = String::new();
        let mut sink = |s: &str| {
            out.push_str(s);
            true
        };
        let mut hider = Hider::new(&mut sink, false);
        assert!(hider.push("<tool_call>x"));
        drop(hider);
        assert_eq!(out, "<tool_call>x");
    }

    #[test]
    fn hider_releases_lookalike_prefixes() {
        let (out, all) = feed(&["a <tool", " is a thing <", "b> and <tool_c"]);
        assert_eq!(out, all, "text that is not a tag must be delivered in full");
    }
}
