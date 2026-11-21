//! Collaboration guest lifecycle through the same owner used by the terminal.
use super::coded;
use crate::collab::{protocol, registry, room};
use crate::config::{paths::Layout, types::Config};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

pub(super) mod hosting;

#[cfg(test)]
pub(super) static TEST_ROOM: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Metadata and admitted peer state only: room links, signing keys and proofs are never returned.
pub(super) fn status(layout: &Layout) -> Result<Value> {
    Ok(json!({"room": room::status(), "local_hosts": registry::list_live(&layout.user_dir)?}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Join {
    link: String,
    #[serde(default = "guest_name")]
    name: String,
    confirm: bool,
}

fn guest_name() -> String {
    "guest".to_owned()
}

fn request(params: &Value, relay: &str) -> Result<Join> {
    let request: Join = serde_json::from_value(params.clone())
        .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
    if !request.confirm {
        return Err(coded(
            ErrorCode::InvalidParams,
            "joining contacts a relay and shares your chosen name with the host; ask the user, then send confirm: true",
        ));
    }
    if request.link.len() > 8192
        || request.name.trim().is_empty()
        || request.name.chars().count() > 64
        || request.name.chars().any(char::is_control)
    {
        return Err(coded(
            ErrorCode::InvalidParams,
            "link must be at most 8192 bytes and name must be 1-64 printable characters",
        ));
    }
    // Refuse malformed links before attempting a connection. Do not echo the secret link in errors.
    protocol::parse_link(&request.link, relay)
        .map_err(|_| coded(ErrorCode::InvalidParams, "invalid collaboration link"))?;
    Ok(request)
}

/// Connection setup is bounded by the socket owner. Cancellation is checked before opening and
/// before installation; `leave` also invalidates an in-flight join. Admission is asynchronous:
/// a successful result means joining, not that write access has been granted.
pub(super) fn join(config: &Config, params: &Value, cancel: &AtomicBool) -> Result<Value> {
    let request = request(params, &config.collab.relay_url)?;
    room::join(
        &config.collab.relay_url,
        &request.link,
        &request.name,
        cancel,
    )
    .map_err(refusal)?;
    Ok(json!({"joining": true, "admission_pending": true}))
}

/// Stop only the joined guest or pending join, never a hosted room.
pub(super) fn leave() -> Result<Value> {
    room::leave().map_err(|error| refusal(error.into()))?;
    Ok(json!({"left": true}))
}

/// Route guest input before local expansion, attachment reads, rewind or compute admission.
/// A guest refusal is terminal for this request; it never falls through to the local agent.
pub(super) fn prompt(method: &str, params: &Value) -> Option<Result<Value>> {
    room::with(|slot| match slot {
        Some(room::Room::Guest(guest)) => Some((|| {
            if method != "send" {
                return Err(coded(
                    ErrorCode::Refused,
                    "joined guests can only send plain text; leave before local retry or voice input",
                ));
            }
            let text = params["text"]
                .as_str()
                .ok_or_else(|| coded(ErrorCode::InvalidParams, "send requires params.text"))?;
            for key in ["images", "inline_images"] {
                if !params[key].is_null() && params[key] != json!([]) {
                    return Err(coded(
                        ErrorCode::InvalidParams,
                        "collaboration guest prompts do not support attachments",
                    ));
                }
            }
            guest
                .send_prompt(text)
                .map_err(|reason| coded(ErrorCode::Refused, reason))?;
            Ok(json!({"routed":"collaboration_host","local_turn":false}))
        })()),
        _ => None,
    })
}

/// Guest aborts retain the host's write-admission gate and never cancel unrelated local work.
pub(super) fn abort() -> Option<Result<Value>> {
    room::send_guest_abort().map(|result| {
        result
            .map(|()| json!({"routed":"collaboration_host","aborted_local":false}))
            .map_err(|reason| coded(ErrorCode::Refused, reason))
    })
}

/// Answer the host's open question as a joined guest. `option` is 1-based, as listed in
/// `collaboration_status` `room.question.options`. The host decides whether guests may answer.
pub(super) fn answer(params: &Value) -> Result<Value> {
    let option = params
        .as_object()
        .filter(|fields| fields.keys().all(|key| key == "option"))
        .and_then(|fields| fields.get("option")?.as_u64())
        .and_then(|option| usize::try_from(option).ok())
        .ok_or_else(|| {
            coded(
                ErrorCode::InvalidParams,
                "collaboration_answer takes only params.option, a 1-based option number",
            )
        })?;
    match room::send_guest_answer(option) {
        None => Err(coded(ErrorCode::NotPending, "not joined as a guest")),
        Some(Ok(())) => Ok(json!({"routed": "collaboration_host", "option": option})),
        Some(Err(reason)) => Err(coded(ErrorCode::Refused, reason)),
    }
}

fn refusal(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<room::Refused>() {
        Some(room::Refused::Busy) => coded(ErrorCode::Busy, error),
        Some(room::Refused::NotGuest) => coded(ErrorCode::NotPending, error),
        Some(room::Refused::Cancelled) => coded(ErrorCode::Conflict, error),
        None => error,
    }
}

#[cfg(test)]
mod tests;
