//! The Type a conversation belongs to, as the desktop sends it. A `send` carries one of three
//! states in `type_context`:
//!
//! - an object `{id, briefing}`: the prompt was written in that Type. The id is kept on the stored
//!   message, the briefing reaches the model as context, and the text stays the user's own words;
//! - `null`: a modern prompt that names no Type. The text is the user's own words, even when it is
//!   exactly legacy framing;
//! - absent: a client that predates `type_context`. Its framing is read from the text, once.
//!
//! Legacy framing is a standalone line: `[[type:ID]]` then a newline, then optionally a
//! `<type-context>` block made of complete lines. Anything else is the user's text.

use dial_agent::agent::TypeContext;
use serde_json::Value;

const LEGACY_OPEN: &str = "[[type:";
const CONTEXT_OPEN: &str = "<type-context>";
const CONTEXT_CLOSE: &str = "</type-context>";

/// Most briefing text one turn may carry; a Type's description and item counts are far smaller.
pub(crate) const MAX_BRIEFING_BYTES: usize = 64 * 1024;

/// The user's text without Type framing, and the Type when the turn was written in one.
pub(crate) fn split(params: &Value, text: &str) -> Result<(String, Option<TypeContext>), String> {
    match params.get("type_context") {
        // A modern prompt that names no Type: the text is exactly what the user typed.
        Some(Value::Null) => Ok((text.to_owned(), None)),
        // A client that predates `type_context` frames the text itself.
        None => Ok(legacy(text)),
        Some(context) => {
            let id = context["id"]
                .as_str()
                .map(str::trim)
                .filter(|id| valid_id(id));
            let briefing = context["briefing"]
                .as_str()
                .map(str::trim)
                .filter(|briefing| !briefing.is_empty() && briefing.len() <= MAX_BRIEFING_BYTES);
            match (id, briefing) {
                (Some(id), Some(briefing)) => Ok((
                    text.to_owned(),
                    Some(TypeContext {
                        id: id.to_owned(),
                        briefing: Some(briefing.to_owned()),
                    }),
                )),
                _ => Err(format!(
                    "type_context requires an id of [a-z0-9-] and a briefing of at most {MAX_BRIEFING_BYTES} bytes"
                )),
            }
        }
    }
}

/// The pre-structured form. The tag is a line of its own, so `[[type:ID]] more text` on one line is
/// not framing; a `<type-context>` block must also be complete lines. Anything else is untouched.
fn legacy(text: &str) -> (String, Option<TypeContext>) {
    let untouched = || (text.to_owned(), None);
    let Some((id, rest)) = text
        .strip_prefix(LEGACY_OPEN)
        .and_then(|tail| tail.split_once("]]"))
    else {
        return untouched();
    };
    if !valid_id(id) {
        return untouched();
    }
    let Some(rest) = rest.strip_prefix('\n') else {
        return untouched();
    };
    if rest.starts_with(CONTEXT_OPEN) {
        let Some(close) = rest.find(CONTEXT_CLOSE) else {
            return untouched();
        };
        let end = close + CONTEXT_CLOSE.len();
        let Some(body) = rest[end..].strip_prefix('\n') else {
            return untouched();
        };
        return (
            body.to_owned(),
            Some(TypeContext {
                id: id.to_owned(),
                briefing: Some(rest[..end].to_owned()),
            }),
        );
    }
    (
        rest.to_owned(),
        Some(TypeContext {
            id: id.to_owned(),
            briefing: None,
        }),
    )
}

/// Type ids are lowercase words with dashes, as the desktop names them.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests;
