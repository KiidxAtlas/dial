use super::*;
use crate::agent::workflow::research::Synthesis;
use serde_json::json;

fn observation(engine: &mut Engine, workspace: &Workspace, index: usize) {
    let call = crate::llm::ToolCall {
        id: format!("observation-{index}"),
        name: "read".into(),
        arguments: json!({"path":format!("source-{index}.txt")}),
        malformed_arguments: false,
    };
    let mut output = ToolOutput::ok(format!("source evidence {index}"));
    engine.settle(workspace, &call, &mut output, false, false, |_| true);
    engine.research_round();
}

#[test]
fn compaction_and_auto_continuations_keep_research_progress_but_steering_resets_it() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut engine = Engine::default();
    for index in 0..24 {
        observation(&mut engine, &workspace, index);
        engine.evidence_compacted();
        engine.transition(RecoveryTransition::AutomaticContinue);
        engine.transition(RecoveryTransition::DirectedContinue);
    }
    assert!(engine.research_required());
    let reply = crate::llm::Reply { text:json!({"finding":"New finding","evidence":["observation-23"],"next_action":"Report the limitation"}).to_string(), finish_reason:"stop".into(), ..Default::default() };
    assert!(matches!(engine.synthesize(&reply), Synthesis::Accepted));
    for index in 24..48 {
        observation(&mut engine, &workspace, index);
    }
    assert!(engine.research_required());
    engine.transition(RecoveryTransition::Steering);
    assert!(!engine.research_required());
    for index in 48..72 {
        observation(&mut engine, &workspace, index);
    }
    assert!(engine.research_required());
    engine.transition(RecoveryTransition::Coordination);
    assert!(!engine.research_required());
}

fn response(
    text: &str,
    call: Option<(usize, &str, serde_json::Value)>,
) -> crate::llm::fake::Script {
    let mut delta = json!({"content":text});
    let reason = if let Some((id, name, args)) = call {
        delta["tool_calls"] = json!([{"index":0,"id":format!("observation-{id}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]);
        "tool_calls"
    } else {
        "stop"
    };
    let event = json!({"choices":[{"delta":delta,"finish_reason":reason}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

#[test]
fn parent_research_checkpoint_prevents_tool_dispatch_and_false_completion() {
    use crate::agent::*;
    let mut scripts: Vec<_> = (0..24)
        .map(|index| {
            response(
                "",
                Some((index, "read", json!({"path":format!("source-{index}.txt")}))),
            )
        })
        .collect();
    for index in 24..27 {
        scripts.push(response(
            "",
            Some((
                index,
                "write",
                json!({"path":"must-not-exist.txt","content":"unrequested effect"}),
            )),
        ));
    }
    // The parent queries local server facts after the first response; cache a valid answer.
    scripts.insert(1, crate::llm::fake::json(200, &json!({"total_slots":1})));
    let server = llm::fake::serve(scripts);
    crate::agent::run::tool_exposure::default_offer_tests::with_agent(
        "[compaction]\nenabled=false",
        |agent| {
            for index in 0..24 {
                std::fs::write(
                    agent.workspace.root().join(format!("source-{index}.txt")),
                    format!("source evidence {index}\n"),
                )
                .unwrap();
            }
            agent.backend = Some(Backend::new(
                ProviderKind::Endpoint {
                    base_url: format!("{}/v1", server.url),
                    model: "test".into(),
                    api_key: None,
                },
                Default::default(),
                5,
            ));
            let report = agent
                .turn(
                    "Investigate the source files and report findings",
                    &mut NullUi,
                )
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
            assert!(!agent.workspace.root().join("must-not-exist.txt").exists());
        },
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 28);
}

#[test]
fn parent_research_synthesis_resumes_read_only_work_and_ordinary_completion() {
    use crate::agent::*;
    let mut scripts: Vec<_> = (0..24)
        .map(|index| {
            response(
                "",
                Some((index, "read", json!({"path":format!("source-{index}.txt")}))),
            )
        })
        .collect();
    let finding = json!({"finding":"The source files have distinct numbered evidence","evidence":["observation-23"],"next_action":"Report the source evidence and its verification limits"}).to_string();
    scripts.push(response(&finding, None));
    scripts.push(response("The 24 source files contain distinct numbered evidence. Runtime behavior was not exercised.", None));
    scripts.insert(1, crate::llm::fake::json(200, &json!({"total_slots":1})));
    let server = llm::fake::serve(scripts);
    crate::agent::run::tool_exposure::default_offer_tests::with_agent(
        "[compaction]\nenabled=false",
        |agent| {
            for index in 0..24 {
                std::fs::write(
                    agent.workspace.root().join(format!("source-{index}.txt")),
                    format!("source evidence {index}\n"),
                )
                .unwrap();
            }
            agent.backend = Some(Backend::new(
                ProviderKind::Endpoint {
                    base_url: format!("{}/v1", server.url),
                    model: "test".into(),
                    api_key: None,
                },
                Default::default(),
                5,
            ));
            let report = agent
                .turn(
                    "Investigate the source files and report findings",
                    &mut NullUi,
                )
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
            assert_ne!(report.text, finding, "a checkpoint is not the final answer");
        },
    );
    assert_eq!(server.requests().len(), 27);
}
