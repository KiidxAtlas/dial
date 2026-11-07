//! Cache-stable tool offers.
//!
//! Every provider caches the prompt as a prefix that starts with the tool definitions, so editing
//! `tools` between requests (a tool load, a mode toggle, a request-scoped narrowing) re-bills the
//! whole conversation. A caller may instead send one tool surface that stays the same for the
//! session and mark, per tool, whether it is callable on this request ([`Mark::callable`]) and
//! whether its definition stays out of the prompt until a tool result references it
//! ([`Mark::deferred`]). Each wire turns the marks into its own form ([`Exposure`]), or removes
//! them so the request is exactly the callable set, as every route sent before.
//!
//! Marks never grant anything: the harness still admits only the tools it offered, and a call to
//! an uncallable tool is refused before schema validation, approval or execution.
use super::Chat;
use serde_json::{Value, json};
use std::borrow::Cow;

/// Per-tool mark on an OpenAI-shaped function schema (`{"type":"function","function":{..}}`).
/// Absent means callable and not deferred. Harness bookkeeping: never sent as such.
pub const MARK: &str = "dial_tool_offer";
/// On a tool-result message: the tool names this result makes visible by reference. Only a wire
/// that defers tools ([`Exposure::Deferred`]) reads it; every other wire sends the result text.
pub const REFERENCES: &str = "dial_tool_references";

/// How a route's wire carries a marked tool surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Exposure {
    /// Only callable tools are sent and references are dropped: the request as before marks.
    #[default]
    Plain,
    /// Anthropic Messages on `api.anthropic.com` for a model that documents tool search: deferred
    /// tools carry `defer_loading: true`, and a tool result that references them returns
    /// `tool_reference` blocks, which the API expands inline without touching the cached prefix.
    /// Uncallable tools stay in `tools` and the harness refuses their calls.
    Deferred,
    /// OpenAI Responses on `api.openai.com`: every non-deferred tool is sent and
    /// `tool_choice: {"type":"allowed_tools"}` names the callable ones.
    AllowedTools,
}

/// What a request's caller says about one tool of its surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// The harness admits calls to this tool on this request.
    pub callable: bool,
    /// The definition stays out of the prompt until a tool result references it.
    pub deferred: bool,
}

impl Mark {
    /// An ordinary offered tool: what an unmarked schema means.
    pub const CALLABLE: Self = Self {
        callable: true,
        deferred: false,
    };
}

/// Mark one schema. [`Mark::CALLABLE`] removes the mark, so an ordinary tool stays unchanged.
pub fn mark(schema: &mut Value, mark: Mark) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if mark == Mark::CALLABLE {
        object.remove(MARK);
    } else {
        object.insert(
            MARK.into(),
            json!({"callable": mark.callable, "deferred": mark.deferred}),
        );
    }
}

/// The mark of one schema; an unmarked or malformed mark is an ordinary callable tool.
pub fn mark_of(schema: &Value) -> Mark {
    let Some(mark) = schema.get(MARK) else {
        return Mark::CALLABLE;
    };
    Mark {
        callable: mark["callable"].as_bool().unwrap_or(true),
        deferred: mark["deferred"].as_bool().unwrap_or(false),
    }
}

/// The callable tools of a surface, unmarked and in order: exactly what a route without marks
/// would have been sent, and what history contracts are checked against. An unmarked surface is
/// returned borrowed.
pub fn callable(tools: &[Value]) -> Cow<'_, [Value]> {
    if !tools.iter().any(|tool| tool.get(MARK).is_some()) {
        return Cow::Borrowed(tools);
    }
    Cow::Owned(
        tools
            .iter()
            .filter(|tool| mark_of(tool).callable)
            .map(unmarked)
            .collect(),
    )
}

/// Names a tool-result message makes visible by reference, in order.
pub fn references(message: &Value) -> Vec<&str> {
    message[REFERENCES]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// The callable tool names when the surface holds a sent tool that is not callable, for
/// `allowed_tools`; `None` when every sent tool is callable, so no `tool_choice` is needed.
pub fn allowed_names(tools: &[Value]) -> Option<Vec<&str>> {
    let mut narrowed = false;
    let mut names = Vec::new();
    for tool in tools {
        let Some(name) = tool["function"]["name"].as_str() else {
            continue;
        };
        if mark_of(tool).callable {
            names.push(name);
        } else {
            narrowed = true;
        }
    }
    narrowed.then_some(names)
}

/// The request in the form `exposure` can carry. A request without marks or references is
/// returned borrowed and unchanged.
///
/// - [`Exposure::Plain`]: only callable tools, unmarked; references removed.
/// - [`Exposure::Deferred`]: the whole surface with its marks. A surface whose sent tools would
///   all be deferred is rejected by the API, so it falls back to `Plain`.
/// - [`Exposure::AllowedTools`]: deferred tools that are not callable are dropped and callable
///   ones are sent normally; references are removed. The marks stay for `allowed_tools`.
pub fn normalize(req: &Chat, exposure: Exposure) -> Cow<'_, Chat> {
    let marked = req.tools.iter().any(|tool| tool.get(MARK).is_some());
    let referenced = req
        .messages
        .iter()
        .any(|message| message.get(REFERENCES).is_some());
    let exposure = if exposure == Exposure::Deferred
        && !req.tools.is_empty()
        && req.tools.iter().all(|tool| mark_of(tool).deferred)
    {
        Exposure::Plain
    } else {
        exposure
    };
    match exposure {
        Exposure::Deferred => Cow::Borrowed(req),
        _ if !marked && !referenced => Cow::Borrowed(req),
        Exposure::Plain => {
            let mut plain = req.clone();
            plain.tools = callable(&req.tools).into_owned();
            strip_references(&mut plain.messages);
            Cow::Owned(plain)
        }
        Exposure::AllowedTools => {
            let mut allowed = req.clone();
            allowed.tools = req
                .tools
                .iter()
                .filter_map(|tool| {
                    let mark = mark_of(tool);
                    match (mark.callable, mark.deferred) {
                        (false, true) => None,
                        (true, _) => Some(unmarked(tool)),
                        // Sent, but left out of `allowed_tools`.
                        (false, false) => Some(tool.clone()),
                    }
                })
                .collect();
            strip_references(&mut allowed.messages);
            Cow::Owned(allowed)
        }
    }
}

fn unmarked(tool: &Value) -> Value {
    let mut tool = tool.clone();
    if let Some(object) = tool.as_object_mut() {
        object.remove(MARK);
    }
    tool
}

fn strip_references(messages: &mut [Value]) {
    for message in messages {
        if let Some(object) = message.as_object_mut() {
            object.remove(REFERENCES);
        }
    }
}

#[cfg(test)]
mod tests;
