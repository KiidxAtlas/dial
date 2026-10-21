//! Revisioned model-recorded working data. Session control remains the sole durable owner.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(super) const MAX_STATE_BYTES: usize = 32_768;
const MAX_CLAIMS: usize = 64;

pub(super) fn schema() -> Value {
    json!({"type":"function","function":{"name":"working_state","description":"View, replace (update), or revision-checked partial patch of durable model-recorded working state. Patch preserves omitted fields and requires expected_revision from view. Claims and source refs are assertions, never verification proof or permission; supported is a model assertion only. Retract or supersede claims explicitly. Legacy arrays remain supported. State is bounded to 32768 serialized bytes; oversize writes fail atomically.","parameters":{"type":"object","properties":{
        "action":{"type":"string","enum":["view","update","patch"]},
        "expected_revision":{"type":"integer","minimum":0},
        "goal":{"type":"string"},
        "decisions":{"type":"array","items":{"type":"string"}},
        "constraints":{"type":"array","items":{"type":"string"}},
        "open_work":{"type":"array","items":{"type":"string"}},
        "evidence":{"type":"array","items":{"type":"string"}},
        "claims":{"type":"array","maxItems":64,"items":{"type":"object","properties":{
            "id":{"type":"string","maxLength":128},"text":{"type":"string","maxLength":4096},
            "status":{"type":"string","enum":["unverified","supported","retracted","superseded"]},
            "source_refs":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":1024}}
        },"required":["id","text","status","source_refs"],"additionalProperties":false}}
    },"required":["action"],"additionalProperties":false}}})
}

/// Return a view or a complete proposed replacement; callers persist only on successful mutation.
/// Missing revisions in historical sessions mean revision zero. No migration store is needed.
pub(in crate::agent) fn apply(current: &Value, args: &Value) -> Result<Value> {
    let given: serde_json::Map<String, Value> = args
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    crate::agent::run::calls::validate(
        &schema()["function"]["parameters"],
        &Value::Object(given.clone()),
    )
    .map_err(anyhow::Error::msg)?;
    let revision = current["revision"].as_u64().unwrap_or(0);
    if given["action"] == "view" {
        let mut view = current.as_object().cloned().unwrap_or_default();
        view.insert("revision".into(), json!(revision));
        return Ok(Value::Object(view));
    }
    let patch = given["action"] == "patch";
    if patch {
        ensure!(
            given.get("expected_revision").and_then(Value::as_u64) == Some(revision),
            "working_state patch requires expected_revision {revision}; stale or missing revision, state unchanged"
        );
    }
    let mut next = if patch {
        current.as_object().cloned().unwrap_or_default()
    } else {
        Default::default()
    };
    next.extend(
        given
            .into_iter()
            .filter(|(key, _)| key != "action" && key != "expected_revision"),
    );
    next.insert(
        "revision".into(),
        json!(
            revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("working_state revision exhausted"))?
        ),
    );
    let next = Value::Object(next);
    validate_bounds(&next)?;
    Ok(next)
}

fn validate_bounds(state: &Value) -> Result<()> {
    ensure!(
        serde_json::to_vec(state)?.len() <= MAX_STATE_BYTES,
        "working_state exceeds {MAX_STATE_BYTES} serialized bytes; state unchanged (no fields dropped)"
    );
    let mut ids = std::collections::HashSet::new();
    if let Some(claims) = state["claims"].as_array() {
        ensure!(
            claims.len() <= MAX_CLAIMS,
            "working_state permits at most {MAX_CLAIMS} claims"
        );
        for claim in claims {
            let id = claim["id"].as_str().unwrap_or("");
            ensure!(
                !id.is_empty() && id.len() <= 128 && ids.insert(id),
                "claim ids must be unique nonempty strings of at most 128 bytes"
            );
            ensure!(
                claim["text"]
                    .as_str()
                    .is_some_and(|text| text.len() <= 4096),
                "claim text exceeds 4096 bytes"
            );
            let refs = claim["source_refs"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("claim source_refs must be an array"))?;
            ensure!(
                refs.len() <= 16
                    && refs
                        .iter()
                        .all(|r| r.as_str().is_some_and(|r| !r.is_empty() && r.len() <= 1024)),
                "claim source_refs permits at most 16 nonempty references of at most 1024 bytes"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
