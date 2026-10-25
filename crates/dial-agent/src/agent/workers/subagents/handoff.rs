//! Presentation only: never changes validated worker payloads or treats claims as verification.
use crate::tools::Workspace;
use serde_json::{Value, json};
use std::path::Path;

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
/// execution completion and arbitrary prose never imply acceptance coverage. With the parent's
/// `workspace`, cited evidence is checked (`evidence_audit`) and a `met` claim whose only cited
/// evidence does not exist is downgraded; the audit never upgrades a claim.
pub(super) fn project(
    report: &str,
    payload: Option<&Value>,
    acceptance: &[String],
    workspace: Option<&Workspace>,
) -> Value {
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
    if let (Some(workspace), Some(source)) = (workspace, source) {
        audit_evidence(&mut envelope, source, workspace);
    }
    envelope
}

/// References a worker cites for one claim: the item's own `evidence` (string or list) when
/// present, otherwise the report's `evidence_refs`. Bounded like the envelope lists.
fn cited(item: Option<&Value>, report: &Value) -> Vec<String> {
    let own = item.and_then(|item| item.get("evidence"));
    let refs = own.unwrap_or(&report["evidence_refs"]);
    let list: Vec<&Value> = match refs {
        Value::String(_) => vec![refs],
        Value::Array(items) => items.iter().collect(),
        _ => Vec::new(),
    };
    list.into_iter()
        .filter_map(Value::as_str)
        .take(8)
        .map(|s| super::clip(s, 192))
        .collect()
}

/// Check cited references against the parent's view of the workspace at projection time.
/// `valid`: the path (and cited lines) exist; `stale`: the file exists but no longer has the
/// cited lines; `invalid`: missing, outside the workspace, or an unknown artifact; `unparsed`:
/// prose or a form this audit does not check. Only `met` claims whose every cited reference
/// is `invalid` are downgraded to `not_verified`.
fn audit_evidence(envelope: &mut Value, report: &Value, workspace: &Workspace) {
    let mut refs: Vec<String> = cited(None, report);
    let items = envelope["acceptance"]
        .as_object()
        .map(|items| items.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let item_refs: Vec<(String, Vec<String>)> = items
        .into_iter()
        .map(|id| {
            let refs = cited(report.get("acceptance").and_then(|a| a.get(&id)), report);
            (id, refs)
        })
        .collect();
    for (_, cited) in &item_refs {
        for reference in cited {
            if !refs.contains(reference) && refs.len() < 24 {
                refs.push(reference.clone());
            }
        }
    }
    if refs.is_empty() {
        return;
    }
    let audited: Vec<(String, &'static str)> = refs
        .into_iter()
        .map(|reference| {
            let status = evidence_status(&reference, workspace);
            (reference, status)
        })
        .collect();
    let status_of = |reference: &str| {
        audited
            .iter()
            .find(|(seen, _)| seen == reference)
            .map_or("unparsed", |(_, status)| *status)
    };
    let mut downgraded = Vec::new();
    for (id, cited) in &item_refs {
        if envelope["acceptance"][id]["status"] == "met"
            && !cited.is_empty()
            && cited
                .iter()
                .all(|reference| status_of(reference) == "invalid")
        {
            envelope["acceptance"][id] =
                json!({"status":"not_verified","basis":"cited_evidence_invalid","claimed":"met"});
            downgraded.push(id.clone());
        }
    }
    let count = |wanted: &str| audited.iter().filter(|(_, s)| *s == wanted).count();
    envelope["evidence_audit"] = json!({
        "refs": audited.iter().map(|(reference, status)| json!({"ref":reference,"status":status})).collect::<Vec<_>>(),
        "valid": count("valid"),
        "stale": count("stale"),
        "invalid": count("invalid"),
        "unparsed": count("unparsed"),
        "downgraded": downgraded,
        "basis": "parent_workspace_check_at_projection_existence_only_not_content_verification",
    });
}

/// `path:line[-end]`, `path:line:column` or a bare path; the range is 1-based and inclusive.
fn split_location(reference: &str) -> (&str, Option<(usize, usize)>) {
    let mut path = reference;
    let mut range = None;
    for _ in 0..2 {
        let Some((head, tail)) = path.rsplit_once(':') else {
            break;
        };
        let parsed = match tail.split_once('-') {
            Some((start, end)) => start.parse().ok().zip(end.parse().ok()),
            None => tail.parse().ok().map(|line| (line, line)),
        };
        let Some(parsed) = parsed else {
            break;
        };
        range = Some(parsed);
        path = head;
    }
    (path, range)
}

/// Files larger than this are checked for existence only.
const LINE_CHECK_BYTES: u64 = 8 << 20;

fn evidence_status(reference: &str, workspace: &Workspace) -> &'static str {
    let reference = reference.trim().trim_matches('`');
    if reference.starts_with("artifact://") {
        return match workspace.artifacts() {
            Some(store) if store.info(reference).is_ok() => "valid",
            Some(_) => "invalid",
            None => "unparsed",
        };
    }
    if reference.is_empty() || reference.contains(char::is_whitespace) || reference.contains("://")
    {
        return "unparsed";
    }
    let (path, range) = split_location(reference);
    let looks_like_path = path.contains('/') || Path::new(path).extension().is_some();
    if !looks_like_path {
        return "unparsed";
    }
    let Ok(resolved) = workspace.resolve(path) else {
        return "invalid";
    };
    let Ok(metadata) = std::fs::metadata(&resolved) else {
        return "invalid";
    };
    let Some((start, end)) = range else {
        return "valid";
    };
    if start == 0 || end < start || metadata.is_dir() {
        return "invalid";
    }
    if metadata.len() > LINE_CHECK_BYTES {
        return "valid";
    }
    match std::fs::read(&resolved) {
        Ok(bytes) if String::from_utf8_lossy(&bytes).lines().count() >= end => "valid",
        Ok(_) => "stale",
        Err(_) => "invalid",
    }
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
