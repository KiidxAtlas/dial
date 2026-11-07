//! Opt-in, batched projection of settled plain-history arguments. Durable history is untouched.
use super::*;
use crate::storage::artifact::ArtifactStore;
use sha2::{Digest, Sha256};

pub(in crate::agent) fn project_arguments(
    messages: &mut [Value],
    memo: &mut Supersession,
    artifacts: &ArtifactStore,
    threshold: u32,
    raw_tokens: u64,
) {
    if threshold == 0 {
        return;
    }
    let eligible: HashSet<String> = messages
        .iter()
        .filter(|message| {
            successful(message)
                && (message["dial_execution_status"] == "ok"
                    || message["dial_execution"]["status"] == "ok"
                    || message["dial_evidence"]["is_error"] == false
                    || message["is_error"] == false)
        })
        .filter_map(|message| message["tool_call_id"].as_str().map(str::to_owned))
        .collect();
    let boundary = raw_tokens >= memo.argument_boundary.max(u64::from(threshold));
    let mut following = 0;
    for message in messages.iter_mut().rev() {
        let old_enough = following >= 8000;
        following += estimate(message);
        // Opaque provider replay (including signed thinking) is not editable generic history.
        if message["role"] != "assistant" || message.get("dial_provider_content").is_some() {
            continue;
        }
        let Some(calls) = message["tool_calls"].as_array_mut() else {
            continue;
        };
        for call in calls {
            let Some(id) = call["id"].as_str().map(str::to_owned) else {
                continue;
            };
            if let Some(saved) = memo.arguments.get(&id) {
                call["function"]["arguments"] = saved.clone();
                continue;
            }
            if !boundary || !old_enough || !eligible.contains(&id) {
                continue;
            }
            let field = match call["function"]["name"].as_str() {
                Some("write") => "content",
                Some("bash") => "command",
                _ => continue,
            };
            let encoded = call["function"]["arguments"].is_string();
            let mut args = match call["function"]["arguments"].as_str() {
                Some(raw) => match serde_json::from_str::<Value>(raw) {
                    Ok(args) => args,
                    Err(_) => continue,
                },
                None => call["function"]["arguments"].clone(),
            };
            let Some(body) = args[field].as_str().filter(|body| body.len() >= 8192) else {
                continue;
            };
            let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
            let bytes = body.len();
            let Ok(reference) = artifacts.retain(&args.to_string()) else {
                continue;
            };
            args[field] = json!(format!(
                "[Historical argument body elided: {bytes} bytes, SHA256 {digest}. Exact original arguments: {reference}. This placeholder was never executed; the paired tool result records the actual effects.]"
            ));
            let projected = if encoded {
                json!(args.to_string())
            } else {
                args
            };
            memo.arguments.insert(id, projected.clone());
            call["function"]["arguments"] = projected;
        }
    }
    if boundary {
        memo.argument_boundary = raw_tokens.saturating_add(u64::from(threshold));
    }
}

#[cfg(test)]
mod tests;
