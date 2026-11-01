//! Runtime-recorded user provenance, separate from model-authored working state and turn drafts.
//! The bounded trail is context, not an inferred decision that an older task remains active.
use serde_json::{Value, json};

const MAX_EARLIER_MESSAGES: usize = 8;
const MAX_EARLIER_BYTES: usize = 16_384;

#[derive(Clone, Copy, Debug)]
pub(in crate::agent) enum Kind {
    Turn,
    Steering,
}

/// Record only an admitted user message. Harness/peer/monitor text must never enter this trail.
/// Keep the latest text exact; older overflow is explicit and recoverable from session history.
pub(in crate::agent) fn record(control: &mut Value, text: &str, entry_id: Value, kind: Kind) {
    let mut intent = control["user_intent"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let previous = intent.remove("latest");
    // Old control state did not identify the writer: child coordination and harness text may
    // occupy current_instruction. Preserve it as unknown context, never as an admitted user.
    let mut legacy = intent.remove("legacy_context").or_else(|| {
        if previous.is_some() {
            return None;
        }
        control["current_instruction"].as_str().map(|text| {
            json!({"text":text,"entry_id":control["current_instruction_entry_id"],
                "provenance":"unknown_effective_instruction"})
        })
    });
    if serde_json::to_vec(&legacy).map_or(usize::MAX, |bytes| bytes.len())
        > MAX_EARLIER_BYTES.saturating_sub(2)
        && let Some(fields) = legacy.as_mut().and_then(Value::as_object_mut)
    {
        fields.remove("text");
        fields.insert("text_omitted".into(), json!(true));
    }
    let legacy_bytes = serde_json::to_vec(&legacy).map_or(usize::MAX, |bytes| bytes.len());
    let mut earlier = intent
        .remove("earlier")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    if let Some(previous) = previous {
        earlier.push(previous);
    }
    let mut omitted = intent
        .get("omitted_messages")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    while !earlier.is_empty()
        && (earlier.len() > MAX_EARLIER_MESSAGES
            || serde_json::to_vec(&earlier)
                .map_or(usize::MAX, |bytes| bytes.len())
                .saturating_add(legacy_bytes)
                > MAX_EARLIER_BYTES)
    {
        earlier.remove(0);
        omitted = omitted.saturating_add(1);
    }
    control["user_intent"] = json!({
        "latest":{"text":text,"entry_id":entry_id,"kind":match kind {
            Kind::Turn => "user_turn", Kind::Steering => "user_steering"
        }},
        "earlier":earlier,"omitted_messages":omitted,"legacy_context":legacy,
    });
}

/// Render only admitted user provenance at the request tail, never an effective draft or a
/// harness continuation. The excerpt bounds repeated context without hiding a suffix correction.
pub(in crate::agent) fn request_context(control: &Value) -> Option<String> {
    const MAX_TEXT_BYTES: usize = 4096;
    const OMITTED: &str =
        "\n[... middle omitted; full user message remains in session history ...]\n";
    let latest = &control["user_intent"]["latest"];
    if !matches!(latest["kind"].as_str(), Some("user_turn" | "user_steering")) {
        return None;
    }
    let text = latest["text"]
        .as_str()
        .filter(|text| !text.trim().is_empty())?;
    let text_truncated = text.len() > MAX_TEXT_BYTES;
    let excerpt = if text_truncated {
        let retained = MAX_TEXT_BYTES - OMITTED.len();
        let mut head = retained / 2;
        while !text.is_char_boundary(head) {
            head -= 1;
        }
        let mut tail = text.len() - (retained - head);
        while !text.is_char_boundary(tail) {
            tail += 1;
        }
        format!("{}{OMITTED}{}", &text[..head], &text[tail..])
    } else {
        text.to_owned()
    };
    let payload = json!({
        "entry_id":latest["entry_id"], "kind":latest["kind"],
        "text":excerpt, "text_truncated":text_truncated,
    });
    Some(format!(
        "{}\n{payload}",
        crate::agent::prompts::system::CURRENT_USER_REQUEST
    ))
}

/// Expose originals alongside, not in place of, reviewed effective drafts. Historical messages
/// are not automatically amendments: only the user can replace, cancel or expand the task.
pub(in crate::agent) fn continuation(prompt: &str, control: &Value, history: &[Value]) -> String {
    if control["user_intent"].is_null() {
        return prompt.to_owned();
    }
    let block = format!(
        "{}\n{}",
        crate::agent::prompts::modes::goal::INTENT_POLICY,
        control["user_intent"]
    );
    let mut context = prompt.to_owned();
    crate::agent::workflow::efficiency::append_ambient(&mut context, &block, history);
    context
}

#[cfg(test)]
mod tests;
