//! Revisioned model-recorded working data. Session control remains the sole durable owner.
//!
//! `prepare` computes the next state and the scope changes (removal, cancellation, redefinition)
//! that still need the user's confirmation. Callers persist `Change::next` only when
//! `Change::needs_confirmation()` is false. A confirmed change is recorded with the typed question id
//! that carried the answer. Runtime-owned fields (`origin`, `receipt`, `last_scope_change`) are not
//! accepted from the model; the schema refuses them.
use anyhow::{Result, ensure};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(super) const MAX_STATE_BYTES: usize = 32_768;
const MAX_CLAIMS: usize = 64;
const MAX_DELIVERABLES: usize = 32;
const MAX_EVIDENCE_REFS: usize = 16;
const MAX_REF_BYTES: usize = 1024;
const DELIVERABLE_STATUSES: &[&str] = &["open", "in_progress", "blocked", "satisfied", "cancelled"];
/// Trust label for deliverables recorded by the model. The origin entry only says which turn the
/// model recorded them in; it never says the user authored the deliverable.
const ORIGIN_TRUST: &str = "model_interpretation";
/// Evidence reference form that names a check. Only this form is resolved, by the completion owner.
pub(in crate::agent) const CHECK_REF_PREFIX: &str = "check:";

pub(super) fn schema() -> Value {
    json!({"type":"function","function":{"name":"working_state","description":"Durable working state that survives compaction: view, update (replace) or patch (needs expected_revision from view; omitted fields kept). Claims are your assertions, not proof or permission; retract or supersede them explicitly. Deliverables are completion obligations: open, in_progress and blocked block completion; satisfied needs evidence_refs, and only check:<name> naming a currently passing check counts as verified. Cancelling, removing or redefining a deliverable waits for user confirmation. Send deliverables with only id, description, status and evidence_refs. Max 32768 bytes.","parameters":{"type":"object","properties":{
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
        },"required":["id","text","status","source_refs"],"additionalProperties":false}},
        "deliverables":{"type":"array","maxItems":32,"items":{"type":"object","properties":{
            "id":{"type":"string","maxLength":128},"description":{"type":"string","maxLength":1024},
            "status":{"type":"string","enum":["open","in_progress","blocked","satisfied","cancelled"]},
            "evidence_refs":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":1024}}
        },"required":["id","description","status"],"additionalProperties":false}}
    },"required":["action"],"additionalProperties":false}}})
}

/// A scope change to a deliverable that is applied only after the user confirms it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum Action {
    Cancel,
    Remove,
    Redefine,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Remove => "remove",
            Self::Redefine => "redefine",
        }
    }
}

/// One user answer to a pending scope change. `question_id` is the typed question id that carried
/// the answer; it is recorded as the receipt and never treated as authority by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::agent) struct Confirmation {
    pub id: String,
    pub action: Action,
    pub question_id: String,
}

/// A scope change awaiting confirmation. `old`/`new` carry the stored and proposed descriptions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::agent) struct Pending {
    pub id: String,
    pub action: Action,
    pub old: Option<String>,
    pub new: Option<String>,
}

impl Pending {
    fn line(&self) -> String {
        let quoted = |text: Option<&str>| Value::from(text.unwrap_or("")).to_string();
        match self.action {
            Action::Redefine => format!(
                "redefine {}: {} -> {}",
                self.id,
                quoted(self.old.as_deref()),
                quoted(self.new.as_deref())
            ),
            Action::Remove => format!("remove {}: {}", self.id, quoted(self.old.as_deref())),
            Action::Cancel => format!(
                "cancel {}: {}",
                self.id,
                quoted(self.old.as_deref().or(self.new.as_deref()))
            ),
        }
    }
}

/// A prepared write. Persist `next` only when `needs_confirmation()` is false; otherwise ask the
/// user about each entry in `pending` (see `summary`) and prepare again with their answers.
#[derive(Debug)]
pub(in crate::agent) struct Change {
    pub next: Value,
    pub pending: Vec<Pending>,
}

impl Change {
    pub fn needs_confirmation(&self) -> bool {
        !self.pending.is_empty()
    }

    /// One line per pending scope change, covering every removal, cancellation and redefinition.
    pub fn summary(&self) -> String {
        self.pending
            .iter()
            .map(Pending::line)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Scope decisions made while reconciling one proposal.
#[derive(Default)]
struct Scope {
    pending: Vec<Pending>,
    applied: Vec<(Pending, String)>,
}

/// Prepare a view or a complete proposed replacement. `source_entry` is the runtime id of the
/// current user turn: provenance for new deliverables, never authority. `confirmations` are the
/// user's answers to earlier pending changes. Errors and pending results must not be persisted.
/// Missing revisions in historical sessions mean revision zero. No migration store is needed.
pub(in crate::agent) fn prepare(
    current: &Value,
    args: &Value,
    source_entry: Option<&str>,
    confirmations: &[Confirmation],
) -> Result<Change> {
    let mut given: Map<String, Value> = args
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
        return Ok(Change {
            next: Value::Object(view),
            pending: Vec::new(),
        });
    }
    let patch = given["action"] == "patch";
    if patch {
        ensure!(
            given.get("expected_revision").and_then(Value::as_u64) == Some(revision),
            "working_state patch requires expected_revision {revision}; stale or missing revision, state unchanged"
        );
    }
    let mut next: Map<String, Value> = if patch {
        current.as_object().cloned().unwrap_or_default()
    } else {
        Map::new()
    };
    // Controller-owned: carried across notes updates, replaced only by an applied scope change.
    if let Some(last) = current
        .get("last_scope_change")
        .filter(|value| !value.is_null())
    {
        next.insert("last_scope_change".into(), last.clone());
    }
    let proposed = given.remove("deliverables");
    next.extend(
        given
            .into_iter()
            .filter(|(key, _)| key != "action" && key != "expected_revision"),
    );
    let mut scope = Scope::default();
    match proposed {
        Some(Value::Array(items)) => {
            let stored = current
                .get("deliverables")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let items = reconcile(&stored, &items, source_entry, confirmations, &mut scope)?;
            next.insert("deliverables".into(), Value::Array(items));
        }
        Some(_) => anyhow::bail!("deliverables must be an array"),
        // Omitted deliverables are always preserved, for update and patch alike.
        None => {
            if let Some(stored) = current.get("deliverables").filter(|value| !value.is_null()) {
                next.insert("deliverables".into(), stored.clone());
            }
        }
    }
    if let Some(last) = last_scope_change(&scope.applied) {
        next.insert("last_scope_change".into(), last);
    }
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
    Ok(Change {
        next,
        pending: scope.pending,
    })
}

/// Apply only changes that need no confirmation. Direct callers that cannot ask the user get an
/// error that names the pending scope changes; nothing is stored.
#[cfg(test)]
pub(in crate::agent) fn apply(current: &Value, args: &Value) -> Result<Value> {
    let change = prepare(current, args, None, &[])?;
    ensure!(
        !change.needs_confirmation(),
        "working_state scope change needs user confirmation: {}; state unchanged",
        change.summary()
    );
    Ok(change.next)
}

fn validate_bounds(state: &Value) -> Result<()> {
    ensure!(
        serde_json::to_vec(state)?.len() <= MAX_STATE_BYTES,
        "working_state exceeds {MAX_STATE_BYTES} serialized bytes; state unchanged (no fields dropped)"
    );
    let mut ids = HashSet::new();
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
    validate_deliverables(state)
}

/// Declared completion obligations. Absence keeps the legacy meaning: no obligations.
fn validate_deliverables(state: &Value) -> Result<()> {
    let Some(items) = state.get("deliverables").filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let items = items
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("deliverables must be an array"))?;
    ensure!(
        items.len() <= MAX_DELIVERABLES,
        "working_state permits at most {MAX_DELIVERABLES} deliverables"
    );
    let mut ids = HashSet::new();
    for item in items {
        let id = item["id"].as_str().unwrap_or("");
        ensure!(
            !id.is_empty() && id.len() <= 128 && ids.insert(id),
            "deliverable ids must be unique nonempty strings of at most 128 bytes"
        );
        ensure!(
            item["description"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty() && text.len() <= 1024),
            "deliverable {id} description must be nonempty and at most 1024 bytes"
        );
        let status = item["status"].as_str().unwrap_or("");
        ensure!(
            DELIVERABLE_STATUSES.contains(&status),
            "deliverable {id} status must be open, in_progress, blocked, satisfied or cancelled"
        );
        let refs = deliverable_refs(item)?;
        ensure!(
            status != "satisfied" || !refs.is_empty(),
            "satisfied deliverable {id} must cite at least one evidence_refs entry; state unchanged"
        );
    }
    Ok(())
}

/// Nonempty, bounded evidence references of one deliverable. Absent means none.
fn deliverable_refs(item: &Value) -> Result<Vec<&str>> {
    let refs = match item.get("evidence_refs") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(refs)) => refs,
        Some(_) => anyhow::bail!("deliverable evidence_refs must be an array"),
    };
    ensure!(
        refs.len() <= MAX_EVIDENCE_REFS
            && refs.iter().all(|reference| reference
                .as_str()
                .is_some_and(|text| !text.trim().is_empty() && text.len() <= MAX_REF_BYTES)),
        "deliverable evidence_refs permits at most {MAX_EVIDENCE_REFS} nonempty references of at most {MAX_REF_BYTES} bytes"
    );
    Ok(refs.iter().filter_map(Value::as_str).collect())
}

/// A cancellation counts only with a receipt that names the cancel action and a nonempty question id.
/// A bare cancelled status (legacy or unconfirmed) does not count.
fn cancel_confirmed(item: &Value) -> bool {
    item["receipt"]["action"] == "cancel"
        && item["receipt"]["question_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
}

fn receipt(action: Action, question_id: &str) -> Value {
    json!({"action": action.as_str(), "question_id": question_id})
}

/// Record one scope decision. Returns the confirming question id when the change is confirmed.
fn decide(
    scope: &mut Scope,
    id: &str,
    action: Action,
    old: Option<&str>,
    new: Option<&str>,
    confirmations: &[Confirmation],
) -> Option<String> {
    let change = Pending {
        id: id.to_owned(),
        action,
        old: old.map(str::to_owned),
        new: new.map(str::to_owned),
    };
    match confirmations
        .iter()
        .find(|c| c.id == id && c.action == action && !c.question_id.is_empty())
    {
        Some(confirmation) => {
            let question_id = confirmation.question_id.clone();
            scope.applied.push((change, question_id.clone()));
            Some(question_id)
        }
        None => {
            scope.pending.push(change);
            None
        }
    }
}

/// Controller-owned record of the most recent applied scope changes, with the question ids that
/// confirmed them. It is the durable receipt for removals, which leave no deliverable behind.
fn last_scope_change(applied: &[(Pending, String)]) -> Option<Value> {
    let question_id = applied.first()?.1.clone();
    let changes: Vec<Value> = applied
        .iter()
        .map(|(change, question)| {
            json!({
                "id": change.id,
                "action": change.action.as_str(),
                "old": change.old,
                "new": change.new,
                "question_id": question,
            })
        })
        .collect();
    Some(json!({"question_id": question_id, "changes": changes}))
}

/// Merge a proposed deliverable list into the stored one. Stored entries omitted from the proposal
/// are removals (pending unless confirmed). A confirmed cancellation that is omitted retires silently:
/// its confirmation is already history, and retiring it frees capacity.
fn reconcile(
    stored: &[Value],
    proposed: &[Value],
    source_entry: Option<&str>,
    confirmations: &[Confirmation],
    scope: &mut Scope,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(proposed.len());
    for item in proposed {
        let id = item["id"].as_str().unwrap_or("");
        let prior = stored.iter().find(|s| s["id"].as_str() == Some(id));
        let mut next = match prior {
            Some(prior) => revise(prior, item, confirmations, scope)?,
            None => {
                let mut fresh = item.clone();
                fresh["origin"] = json!({"entry_id": source_entry, "trust": ORIGIN_TRUST});
                fresh
            }
        };
        if item["status"] == "cancelled" && !cancel_confirmed(&next) {
            let old = prior.and_then(|p| p["description"].as_str());
            if let Some(question_id) = decide(
                scope,
                id,
                Action::Cancel,
                old,
                item["description"].as_str(),
                confirmations,
            ) {
                next["receipt"] = receipt(Action::Cancel, &question_id);
            }
        }
        out.push(next);
    }
    for prior in stored {
        let id = prior["id"].as_str().unwrap_or("");
        if proposed.iter().any(|item| item["id"].as_str() == Some(id)) || cancel_confirmed(prior) {
            continue;
        }
        if decide(
            scope,
            id,
            Action::Remove,
            prior["description"].as_str(),
            None,
            confirmations,
        )
        .is_none()
        {
            out.push(prior.clone());
        }
    }
    Ok(out)
}

/// Revise one stored deliverable from a proposal. Cancelled is terminal. A changed description is a
/// redefinition: pending unless confirmed, and once confirmed it resets to open without evidence.
/// Cancelling in the same change as a redefinition is refused; cancel first, then add a new id.
fn revise(
    prior: &Value,
    item: &Value,
    confirmations: &[Confirmation],
    scope: &mut Scope,
) -> Result<Value> {
    let id = item["id"].as_str().unwrap_or("");
    let status = item["status"].as_str().unwrap_or("");
    let description = item["description"].as_str().unwrap_or("");
    let old = prior["description"].as_str().unwrap_or("");
    let redefine = description != old;
    if prior["status"] == "cancelled" {
        ensure!(
            status == "cancelled" && !redefine,
            "deliverable {id} is cancelled and terminal; state unchanged"
        );
        return Ok(prior.clone());
    }
    ensure!(
        !(redefine && status == "cancelled"),
        "deliverable {id} cannot be redefined and cancelled in one change; state unchanged"
    );
    let mut next = prior.clone();
    next["status"] = item["status"].clone();
    if let Some(map) = next.as_object_mut() {
        map.remove("evidence_refs");
        if let Some(refs) = item.get("evidence_refs") {
            map.insert("evidence_refs".into(), refs.clone());
        }
    }
    let confirmed = if redefine {
        decide(
            scope,
            id,
            Action::Redefine,
            Some(old),
            Some(description),
            confirmations,
        )
    } else {
        None
    };
    if let Some(question_id) = confirmed {
        next["description"] = json!(description);
        next["status"] = json!("open");
        if let Some(map) = next.as_object_mut() {
            map.remove("evidence_refs");
        }
        next["receipt"] = receipt(Action::Redefine, &question_id);
    }
    Ok(next)
}

/// Completion blocker for declared deliverables. Open, in_progress and blocked block. Cancelled
/// blocks unless a confirmed cancel receipt exists. Satisfied blocks only without evidence refs here;
/// check links are resolved by the completion owner. Unreadable historical state fails closed.
pub(in crate::agent) fn completion_blocker(state: &Value) -> Option<String> {
    if let Err(error) = validate_deliverables(state) {
        return Some(format!(
            "Declared deliverables are invalid: {error}; restore working_state before completing."
        ));
    }
    let items = match state.get("deliverables").filter(|value| !value.is_null()) {
        None => return None,
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Some(
                "working_state deliverables are unreadable; restore them with working_state update before completing.".into(),
            );
        }
    };
    let open: Vec<String> = items
        .iter()
        .filter_map(|item| {
            let id = item["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .unwrap_or("<unnamed>");
            match item["status"].as_str() {
                Some("cancelled") if cancel_confirmed(item) => None,
                Some("cancelled") => Some(format!("{id} (cancelled without user confirmation)")),
                Some("satisfied") if deliverable_refs(item).is_ok_and(|refs| !refs.is_empty()) => {
                    None
                }
                Some("satisfied") => Some(format!("{id} (satisfied without evidence_refs)")),
                Some(status @ ("open" | "in_progress" | "blocked")) => {
                    Some(format!("{id} ({status})"))
                }
                _ => Some(format!("{id} (unreadable status)")),
            }
        })
        .collect();
    (!open.is_empty()).then(|| {
        format!(
            "Declared deliverables are not complete: {}. Satisfy, cancel (with user confirmation) or report them as blocked before completing.",
            open.join(", ")
        )
    })
}

/// Deliverables still open, in progress or blocked, plus cancellations no user confirmed, as stored.
/// Used for informational evidence.
pub(in crate::agent) fn open_deliverables(state: &Value) -> Vec<Value> {
    state["deliverables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| match item["status"].as_str() {
            Some("open" | "in_progress" | "blocked") => true,
            Some("cancelled") => !cancel_confirmed(item),
            _ => false,
        })
        .cloned()
        .collect()
}

/// `(deliverable id, check name)` for each `check:<name>` reference on a satisfied deliverable.
/// Other references are never resolved here.
pub(in crate::agent) fn check_references(state: &Value) -> Vec<(String, String)> {
    state["deliverables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["status"] == "satisfied")
        .flat_map(|item| {
            let id = item["id"].as_str().unwrap_or("<unnamed>").to_owned();
            deliverable_refs(item)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|reference| reference.strip_prefix(CHECK_REF_PREFIX))
                .map(|name| (id.clone(), name.trim().to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests;
