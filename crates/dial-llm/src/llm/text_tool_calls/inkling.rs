//! Inkling's addressed tool message, as distinct from an answer or thinking frame.

use super::{ToolCall, call_id};
use serde::Deserialize;
use serde_json::{Map, Value};

pub(in crate::llm) enum Header<'a> {
    Incomplete,
    Other,
    Call {
        name: &'a str,
        body: &'a str,
        end: &'static str,
    },
}

/// Probe only at the start of a message. Bound the undecided part of a streamed header.
pub(in crate::llm) fn header(text: &str) -> Header<'_> {
    for (start, invoke, end) in [
        (
            "<|message_model|>",
            "<|content_invoke_tool_json|>",
            "<|end_message|>",
        ),
        (
            "◁message_model▷",
            "◁content_invoke_tool_json▷",
            "◁end_message▷",
        ),
    ] {
        let Some(rest) = text.strip_prefix(start) else {
            if start.starts_with(text) {
                return Header::Incomplete;
            }
            continue;
        };
        let n = rest
            .bytes()
            .take(257)
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            .count();
        if n > 256 || (n == 0 && !rest.is_empty()) {
            return Header::Other;
        }
        let (name, tail) = rest.split_at(n);
        if let Some(body) = tail.strip_prefix(invoke) {
            return Header::Call { name, body, end };
        }
        if invoke.starts_with(tail) {
            return Header::Incomplete;
        }
    }
    Header::Other
}

/// Require one complete frame with matching recipients and object arguments. Do not infer a call
/// from prose, a truncated frame, or conflicting fields in the payload.
pub(super) fn parse(name: &str, body: &str, end: &str) -> Option<ToolCall> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Payload {
        name: String,
        args: Map<String, Value>,
    }
    let payload: Payload = serde_json::from_str(body.trim_end().strip_suffix(end)?).ok()?;
    (payload.name == name).then(|| ToolCall {
        id: call_id(0),
        name: payload.name,
        arguments: Value::Object(payload.args),
        malformed_arguments: false,
    })
}
