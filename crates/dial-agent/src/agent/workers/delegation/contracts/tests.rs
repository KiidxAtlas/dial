use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::agent::{
    NullUi,
    workers::contracts::{Contract, Ledger},
};

#[test]
fn malformed_or_unchecked_contract_starts_launch_nothing() {
    with_agent("", |agent| {
        for args in [
            json!({"contract":{"id":"n1"},"action":"start","task":"inspect"}),
            json!({"contract":{"id":"n1"},"action":"start","task":"inspect","acceptance_checks":[]}),
            json!({"contract":{"id":"n1"},"action":"start","task":"inspect","acceptance_checks":["unconfigured"]}),
            json!({"contract":{"id":"bad id"},"action":"start","task":"inspect","acceptance_checks":["behavior"]}),
        ] {
            assert!(agent.contract_task(&args, &mut NullUi).is_err(), "{args}");
        }
        assert!(agent.workflow_contracts.snapshot().is_null());
        assert!(agent.live_tasks.running_ids().is_empty());
    });
}

#[test]
fn plan_mode_contract_start_is_refused_without_a_journal_entry() {
    with_agent("", |agent| {
        agent.set_plan_mode(true);
        let args = json!({"contract":{"id":"n1"},"action":"start","task":"inspect","acceptance_checks":["behavior"]});
        assert!(agent.contract_task(&args, &mut NullUi).is_err());
        assert!(agent.workflow_contracts.snapshot().is_null());
    });
}

#[test]
fn restored_running_node_blocks_completion_while_ordinary_tasks_stay_unblocked() {
    with_agent("", |agent| {
        assert!(
            agent.workflow_contracts.blocker().is_none(),
            "ordinary sessions are unchanged"
        );
        let mut ledger = Ledger::default();
        let node = Contract {
            id: "n1".into(),
            depends_on: vec![],
        };
        ledger
            .begin(
                &node,
                json!({"task":"inspect","acceptance_checks":["behavior"]}),
            )
            .unwrap();
        ledger.attach("n1", "task-1").unwrap();
        agent.workflow_contracts = Ledger::restore(&ledger.snapshot());
        let blocker = agent
            .workflow_contracts
            .blocker()
            .expect("interrupted node must block");
        assert!(blocker.contains("interrupted"));
        assert!(agent.workflow_contracts.running().is_empty());
    });
}

#[test]
fn unsettled_contracts_block_plain_final_and_explicit_finish() {
    use crate::agent::{Backend, ProviderKind, TurnOutcome};
    for explicit in [false, true] {
        let mut delta = json!({"content":"The delivery is ready."});
        let reason = if explicit {
            delta["tool_calls"] = json!([{"index":0,"id":"finish","type":"function","function":{"name":"finish","arguments":json!({"outcome":"completed","summary":"The delivery is ready."}).to_string()}}]);
            "tool_calls"
        } else {
            "stop"
        };
        let response = crate::llm::fake::Script::Sse(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":delta,"finish_reason":reason}]})
        ));
        let server = crate::llm::fake::serve(vec![
            response,
            crate::llm::fake::json(200, &json!({"total_slots":1})),
        ]);
        with_agent(
            "[compaction]\nenabled=false\n[harness]\nmax_steps=2",
            |agent| {
                agent.backend = Some(Backend::new(
                    ProviderKind::Endpoint {
                        base_url: format!("{}/v1", server.url),
                        model: "test".into(),
                        api_key: None,
                    },
                    Default::default(),
                    5,
                ));
                agent
                    .workflow_contracts
                    .begin(
                        &Contract {
                            id: "pending".into(),
                            depends_on: vec![],
                        },
                        json!({"task":"deliver"}),
                    )
                    .unwrap();
                let report = agent.turn("Prepare the delivery", &mut NullUi).unwrap();
                assert_eq!(
                    report.outcome,
                    TurnOutcome::Incomplete,
                    "explicit={explicit}: {}",
                    report.text
                );
                assert!(report.text.contains("pending"), "{}", report.text);
                assert!(agent.session.custom_records("training_turn").is_empty());
            },
        );
    }
}

#[test]
fn nested_and_batch_contracts_cannot_fall_back_to_ordinary_execution() {
    with_agent("", |agent| {
        for args in [
            json!({"contract":{"id":"nested"},"task":"t","solution_space":"s"}),
            json!({"tasks":[{"contract":{"id":"batch"},"task":"t","solution_space":"s"}]}),
        ] {
            assert!(subagents::parse(&args, &agent.auth_config).is_err());
        }
    });
}

#[test]
fn contracts_live_worker_checks_persist_and_reverify_without_relaunch() {
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Inspected the candidate.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    )]);
    let overlay = format!(
        r#"
endpoint = {{base_url = "{}/v1"}}
[task]
persist = true
[compaction]
enabled = false
[codebase]
enabled = false
[verification]
automatic_checks = false
[[verification.commands]]
name = "behavior"
command = "test -f source.txt && printf '1 test passed\\n'"
timeout_secs = 5
[verification.inputs.behavior]
paths = ["source.txt"]
"#,
        server.url
    );
    with_agent(&overlay, |agent| {
        agent.approval = crate::config::types::Approval::Auto;
        std::fs::write(agent.workspace.root().join("source.txt"), "candidate").unwrap();
        let reply = agent.start_subagents(&json!({
            "action":"start", "agent":"worker", "task":"Inspect the candidate; do not edit it",
            "solution_space":"Return findings only", "max_steps":4,
            "acceptance_checks":["behavior"], "contract":{"id":"live"}
        }), &mut NullUi).unwrap();
        let worker = reply["workers"][0]["id"].as_str().unwrap().to_owned();
        let mut backend = agent.backend.take().unwrap();
        let waited = agent.run_task(
            &mut backend,
            &json!({"action":"wait","id":worker,"timeout":30}),
            &mut NullUi,
        );
        agent.backend = Some(backend);
        assert!(!waited.is_error, "{}", waited.text);
        agent.sync_contracts().unwrap();
        let accepted = agent.workflow_contracts.node("live").unwrap();
        assert_eq!(accepted["state"], "accepted", "{accepted}");
        assert!(agent.workflow_contracts.blocker().is_none());
        agent.restore_control().unwrap();
        assert!(agent.workflow_contracts.blocker().is_some());
        let verified = agent
            .contract_task(
                &json!({"action":"verify","contract":{"id":"live"}}),
                &mut NullUi,
            )
            .unwrap();
        assert_eq!(verified["state"], "accepted", "{verified}");
        assert_eq!(verified["output"], accepted["output"]);
        assert_eq!(
            verified["attempts"], 1,
            "verification cannot launch another worker"
        );
        std::fs::write(agent.workspace.root().join("source.txt"), "concurrent edit").unwrap();
        agent.sync_contracts().unwrap();
        assert!(agent.workflow_contracts.blocker().is_some());
    });
}
