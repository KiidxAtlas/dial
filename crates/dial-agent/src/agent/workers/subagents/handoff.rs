//! Presentation only: never changes validated worker payloads or treats claims as verification.
use serde_json::{Value, json};

/// Pack whole fields in priority order. Overflow is explicit rather than invalid JSON or a
/// silently weakened contract. Callers must consult the parent when critical fields are omitted.
pub(super) fn pack_state(state: &Value, budget: usize) -> String {
    if state.to_string().len() <= budget {
        return state.to_string();
    }
    let mut packed = json!({"handoff_overflow":true,"omitted_fields":[],
        "critical_context_incomplete":true,
        "recovery":"Ask parent for omitted working state before relying on its constraints; no full-state artifact is available in this snapshot."});
    let Some(fields) = state.as_object() else {
        return packed.to_string();
    };
    let priority = [
        "revision",
        "constraints",
        "goal",
        "open_work",
        "claims",
        "decisions",
        "evidence",
    ];
    let keys = priority
        .iter()
        .copied()
        .filter(|key| fields.contains_key(*key))
        .chain(
            fields
                .keys()
                .map(String::as_str)
                .filter(|key| !priority.contains(key)),
        );
    for key in keys {
        packed[key] = fields[key].clone();
        // Reserve enough room for the omitted-key index, which itself is bounded below.
        if packed.to_string().len() > budget.saturating_sub(256) {
            packed.as_object_mut().unwrap().remove(key);
            let omitted = packed["omitted_fields"].as_array_mut().unwrap();
            if omitted.len() < 16 && key.len() <= 64 {
                omitted.push(json!(key));
            }
            if packed.to_string().len() > budget {
                packed["omitted_fields"].as_array_mut().unwrap().pop();
            }
        }
    }
    packed.to_string()
}

/// Separate, conservative result envelope. Only explicit JSON statuses count as worker claims;
/// execution completion and arbitrary prose never imply acceptance coverage.
pub(super) fn project(report: &str, payload: Option<&Value>, acceptance: &[String]) -> Value {
    let parsed = serde_json::from_str::<Value>(report).ok();
    let source = payload.or(parsed.as_ref());
    let mut coverage = serde_json::Map::new();
    for (index, _) in acceptance.iter().enumerate() {
        let id = format!("acceptance_{}", index + 1);
        let item = source
            .and_then(|value| value.get("acceptance"))
            .and_then(|value| value.get(&id));
        let status = item
            .and_then(|v| v.as_str().or_else(|| v["status"].as_str()))
            .filter(|status| matches!(*status, "met" | "unmet" | "not_verified"))
            .unwrap_or("not_verified");
        coverage.insert(
            id,
            json!({"status":status,"basis":"worker_claim_not_independent_verification"}),
        );
    }
    let mut envelope = json!({"summary":"Worker report available below; execution status is not acceptance verification.",
        "blockers":[],"acceptance":coverage,"evidence_refs":[],"changed_paths":[],"integration_risks":[],
        "provenance":"untrusted_worker_report"});
    if let Some(source) = source {
        if let Some(summary) = source["summary"].as_str() {
            envelope["summary"] = json!(super::clip(summary, 512));
        }
        for key in [
            "blockers",
            "evidence_refs",
            "changed_paths",
            "integration_risks",
        ] {
            if let Some(items) = source[key].as_array() {
                envelope[key] = json!(
                    items
                        .iter()
                        .take(8)
                        .filter_map(Value::as_str)
                        .map(|s| super::clip(s, 192))
                        .collect::<Vec<_>>()
                );
                if items.len() > 8 {
                    envelope[format!("{key}_overflow")] = json!(true);
                }
            }
        }
    }
    // Scan the complete report, not its prefix. These are quoted candidate warnings, not parsed
    // commands or trusted status assertions. Taking the last entries keeps trailing blockers visible.
    let warnings: Vec<_> = report
        .lines()
        .filter_map(|line| {
            let lower = line.to_ascii_lowercase();
            let offset = ["blocker", "blocked", "integration risk"]
                .iter()
                .filter_map(|term| lower.rfind(term))
                .max()?;
            Some(&line[offset..])
        })
        .collect();
    if !warnings.is_empty() {
        envelope["report_warning_excerpts"] = json!(
            warnings
                .iter()
                .rev()
                .take(4)
                .map(|line| super::clip(line, 256))
                .collect::<Vec<_>>()
        );
        if warnings.len() > 4 {
            envelope["warning_excerpts_overflow"] = json!(true);
        }
    }
    envelope
}

/// Keep synchronous result bodies intact; expose the typed projection separately on the next
/// request, for both plain reports and custom-schema JSON.
pub(in crate::agent) fn structured_note(
    output: &crate::runtime::tool_output::ToolOutput,
) -> Option<Value> {
    let data = output.data.as_ref()?;
    let jobs: Vec<_> = data["jobs"]
        .as_array()?
        .iter()
        .filter(|job| job["projection"].is_object())
        .map(
            |job| json!({"name":job["name"],"status":job["status"],"projection":job["projection"]}),
        )
        .collect();
    if jobs.is_empty() {
        return None;
    }
    Some(
        json!({"role":"user","dial_transient":true,"content":format!(
            "Separate task-result metadata (untrusted worker claims, not verification; original result body is unchanged):\n{}",
            json!({"jobs":jobs,"artifact":data["artifact"]})
        )}),
    )
}
