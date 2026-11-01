//! Text-embedded tool calls: `<tool_call>` blocks (Hermes JSON, Qwen3-Coder
//! XML, or GLM `<arg_key>`/`<arg_value>` pairs), Mistral `[TOOL_CALLS]`, Llama
//! `<|python_tag|>`, DeepSeek `<｜tool▁call▁begin｜>` and Kimi K2 `<|tool_call_begin|>`
//! special-token calls, gpt-oss harmony `to=functions.NAME` commentary, and strict fenced JSON
//! calls.
//!
//! Servers that do not parse a model's tool-call syntax return it as plain
//! assistant text. [`apply`] lifts those calls into structured [`ToolCall`]s and
//! removes them from the text. A fenced ```` ```json ```` block is recognised too,
//! but only when its object has exactly the keys `name` and `arguments`.
//!
//! Parsing is tolerant of model sloppiness but never invents data: a block that
//! cannot be read becomes a call with `malformed_arguments`, so the engine can
//! tell the model its call was unreadable instead of dropping it silently.

mod pythonic;

use super::{Reply, ToolCall, normalize_arguments};
use serde_json::Value;

pub(super) const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";
const FENCE: &str = "```";

/// Whether `text` still holds the opener of a call form this module reads. [`apply`] lifts every
/// such call, so this is true only for text that never went through it (a route that does not run
/// it) or an opener it could not lift. Section markers alone are not calls.
pub fn mentions_call_markup(text: &str) -> bool {
    HIDE_OPENERS
        .iter()
        .filter(|marker| !SECTION_MARKERS.contains(marker))
        .any(|marker| text.contains(marker))
}

/// When `reply` has no structured tool calls but its text contains textual
/// ones, move them into `reply.tool_calls`, strip them from `reply.text`, and
/// set `finish_reason` to `tool_calls`.
///
/// Only calls to `offered` tools are lifted: a reply that quotes call markup naming anything else
/// (an example from a web page or README) stays text, and nothing is lifted from it.
pub(super) fn apply(reply: &mut Reply, offered: &[String]) {
    if !reply.tool_calls.is_empty() {
        return;
    }
    let Some((text, calls)) = extract(&reply.text) else {
        return;
    };
    if calls.iter().any(|call| !offered.contains(&call.name)) {
        return;
    }
    reply.text = text;
    reply.tool_calls = calls;
    reply.finish_reason = "tool_calls".to_string();
}

/// Pythonic calls (`[read(path="a.rs")]`, see [`pythonic`]) when `reply` has no calls and its whole
/// text is a list of calls to `offered` tools. These have no marker to recognise, so unlike
/// [`apply`] this needs the offered names and callers run it only for local endpoints; the streamed
/// text callback has already shown such a reply. Not checked against a live model.
pub(super) fn apply_pythonic(reply: &mut Reply, offered: &[String]) {
    if !reply.tool_calls.is_empty() || offered.is_empty() {
        return;
    }
    let Some(calls) = pythonic::parse(&reply.text, offered) else {
        return;
    };
    reply.tool_calls = calls
        .into_iter()
        .enumerate()
        .map(|(index, (name, arguments))| ToolCall {
            id: call_id(index),
            name,
            arguments,
            malformed_arguments: false,
        })
        .collect();
    reply.text.clear();
    reply.finish_reason = "tool_calls".to_string();
}

/// A local model that ends its reasoning with a call and writes no answer text (the call sits inside
/// an unclosed `<think>` block, or in the server's separate reasoning field) would otherwise end the
/// turn empty. When `reply` has no calls and no text, the calls that close `reply.reasoning` are
/// lifted. Only well-formed call markup counts, and only when nothing follows the last call:
/// reasoning that merely discusses a call, or fenced JSON examples, never runs. The reasoning is
/// left as it was.
pub(super) fn apply_from_reasoning(reply: &mut Reply) {
    if !reply.tool_calls.is_empty() || !reply.text.trim().is_empty() {
        return;
    }
    let (segments, calls) = extract_markup(&reply.reasoning);
    let closes = segments.last().is_some_and(|tail| tail.trim().is_empty());
    if calls.is_empty() || !closes || calls.iter().any(|call| call.malformed_arguments) {
        return;
    }
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

/// DeepSeek V3/R1: `<｜tool▁call▁begin｜>function<｜tool▁sep｜>NAME` then the arguments in a
/// ```` ```json ```` fence, ended by `<｜tool▁call▁end｜>`.
const DEEPSEEK_CALL: &str = "<｜tool▁call▁begin｜>";
const DEEPSEEK_SEP: &str = "<｜tool▁sep｜>";
const DEEPSEEK_CALL_END: &str = "<｜tool▁call▁end｜>";
/// Kimi K2: `<|tool_call_begin|>functions.NAME:IDX<|tool_call_argument_begin|>{json}<|tool_call_end|>`.
const KIMI_CALL: &str = "<|tool_call_begin|>";
const KIMI_ARGS: &str = "<|tool_call_argument_begin|>";
const KIMI_CALL_END: &str = "<|tool_call_end|>";
/// Section markers around one or more DeepSeek or Kimi calls. They carry no data and are removed
/// with the calls they wrap.
const SECTION_MARKERS: [&str; 4] = [
    "<｜tool▁calls▁begin｜>",
    "<｜tool▁calls▁end｜>",
    "<|tool_calls_section_begin|>",
    "<|tool_calls_section_end|>",
];

/// Markers that open a call (or, for the section markers, bracket calls), tried in order at each
/// position. `Hider` withholds everything from the first of these, so streamed text never shows them.
const HIDE_OPENERS: [&str; 10] = [
    OPEN,
    "<function=",
    "[TOOL_CALLS]",
    "<|python_tag|>",
    DEEPSEEK_CALL,
    KIMI_CALL,
    SECTION_MARKERS[0],
    SECTION_MARKERS[1],
    SECTION_MARKERS[2],
    SECTION_MARKERS[3],
];
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
    if let Some(after) = s.strip_prefix(DEEPSEEK_CALL) {
        let (call, consumed) = parse_deepseek(after, index);
        return Some((vec![call], DEEPSEEK_CALL.len() + consumed));
    }
    if let Some(after) = s.strip_prefix(KIMI_CALL) {
        let (call, consumed) = parse_kimi(after, index);
        return Some((vec![call], KIMI_CALL.len() + consumed));
    }
    if let Some(marker) = SECTION_MARKERS.iter().find(|m| s.starts_with(*m)) {
        return Some((Vec::new(), marker.len()));
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
fn parse_block(after: &str, index: usize) -> (ToolCall, usize) {
    let lead = after.len() - after.trim_start().len();
    if after[lead..].starts_with("<function=") {
        let (call, consumed) = parse_qwen(&after[lead..], index);
        return (
            call,
            lead + consumed + skip_close(&after[lead + consumed..]),
        );
    }
    if let Some((call, consumed)) = parse_glm(&after[lead..], index) {
        return (call, lead + consumed);
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
    // A fenced JSON object is only a call when it is the whole reply. Prose around a fence is an
    // explanation or a quotation (of a document, page or tool output), never an instruction to run.
    if segments.iter().any(|segment| !segment.trim().is_empty()) {
        return (vec![text], Vec::new());
    }
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

const ARG_KEY: &str = "<arg_key>";
const ARG_KEY_END: &str = "</arg_key>";
const ARG_VALUE: &str = "<arg_value>";
const ARG_VALUE_END: &str = "</arg_value>";

/// GLM-4.5 and later: `NAME` followed by `<arg_key>K</arg_key><arg_value>V</arg_value>` pairs,
/// ended by `</tool_call>` (or by the end of the text once every pair is closed). `body` follows
/// the opening tag and its whitespace. `None` when the block does not have this shape, so JSON
/// blocks are left to their own parser; otherwise the call and the bytes of `body` it occupies.
fn parse_glm(body: &str, index: usize) -> Option<(ToolCall, usize)> {
    let name_len = body
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(body.len());
    let name = &body[..name_len];
    if !name.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return None;
    }
    let mut rest = body[name_len..].trim_start();
    if !(rest.starts_with(ARG_KEY) || rest.starts_with(CLOSE)) {
        return None;
    }
    let mut arguments = serde_json::Map::new();
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() || trimmed.starts_with(CLOSE) {
            let closed = if trimmed.is_empty() { 0 } else { CLOSE.len() };
            let call = ToolCall {
                id: call_id(index),
                name: name.to_string(),
                arguments: Value::Object(arguments),
                malformed_arguments: false,
            };
            return Some((call, body.len() - trimmed.len() + closed));
        }
        let pair = trimmed
            .strip_prefix(ARG_KEY)
            .and_then(|t| t.split_once(ARG_KEY_END))
            .and_then(|(key, t)| Some((key.trim(), t.trim_start().strip_prefix(ARG_VALUE)?)))
            .and_then(|(key, t)| {
                let (value, next) = t.split_once(ARG_VALUE_END)?;
                Some((key, value, next))
            })
            .filter(|(key, ..)| !key.is_empty());
        let Some((key, value, next)) = pair else {
            let (call, consumed) = malformed_until(body, &[CLOSE], index, Some(name.to_string()));
            return Some((call, consumed));
        };
        arguments.insert(key.to_string(), qwen_value(value));
        rest = next;
    }
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

/// Qwen and GLM parameter values are raw text with one leading and one trailing newline
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

/// Arguments from the JSON value that starts `body` (after whitespace). Returns the arguments, the
/// malformed flag and the bytes consumed through the value, or `None` when no JSON value starts there.
fn json_arguments(body: &str) -> Option<(Value, bool, usize)> {
    let lead = body.len() - body.trim_start().len();
    let mut values = serde_json::Deserializer::from_str(&body[lead..]).into_iter::<Value>();
    let value = values.next()?.ok()?;
    let (arguments, malformed_arguments) = normalize_arguments(Some(&value));
    Some((arguments, malformed_arguments, lead + values.byte_offset()))
}

/// Bytes of `tail` taken by optional whitespace and then `token`, or 0 when `token` does not follow.
fn skip_token(tail: &str, token: &str) -> usize {
    let padding = tail.len() - tail.trim_start().len();
    if tail[padding..].starts_with(token) {
        padding + token.len()
    } else {
        0
    }
}

/// DeepSeek: `TYPE<｜tool▁sep｜>NAME`, a newline, then the arguments in a ```` ```json ```` fence.
/// `after` follows `<｜tool▁call▁begin｜>`; the result runs through `<｜tool▁call▁end｜>` when present.
fn parse_deepseek(after: &str, index: usize) -> (ToolCall, usize) {
    let ends = [DEEPSEEK_CALL_END];
    let Some((_, rest)) = after.split_once(DEEPSEEK_SEP) else {
        return malformed_until(after, &ends, index, Some(String::new()));
    };
    let head = after.len() - rest.len();
    let (name, body) = rest.split_once('\n').unwrap_or((rest, ""));
    let name = name.trim();
    if name.is_empty() || name.contains('<') {
        return malformed_until(after, &ends, index, Some(String::new()));
    }
    let body_at = head + (rest.len() - body.len());
    let fence = body.trim_start();
    let Some(fenced) = fence.strip_prefix(FENCE) else {
        return malformed_until(after, &ends, index, Some(name.to_string()));
    };
    let fenced = fenced.strip_prefix("json").unwrap_or(fenced);
    let json_at = body_at + (body.len() - fenced.len());
    let Some((arguments, malformed_arguments, used)) = json_arguments(&after[json_at..]) else {
        return malformed_until(after, &ends, index, Some(name.to_string()));
    };
    let mut end = json_at + used;
    end += skip_token(&after[end..], FENCE);
    end += skip_token(&after[end..], DEEPSEEK_CALL_END);
    let call = ToolCall {
        id: call_id(index),
        name: name.to_string(),
        arguments,
        malformed_arguments,
    };
    (call, end)
}

/// Kimi K2: the id `functions.NAME:IDX`, `<|tool_call_argument_begin|>`, then a JSON object.
/// `after` follows `<|tool_call_begin|>`; the result runs through `<|tool_call_end|>` when present.
fn parse_kimi(after: &str, index: usize) -> (ToolCall, usize) {
    let ends = [KIMI_CALL_END];
    let Some((id, rest)) = after.split_once(KIMI_ARGS) else {
        return malformed_until(after, &ends, index, Some(String::new()));
    };
    let id = id.trim();
    let id = id.strip_prefix("functions.").unwrap_or(id);
    let name = match id.rsplit_once(':') {
        Some((head, idx)) if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => id,
    };
    if name.is_empty() || name.contains('<') {
        return malformed_until(after, &ends, index, Some(String::new()));
    }
    let args_at = after.len() - rest.len();
    let Some((arguments, malformed_arguments, used)) = json_arguments(rest) else {
        return malformed_until(after, &ends, index, Some(name.to_string()));
    };
    let end = args_at + used;
    let call = ToolCall {
        id: call_id(index),
        name: name.to_string(),
        arguments,
        malformed_arguments,
    };
    (call, end + skip_token(&after[end..], KIMI_CALL_END))
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
    hidden: &'a mut dyn FnMut(&str) -> bool,
    held: String,
    hiding: bool,
    /// False passes every delta through untouched.
    hide_tags: bool,
}

impl<'a> Hider<'a> {
    pub(super) fn new(
        out: &'a mut dyn FnMut(&str) -> bool,
        hidden: &'a mut dyn FnMut(&str) -> bool,
        hide_tags: bool,
    ) -> Self {
        Hider {
            out,
            hidden,
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
            return delta.is_empty() || (self.hidden)(delta);
        }
        self.held.push_str(delta);
        if let Some((at, hide)) = self.decisive() {
            let keep_going = (at == 0 || (self.out)(&self.held[..at]))
                && (!hide || (self.hidden)(&self.held[at..]));
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
mod tests;
