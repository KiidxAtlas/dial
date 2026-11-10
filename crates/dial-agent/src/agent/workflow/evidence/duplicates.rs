//! Fold newly encountered exact observation repeats without rewriting an already-seen prefix.
use super::*;

pub(in crate::agent) fn project_duplicates(messages: &[Value], memo: &mut Supersession) {
    // Opaque replay is not editable plain history, even after switching providers.
    let calls = observation_calls(messages);
    let opaque: HashSet<&str> = messages
        .iter()
        .filter(|message| message.get("dial_provider_content").is_some())
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| call["id"].as_str())
        .collect();
    let mut retained = HashMap::new();
    for message in messages {
        let Some(id) = message["tool_call_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let fresh = memo.seen_results.insert(id.to_owned());
        if !memo.holds_full(id)
            || !successful(message)
            || opaque.contains(id)
            || message["dial_evidence"]["observation"].is_string()
        {
            continue;
        }
        let Some(key) = observation_key(message, &calls) else {
            continue;
        };
        if let Some(first) = retained.get(&key) {
            // A result seen in an earlier request stays byte-stable even if its original anchor
            // disappeared after compaction. Only a newly appended duplicate gets a pointer.
            if fresh {
                let artifact = message["dial_evidence"]["artifact"]
                    .as_str()
                    .unwrap_or_default();
                let pointer = format!(
                    "[Exact repeat of successful observation {first}: identical tool arguments and output. Full original result for this call: {artifact}; retrieve with read. This call ran again; this pointer is not a fresh verification claim.]"
                );
                if message["content"]
                    .as_str()
                    .is_some_and(|text| text.len() > pointer.len())
                {
                    memo.duplicates.insert(id.to_owned(), pointer);
                }
            }
        } else {
            retained.insert(key, id.to_owned());
        }
    }
}

#[cfg(test)]
mod tests;
