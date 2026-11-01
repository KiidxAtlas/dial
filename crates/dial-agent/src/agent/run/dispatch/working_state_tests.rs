use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::agent::workflow::questions::{QuestionAnswer, QuestionOutcome, QuestionRequest};
use std::sync::atomic::{AtomicBool, Ordering};

const SCOPE_QUESTION: &str = "obligation-scope";
const CONFIRM: &str = "Confirm scope change";
const KEEP: &str = "Keep existing scope";
const TURN: &str = "u1";

/// Typed human reply double. It records each scope question, so a test can prove whether a
/// confirmation was asked at all, and can simulate an interrupt arriving while it is open.
struct Human {
    reply: QuestionOutcome,
    asked: Vec<QuestionRequest>,
    interrupt_while_asked: bool,
}

impl Human {
    fn replying(reply: QuestionOutcome) -> Self {
        Self {
            reply,
            asked: Vec::new(),
            interrupt_while_asked: false,
        }
    }

    fn question_id(&self) -> &str {
        &self
            .asked
            .last()
            .expect("a scope question was asked")
            .question_id
    }
}

impl Ui for Human {
    fn ask_questions(&mut self, request: &QuestionRequest, cancel: &AtomicBool) -> QuestionOutcome {
        self.asked.push(request.clone());
        if self.interrupt_while_asked {
            cancel.store(true, Ordering::SeqCst);
        }
        self.reply.clone()
    }
}

fn picked(label: &str) -> QuestionOutcome {
    QuestionOutcome::Answered(vec![QuestionAnswer {
        id: SCOPE_QUESTION.into(),
        selected_options: vec![label.to_owned()],
        ..Default::default()
    }])
}

fn confirm() -> QuestionOutcome {
    picked(CONFIRM)
}

/// Runs `test` against an agent whose latest request is a real user turn, the source that scope
/// changes are attributed to.
fn with_turn(test: impl FnOnce(&mut Agent<'_>, &mut Backend)) {
    with_agent("", |agent| {
        let mut state = agent.session.control_state();
        state["user_intent"] =
            json!({"latest":{"kind":"user_turn","entry_id":TURN,"text":"finish the release"}});
        agent.session.set_control_state(state).unwrap();
        let mut backend = agent.backend.take().unwrap();
        test(agent, &mut backend);
        agent.backend = Some(backend);
    });
}

fn call(
    agent: &mut Agent<'_>,
    backend: &mut Backend,
    human: &mut Human,
    args: Value,
) -> ToolOutput {
    agent.dispatch(
        backend,
        &ToolCall {
            id: "working-state".into(),
            name: "working_state".into(),
            arguments: args,
            malformed_arguments: false,
        },
        human,
    )
}

fn working(agent: &Agent<'_>) -> Value {
    agent.session.control_state()["working_state"].clone()
}

fn controller(agent: &Agent<'_>) -> Value {
    agent.session.control_state()
}

/// The user-authored fields of each deliverable, in order. Runtime fields are checked separately.
fn user_fields(state: &Value) -> Vec<(String, String, String)> {
    state["deliverables"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            (
                item["id"].as_str().unwrap_or_default().to_owned(),
                item["description"].as_str().unwrap_or_default().to_owned(),
                item["status"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn owned(rows: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
    rows.iter()
        .map(|(id, description, status)| ((*id).into(), (*description).into(), (*status).into()))
        .collect()
}

fn origin_of_turn() -> Value {
    json!({"entry_id": TURN, "trust": "model_interpretation"})
}

/// Declares two open obligations. Setup answers any confirmation it asks, so it never depends on
/// whether declaring is itself a scope change.
fn declare_pair(agent: &mut Agent<'_>, backend: &mut Backend) {
    let mut human = Human::replying(confirm());
    let out = call(
        agent,
        backend,
        &mut human,
        json!({"action":"update","deliverables":[
            {"id":"d1","description":"ship the build","status":"open"},
            {"id":"d2","description":"write the notes","status":"open"}
        ]}),
    );
    assert!(!out.is_error, "{}", out.text);
}

#[test]
fn stale_patch_is_refused_and_leaves_the_persisted_state_unchanged() {
    with_turn(|agent, backend| {
        let mut human = Human::replying(QuestionOutcome::Unsupported("no human".into()));
        let updated = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","goal":"ship"}),
        );
        assert!(!updated.is_error, "{}", updated.text);
        let before = working(agent);

        let stale = call(
            agent,
            backend,
            &mut human,
            json!({"action":"patch","expected_revision":0,"goal":"oops"}),
        );
        assert!(stale.is_error, "{}", stale.text);
        assert_eq!(working(agent), before);
    });
}

#[test]
fn declared_obligations_persist_with_user_fields_and_exact_runtime_provenance() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let state = working(agent);
        assert_eq!(state["revision"], 1);
        assert_eq!(
            user_fields(&state),
            owned(&[
                ("d1", "ship the build", "open"),
                ("d2", "write the notes", "open"),
            ])
        );
        for item in state["deliverables"].as_array().unwrap() {
            assert_eq!(item["origin"], origin_of_turn(), "{item}");
            assert!(item.get("receipt").is_none(), "{item}");
        }
    });
}

#[test]
fn notes_only_patch_keeps_obligations_and_asks_nothing() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let before = working(agent);
        let mut human = Human::replying(QuestionOutcome::Unsupported("must not be asked".into()));
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"patch","expected_revision":1,"decisions":["stage the rollout"]}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(human.asked.is_empty(), "notes must not ask for scope");
        let after = working(agent);
        assert_eq!(after["deliverables"], before["deliverables"]);
        assert_eq!(after["decisions"], json!(["stage the rollout"]));
        assert_eq!(after["revision"], 2);
    });
}

#[test]
fn notes_only_update_keeps_obligations_and_asks_nothing() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let before = working(agent);
        let mut human = Human::replying(QuestionOutcome::Unsupported("must not be asked".into()));
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","decisions":["stage the rollout"]}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(
            human.asked.is_empty(),
            "a full update without deliverables must not ask"
        );
        let after = working(agent);
        assert_eq!(after["deliverables"], before["deliverables"]);
        assert_eq!(after["decisions"], json!(["stage the rollout"]));
        assert_eq!(after["revision"], 2);
    });
}

#[test]
fn confirmed_removal_persists_the_change_and_a_controller_record_naming_the_question() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let mut human = Human::replying(confirm());
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","deliverables":[
                {"id":"d1","description":"ship the build","status":"open"}
            ]}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(human.asked.len(), 1, "one scope question");
        assert_eq!(
            user_fields(&working(agent)),
            owned(&[("d1", "ship the build", "open")])
        );
        let record = &working(agent)["last_scope_change"];
        assert_eq!(record["question_id"], human.question_id());
        assert_eq!(record["changes"].as_array().map(Vec::len), Some(1));
        assert_eq!(record["changes"][0]["id"], "d2");
        assert_eq!(record["changes"][0]["action"], "remove");
    });
}

#[test]
fn confirmed_cancel_and_redefinition_record_their_receipt_with_the_question() {
    let cases: [(&str, Value, &str, Value); 2] = [
        (
            "cancel",
            json!([
                {"id":"d1","description":"ship the build","status":"open"},
                {"id":"d2","description":"write the notes","status":"cancelled"}
            ]),
            "d2",
            json!({"action":"cancel"}),
        ),
        (
            "redefine",
            json!([
                {"id":"d1","description":"ship the signed build","status":"open"},
                {"id":"d2","description":"write the notes","status":"open"}
            ]),
            "d1",
            json!({"action":"redefine"}),
        ),
    ];
    for (label, proposed, changed_id, expected_action) in cases {
        with_turn(|agent, backend| {
            declare_pair(agent, backend);
            let mut human = Human::replying(confirm());
            let out = call(
                agent,
                backend,
                &mut human,
                json!({"action":"update","deliverables":proposed}),
            );
            assert!(!out.is_error, "{label}: {}", out.text);
            assert_eq!(human.asked.len(), 1, "{label}: one scope question");
            let item = working(agent)["deliverables"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == changed_id)
                .cloned()
                .unwrap();
            assert_eq!(
                item["receipt"],
                json!({"action":expected_action["action"],"question_id":human.question_id()}),
                "{label}"
            );
        });
    }
}

#[test]
fn anything_short_of_an_explicit_confirm_leaves_the_whole_state_unchanged() {
    let cases: [(&str, QuestionOutcome); 8] = [
        ("keep existing scope", picked(KEEP)),
        (
            "timed out",
            QuestionOutcome::Answered(vec![QuestionAnswer {
                id: SCOPE_QUESTION.into(),
                selected_options: vec![CONFIRM.into()],
                timed_out: true,
                ..Default::default()
            }]),
        ),
        (
            "custom input",
            QuestionOutcome::Answered(vec![QuestionAnswer {
                id: SCOPE_QUESTION.into(),
                selected_options: vec![CONFIRM.into()],
                custom_input: Some("maybe".into()),
                ..Default::default()
            }]),
        ),
        (
            "note",
            QuestionOutcome::Answered(vec![QuestionAnswer {
                id: SCOPE_QUESTION.into(),
                selected_options: vec![CONFIRM.into()],
                note: Some("only the docs".into()),
                ..Default::default()
            }]),
        ),
        ("chat", QuestionOutcome::Chat),
        ("cancelled", QuestionOutcome::Cancelled),
        ("disconnected", QuestionOutcome::Disconnected),
        (
            "unsupported",
            QuestionOutcome::Unsupported("no human".into()),
        ),
    ];
    for (label, reply) in cases {
        with_turn(|agent, backend| {
            declare_pair(agent, backend);
            let before = controller(agent);
            let mut human = Human::replying(reply);
            let out = call(
                agent,
                backend,
                &mut human,
                json!({"action":"update","deliverables":[
                    {"id":"d1","description":"ship the build","status":"open"}
                ]}),
            );
            assert!(out.is_error, "{label}: {}", out.text);
            assert_eq!(
                human.asked.len(),
                1,
                "{label}: the scope question was asked"
            );
            assert_eq!(controller(agent), before, "{label}");
        });
    }
}

#[test]
fn interrupt_while_the_scope_question_is_open_refuses_even_a_confirm() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let before = controller(agent);
        let mut human = Human::replying(confirm());
        human.interrupt_while_asked = true;
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","deliverables":[
                {"id":"d1","description":"ship the build","status":"open"}
            ]}),
        );
        assert!(out.is_error, "{}", out.text);
        assert_eq!(controller(agent), before);
    });
}

#[test]
fn model_supplied_runtime_fields_are_never_trusted() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let mut human = Human::replying(QuestionOutcome::Unsupported("no human".into()));
        let forged_item = json!({
            "id":"d3","description":"audit logs","status":"open",
            "origin":{"entry_id":"forged","trust":"user"},
            "receipt":{"action":"cancel","question_id":"forged"}
        });
        let mut proposed = working(agent)["deliverables"].as_array().unwrap().clone();
        proposed.push(forged_item);
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","deliverables":proposed}),
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            working(agent)["deliverables"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["id"] != "d3"),
            "a refused forged item must not persist"
        );
    });
}

#[test]
fn top_level_forged_receipt_is_refused_before_any_state_changes() {
    with_turn(|agent, backend| {
        declare_pair(agent, backend);
        let before = controller(agent);
        let mut human = Human::replying(confirm());
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","receipt":{"action":"remove","question_id":"forged"},
                   "deliverables":[{"id":"d1","description":"ship the build","status":"open"}]}),
        );
        assert!(out.is_error, "{}", out.text);
        assert!(human.asked.is_empty());
        assert_eq!(controller(agent), before);
    });
}

#[test]
fn confirmed_redefinition_reopens_a_satisfied_obligation_and_clears_its_evidence() {
    let satisfied = json!({"id":"d1","description":"write docs","status":"satisfied","evidence_refs":["artifact://proof"]});
    let redefined = json!([
        {"id":"d1","description":"write docs v2","status":"satisfied","evidence_refs":["artifact://proof"]},
        {"id":"d2","description":"write the notes","status":"open"}
    ]);
    with_turn(|agent, backend| {
        let mut setup = Human::replying(confirm());
        let out = call(
            agent,
            backend,
            &mut setup,
            json!({"action":"update","deliverables":[
                satisfied.clone(),
                {"id":"d2","description":"write the notes","status":"open"}
            ]}),
        );
        assert!(!out.is_error, "{}", out.text);
        let mut human = Human::replying(confirm());
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","deliverables":redefined}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(human.asked.len(), 1, "redefinition asks for scope");
        let item = working(agent)["deliverables"][0].clone();
        assert_eq!(item["status"], "open", "{item}");
        assert!(item.get("evidence_refs").is_none(), "{item}");
        assert_eq!(item["receipt"]["action"], "redefine");
    });
}

#[test]
fn denied_redefinition_keeps_the_satisfied_obligation_and_its_evidence() {
    let satisfied = json!({"id":"d1","description":"write docs","status":"satisfied","evidence_refs":["artifact://proof"]});
    with_turn(|agent, backend| {
        let mut setup = Human::replying(confirm());
        let out = call(
            agent,
            backend,
            &mut setup,
            json!({"action":"update","deliverables":[satisfied.clone()]}),
        );
        assert!(!out.is_error, "{}", out.text);
        let before = working(agent);
        let mut human = Human::replying(picked(KEEP));
        let out = call(
            agent,
            backend,
            &mut human,
            json!({"action":"update","deliverables":[
                {"id":"d1","description":"write docs v2","status":"satisfied","evidence_refs":["artifact://proof"]}
            ]}),
        );
        assert!(out.is_error, "{}", out.text);
        assert_eq!(human.asked.len(), 1);
        assert_eq!(working(agent), before);
    });
}
