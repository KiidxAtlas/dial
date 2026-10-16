//! Text-embedded tool calls: `<tool_call>` blocks (Hermes JSON or Qwen3-Coder
//! XML), Mistral `[TOOL_CALLS]`, Llama `<|python_tag|>`, gpt-oss harmony
//! `to=functions.NAME` commentary, and strict fenced JSON calls.
//!
//! Servers that do not parse a model's tool-call syntax return it as plain
//! assistant text. [`apply`] lifts those calls into structured [`ToolCall`]s and
//! removes them from the text. A fenced ```` ```json ```` block is recognised too,
//! but only when its object has exactly the keys `name` and `arguments`.
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
    let (segments, calls) = extract_markup(text);
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

/// Markers that open a call, tried in order at each position.
const HIDE_OPENERS: [&str; 4] = [OPEN, "<function=", "[TOOL_CALLS]", "<|python_tag|>"];
/// gpt-oss harmony markers. A header counts only when it names `to=functions.NAME`.
const HARMONY_OPENERS: [&str; 3] = ["<|start|>", "<|channel|>", TO_FUNCTION];
const TO_FUNCTION: &str = "to=functions.";
const CALL_END: &str = "<|call|>";
const MESSAGE: &str = "<|message|>";
/// Longest harmony header (up to `<|message|>`) that can still be a call.
const HEADER_MAX: usize = 256;

/// Scans for every supported call form in order. A marker that does not open a
/// readable call stays in the text, and scanning resumes just after it.
fn extract_markup(text: &str) -> (Vec<&str>, Vec<ToolCall>) {
    let mut segments = Vec::new();
    let mut calls = Vec::new();
    let mut kept_from = 0;
    let mut cursor = 0;
    while let Some(at) = next_opener(text, cursor) {
        match parse_at(&text[at..], calls.len()) {
            Some((parsed, consumed)) => {
                segments.push(&text[kept_from..at]);
                kept_from = at + consumed;
                cursor = kept_from;
                calls.extend(parsed);
            }
            None => cursor = at + 1,
        }
    }
    segments.push(&text[kept_from..]);
    (segments, calls)
}

fn next_opener(text: &str, from: usize) -> Option<usize> {
    HIDE_OPENERS
        .iter()
        .chain(HARMONY_OPENERS.iter())
        .filter_map(|marker| text[from..].find(marker))
        .min()
        .map(|rel| from + rel)
}

/// Parses the call form that starts at `s`, which begins with an opener. Returns
/// the calls and the bytes of `s` they occupy, or `None` when it is not a call.
fn parse_at(s: &str, index: usize) -> Option<(Vec<ToolCall>, usize)> {
    if let Some(after) = s.strip_prefix(OPEN) {
        let (call, consumed) = parse_block(after, index);
        return Some((vec![call], OPEN.len() + consumed));
    }
    if s.starts_with("<function=") {
        let (call, consumed) = parse_qwen(s, index);
        return Some((vec![call], consumed));
    }
    if let Some(after) = s.strip_prefix("[TOOL_CALLS]") {
        let (calls, consumed) = parse_mistral(after, index);
        return Some((calls, "[TOOL_CALLS]".len() + consumed));
    }
    if let Some(after) = s.strip_prefix("<|python_tag|>") {
        let (call, consumed) = parse_llama(after, index);
        return Some((vec![call], "<|python_tag|>".len() + consumed));
    }
    let (call, consumed) = parse_harmony(s, index)?;
    Some((vec![call], consumed))
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
/// Parse the text following an opening tag. Returns the call and how many bytes
/// of `after` the block occupied (through the closing tag when present).
fn parse_block(after: &str, index: usize) -> (ToolCall, usize) {
    let lead = after.len() - after.trim_start().len();
    if after[lead..].starts_with("<function=") {
        let (call, consumed) = parse_qwen(&after[lead..], index);
        return (
            call,
            lead + consumed + skip_close(&after[lead + consumed..]),
        );
    }
    // A JSON parser, not brace counting or a search for the closing tag: both
    // would be fooled by braces or `</tool_call>` inside argument strings.
    let mut values = serde_json::Deserializer::from_str(&after[lead..]).into_iter::<Value>();
    if let Some(Ok(value)) = values.next() {
        let end = lead + values.byte_offset();
        return (
            call_from_value(&value, index),
            end + skip_close(&after[end..]),
        );
    }
    malformed_until(after, &[CLOSE], index, None)
}

/// Bytes of `tail` taken by a closing tag after optional whitespace, or 0.
fn skip_close(tail: &str) -> usize {
    let padding = tail.len() - tail.trim_start().len();
    if tail[padding..].starts_with(CLOSE) {
        padding + CLOSE.len()
    } else {
        0
    }
}

/// A block that cannot be read becomes a malformed call (never executed). It
/// consumes through the first of `ends`, or the rest of the text when none is
/// present. The name is `name` when given, otherwise a best-effort guess.
fn malformed_until(
    body: &str,
    ends: &[&str],
    index: usize,
    name: Option<String>,
) -> (ToolCall, usize) {
    let consumed = ends
        .iter()
        .filter_map(|end| body.find(end).map(|at| at + end.len()))
        .min()
        .unwrap_or(body.len());
    let name = name.unwrap_or_else(|| guess_name(&body[..consumed]));
    (malformed(index, name), consumed)
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

/// Qwen3-Coder XML: `<function=NAME>` with `<parameter=KEY>VALUE</parameter>`
/// children. `s` starts at `<function=`; the result runs through `</function>`.
fn parse_qwen(s: &str, index: usize) -> (ToolCall, usize) {
    const FUNCTION_END: &str = "</function>";
    const PARAMETER_END: &str = "</parameter>";
    let head = &s["<function=".len()..];
    let Some(gt) = head.find('>') else {
        return malformed_until(s, &[CLOSE], index, Some(String::new()));
    };
    let name = head[..gt].trim().to_string();
    if name.is_empty() || name.contains('<') {
        return malformed_until(s, &[CLOSE], index, Some(String::new()));
    }
    let mut arguments = serde_json::Map::new();
    let mut rest = &head[gt + 1..];
    loop {
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix(FUNCTION_END) {
            let call = ToolCall {
                id: call_id(index),
                name,
                arguments: Value::Object(arguments),
                malformed_arguments: false,
            };
            return (call, s.len() - after.len());
        }
        let Some((key, body)) = trimmed
            .strip_prefix("<parameter=")
            .and_then(|p| p.split_once('>'))
        else {
            return malformed_until(s, &[CLOSE], index, Some(name));
        };
        let Some((value, next)) = body.split_once(PARAMETER_END) else {
            return malformed_until(s, &[CLOSE], index, Some(name));
        };
        let key = key.trim();
        if key.is_empty() {
            return malformed_until(s, &[CLOSE], index, Some(name));
        }
        arguments.insert(key.to_string(), qwen_value(value));
        rest = next;
    }
}

/// Qwen parameter values are raw text with one leading and one trailing newline
/// removed. Only a value that is clearly a JSON object or array is parsed; numbers,
/// booleans and everything else stay strings, because the tool schemas are not
/// available to this module to coerce them by type.
fn qwen_value(raw: &str) -> Value {
    let text = raw
        .strip_prefix("\r\n")
        .or_else(|| raw.strip_prefix('\n'))
        .unwrap_or(raw);
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let trimmed = text.trim();
    let clearly_json = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if clearly_json && let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value;
    }
    Value::String(text.to_string())
}

/// Llama 3.x: `<|python_tag|>` then a JSON call, optionally ended by
/// `<|eom_id|>` or `<|eot_id|>`. `after` follows the `<|python_tag|>` marker.
fn parse_llama(after: &str, index: usize) -> (ToolCall, usize) {
    const ENDS: [&str; 2] = ["<|eom_id|>", "<|eot_id|>"];
    let lead = after.len() - after.trim_start().len();
    let mut values = serde_json::Deserializer::from_str(&after[lead..]).into_iter::<Value>();
    if let Some(Ok(value)) = values.next() {
        let end = lead + values.byte_offset();
        let tail = &after[end..];
        let padding = tail.len() - tail.trim_start().len();
        let consumed = ENDS
            .iter()
            .find(|tag| tail[padding..].starts_with(*tag))
            .map_or(end, |tag| end + padding + tag.len());
        return (call_from_value(&value, index), consumed);
    }
    malformed_until(after, &ENDS, index, None)
}

/// Mistral `[TOOL_CALLS]`: either a JSON array of calls, or `NAME[ARGS]{...}`.
/// `after` follows the `[TOOL_CALLS]` marker.
fn parse_mistral(after: &str, index: usize) -> (Vec<ToolCall>, usize) {
    const ARGS: &str = "[ARGS]";
    let lead = after.len() - after.trim_start().len();
    let s = &after[lead..];
    if s.starts_with('[') {
        let mut values = serde_json::Deserializer::from_str(s).into_iter::<Value>();
        if let Some(Ok(Value::Array(items))) = values.next() {
            let calls = items
                .iter()
                .enumerate()
                .map(|(i, item)| call_from_value(item, index + i))
                .collect();
            return (calls, lead + values.byte_offset());
        }
        let (call, consumed) = malformed_until(after, &[], index, Some(String::new()));
        return (vec![call], consumed);
    }
    let name_len = s
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(s.len());
    let name = &s[..name_len];
    if !name.is_empty() && s[name_len..].starts_with(ARGS) {
        let args_at = lead + name_len + ARGS.len();
        let mut values = serde_json::Deserializer::from_str(&after[args_at..]).into_iter::<Value>();
        if let Some(Ok(value)) = values.next() {
            let (arguments, malformed_arguments) = normalize_arguments(Some(&value));
            let call = ToolCall {
                id: call_id(index),
                name: name.to_string(),
                arguments,
                malformed_arguments,
            };
            return (vec![call], args_at + values.byte_offset());
        }
        let (call, consumed) =
            malformed_until(&after[args_at..], &[], index, Some(name.to_string()));
        return (vec![call], args_at + consumed);
    }
    let (call, consumed) = malformed_until(after, &[], index, Some(name.to_string()));
    (vec![call], consumed)
}

/// Longest part of a harmony header that is scanned for `<|message|>`.
fn harmony_window(s: &str) -> &str {
    let mut end = s.len().min(HEADER_MAX);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A harmony header that names a function: `s` starts at a harmony marker, and
/// the header runs to the first `<|message|>` in the window. Returns the name and
/// the offset just past `<|message|>`.
fn harmony_call_header(s: &str) -> Option<(&str, usize)> {
    let window = harmony_window(s);
    let message = window.find(MESSAGE)?;
    let header = &window[..message];
    let start = header.find(TO_FUNCTION)? + TO_FUNCTION.len();
    let name = header[start..]
        .split(|c: char| c.is_whitespace() || c == '<')
        .next()
        .unwrap_or_default();
    (!name.is_empty()).then_some((name, message + MESSAGE.len()))
}

/// gpt-oss harmony leak: `... to=functions.NAME ... <|message|>{json}<|call|>`.
fn parse_harmony(s: &str, index: usize) -> Option<(ToolCall, usize)> {
    let (name, body_at) = harmony_call_header(s)?;
    let body = &s[body_at..];
    let lead = body.len() - body.trim_start().len();
    let mut values = serde_json::Deserializer::from_str(&body[lead..]).into_iter::<Value>();
    if let Some(Ok(value)) = values.next() {
        let end = body_at + lead + values.byte_offset();
        let tail = &s[end..];
        let padding = tail.len() - tail.trim_start().len();
        let consumed = if tail[padding..].starts_with(CALL_END) {
            end + padding + CALL_END.len()
        } else {
            end
        };
        let (arguments, malformed_arguments) = normalize_arguments(Some(&value));
        let call = ToolCall {
            id: call_id(index),
            name: name.to_string(),
            arguments,
            malformed_arguments,
        };
        return Some((call, consumed));
    }
    let (call, consumed) =
        malformed_until(&s[body_at..], &[CALL_END], index, Some(name.to_string()));
    Some((call, body_at + consumed))
}

/// Forwards streamed text but withholds anything from a call opener onward, so
/// a UI never shows the raw call syntax that [`apply`] will remove. A harmony
/// header (`<|start|>`, `<|channel|>`, `to=functions.`) is held back until its
/// `<|message|>` shows whether it names a function.
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
        if let Some((at, hide)) = self.decisive() {
            let keep_going = at == 0 || (self.out)(&self.held[..at]);
            if hide {
                self.held.clear();
                self.hiding = true;
            } else {
                self.held.drain(..at);
            }
            return keep_going;
        }
        // Keep back a tail that might be the start of a marker split across deltas.
        let markers = || HIDE_OPENERS.iter().chain(HARMONY_OPENERS.iter());
        let longest = markers().map(|m| m.len()).max().unwrap_or(0);
        let keep = (1..=self.held.len().min(longest))
            .rev()
            .find(|&n| {
                let start = self.held.len() - n;
                self.held.is_char_boundary(start)
                    && markers().any(|m| m.len() > n && m.starts_with(&self.held[start..]))
            })
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

    /// The earliest point in `held` that must not be emitted, paired with
    /// whether everything from there on is call markup (`true`) or a harmony
    /// header still waiting for its `<|message|>` (`false`).
    fn decisive(&self) -> Option<(usize, bool)> {
        let held = self.held.as_str();
        let hide = HIDE_OPENERS
            .iter()
            .filter_map(|m| held.find(m))
            .min()
            .map(|at| (at, true));
        let header = HARMONY_OPENERS
            .iter()
            .flat_map(|m| held.match_indices(m).map(|(at, _)| at))
            .filter_map(|at| {
                let rest = &held[at..];
                if harmony_call_header(rest).is_some() {
                    Some((at, true))
                } else if rest.len() < HEADER_MAX && !rest.contains(MESSAGE) {
                    Some((at, false))
                } else {
                    None
                }
            })
            .min();
        hide.into_iter().chain(header).min()
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
        assert!(
            nameless.tool_calls[0].malformed_arguments && nameless.tool_calls[0].name.is_empty()
        );
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
}
