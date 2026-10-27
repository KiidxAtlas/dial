use super::*;

fn plan() -> RolloverPlan {
    RolloverPlan {
        owner_id: "owner".into(),
        source_leaf: Some("leaf".into()),
        boundary_id: None,
        notes: Some(ContextNotesRevision {
            entry_id: "notes-entry".into(),
            revision: "rev".into(),
            text: "notebook".into(),
        }),
        history: vec![json!({"role":"user","content":"goal"})],
        digest: String::new(),
        prompt_tokens: 10,
        history_generation: 2,
    }
}

fn projection() -> Chat {
    Chat {
        messages: vec![json!({"role":"system","content":"policy"})],
        tools: vec![json!({"type":"function","function":{"name":"new_context"}})],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    }
}

const BUDGET: RolloverBudget<'static> = RolloverBudget {
    identity: "model",
    prompt_tokens: 10,
    prompt_budget: 50,
};

#[test]
fn plan_digest_keeps_its_serialized_identity() {
    let plan = plan();
    let chat = projection();
    // The pre-refactor argument shapes: borrowed `&str` and `&[Value]` fields.
    let history: &[Value] = &plan.history;
    let notes = plan.notes.as_ref().map(|notes| {
        (
            notes.entry_id.as_str(),
            notes.revision.as_str(),
            notes.text.as_str(),
        )
    });
    let legacy = serde_json::to_vec(&(
        (
            "experimental-context-rollover",
            1,
            plan.owner_id.as_str(),
            plan.source_leaf.as_deref(),
            plan.boundary_id.as_deref(),
            notes,
            history,
            plan.history_generation,
        ),
        (
            &chat.messages,
            &chat.tools,
            chat.max_tokens,
            chat.temperature,
            chat.seed,
        ),
        (BUDGET.identity, BUDGET.prompt_tokens, BUDGET.prompt_budget),
    ))
    .unwrap();
    assert_eq!(
        plan_digest(&plan, &chat, BUDGET).unwrap(),
        format!("{:x}", Sha256::digest(legacy))
    );
}

#[test]
fn plan_digest_ignores_the_seal_and_tracks_the_request() {
    let mut plan = plan();
    let chat = projection();
    let sealed = plan_digest(&plan, &chat, BUDGET).unwrap();
    plan.digest = sealed.clone();
    assert_eq!(plan_digest(&plan, &chat, BUDGET).unwrap(), sealed);

    let mut changed = projection();
    changed
        .messages
        .push(json!({"role":"user","content":"late"}));
    assert_ne!(plan_digest(&plan, &changed, BUDGET).unwrap(), sealed);
    plan.history_generation += 1;
    assert_ne!(plan_digest(&plan, &chat, BUDGET).unwrap(), sealed);
}

#[test]
fn tool_name_reads_both_schema_shapes() {
    assert_eq!(
        tool_name(&json!({"type":"function","function":{"name":"a"}})),
        Some("a")
    );
    assert_eq!(tool_name(&json!({"name":"b"})), Some("b"));
    assert_eq!(tool_name(&json!({"type":"function"})), None);
}
