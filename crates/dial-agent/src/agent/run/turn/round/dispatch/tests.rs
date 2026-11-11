//! One-call contract: a leading read-only inspection runs; later calls are skipped unexecuted.
use crate::agent::run::tool_exposure::default_offer_tests::with_review_disabled as with_agent;
use crate::agent::run::turn::completion_tests::{completion_requests, connect, response};
use crate::agent::*;

#[test]
fn custom_foreground_checks_record_real_failure_and_success_without_runner_names() {
    let server = llm::fake::serve(vec![
        response("", Some(("bash", json!({"command":"exit 7","check":true})))),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("", Some(("bash", json!({"command":"exit 0","check":true})))),
        response("The first check failed; the second passed.", None),
    ]);
    with_agent(
        "[compaction]\nenabled=false\n[sandbox]\nmode='off'\n",
        |agent| {
            connect(agent, &server.url);
            agent.turn("Run both requested diagnostic checks and report their verdicts; do not repair anything.", &mut NullUi).unwrap();
            let state = agent.session.control_state();
            assert_eq!(
                state["reliability"]["stats"]["failing_checks"], 1,
                "{state}"
            );
            assert_eq!(
                state["reliability"]["stats"]["passing_checks"], 1,
                "{state}"
            );
            // The turn report lists both shell checks with their latest verdicts.
            let checks = state["turn_summary"]["recorded_checks"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let verdicts: Vec<(&str, &str)> = checks
                .iter()
                .filter(|check| check["kind"] == "shell")
                .map(|check| {
                    (
                        check["name"].as_str().unwrap_or_default(),
                        check["status"].as_str().unwrap_or_default(),
                    )
                })
                .collect();
            assert_eq!(
                verdicts,
                [("exit 7", "failed"), ("exit 0", "passed")],
                "{state}"
            );
        },
    );
}

#[test]
fn a_fourth_large_unverified_write_is_withheld_before_mutating_the_file() {
    let versions: Vec<String> = (0..4)
        .map(|i| format!("{i}{}\n", "x".repeat(5000)))
        .collect();
    let mut scripts = Vec::new();
    for (i, text) in versions.iter().enumerate() {
        scripts.push(response(
            "",
            Some(("write", json!({"path":"encoder.txt","content":text}))),
        ));
        if i == 0 {
            scripts.push(llm::fake::json(200, &json!({"total_slots":1})));
        }
    }
    scripts.push(response(
        "The large rewrite was withheld; the previous file is retained and unverified.",
        None,
    ));
    let server = llm::fake::serve(scripts);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.turn("Write successive draft versions to encoder.txt and report what was saved; this exercise does not ask you to verify the contents.", &mut NullUi).unwrap();
        assert_eq!(
            std::fs::read_to_string(agent.workspace.root().join("encoder.txt")).unwrap(),
            versions[2]
        );
    });
    let requests = completion_requests(&server);
    let messages = requests.last().unwrap()["messages"].to_string();
    assert!(messages.contains("Large rewrite"), "{messages}");
    assert!(messages.contains("no bytes were written"), "{messages}");
}

fn calls(calls: &[(&str, Value)]) -> llm::fake::Script {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, args))| json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}))
        .collect();
    let event = json!({"choices":[{"delta":{"content":"","tool_calls":calls},"finish_reason":"tool_calls"}]});
    llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn tool_result<'a>(request: &'a Value, id: &str) -> &'a str {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == id)
        .and_then(|message| message["content"].as_str())
        .unwrap_or_else(|| panic!("no result for {id}: {request}"))
}

fn one_call_turn(batch: llm::fake::Script, answers: usize) -> (Vec<Value>, bool) {
    let slots = llm::fake::json(200, &json!({"total_slots":1}));
    // A refused batch earns a contract-repair round before the final answer.
    let mut scripts = vec![batch, slots];
    scripts.extend((0..answers).map(|_| response("Done.", None)));
    let server = llm::fake::serve(scripts);
    let written = with_agent(
        "[compaction]\nenabled=false\n[reliability]\nmeasured_interfaces=true\n",
        |agent| {
            connect(agent, &server.url);
            std::fs::write(agent.workspace.root().join("a.txt"), "alpha\n").unwrap();
            std::fs::create_dir_all(&agent.config.state_dir).unwrap();
            let record = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":crate::reliability::binding(agent.config),"batch_passed":false,"passed":1,"total":2});
            std::fs::write(
                agent.config.state_dir.join("reliability.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            agent.turn("Inspect a.txt.", &mut NullUi).unwrap();
            assert_eq!(
                agent.session.control_state()["reliability"]["call_policy"]["mode"],
                "one_call"
            );
            agent.workspace.root().join("b.txt").exists()
        },
    );
    (completion_requests(&server), written)
}

#[test]
fn one_call_mode_runs_a_leading_read_and_skips_the_rest_unexecuted() {
    let (requests, written) = one_call_turn(
        calls(&[
            ("read", json!({"path":"a.txt"})),
            ("write", json!({"path":"b.txt","content":"x\n"})),
        ]),
        1,
    );
    assert!(!written, "a skipped call never executes");
    let followup = &requests[1];
    assert!(tool_result(followup, "call-0").contains("alpha"));
    assert!(tool_result(followup, "call-1").contains("Only the first call"));
}

#[test]
fn one_call_mode_still_rejects_a_batch_led_by_an_effect() {
    let (requests, written) = one_call_turn(
        calls(&[
            ("write", json!({"path":"b.txt","content":"x\n"})),
            ("read", json!({"path":"a.txt"})),
        ]),
        2,
    );
    assert!(!written);
    let followup = &requests[1];
    for id in ["call-0", "call-1"] {
        assert!(
            tool_result(followup, id).contains("entire batch is rejected"),
            "{id}"
        );
    }
}

/// Records approval prompts and scheduler diagnostics; answers every prompt with `allow`.
struct Prompted {
    allow: bool,
    asked: Vec<String>,
    diagnostics: Vec<String>,
}

impl Ui for Prompted {
    fn approve_call(
        &mut self,
        _: &str,
        tool: &str,
        _: &str,
        _: Option<&std::sync::atomic::AtomicBool>,
    ) -> Decision {
        self.asked.push(tool.to_owned());
        if self.allow {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }
    fn diagnostic(&mut self, message: &str) {
        self.diagnostics.push(message.to_owned());
    }
}

/// One `Ask`-mode turn over `batch` (only `a.txt` exists beforehand); returns the follow-up
/// request, the prompt surface, the early-read counters and the final `b.txt`/`c.txt` contents.
fn approval_turn(
    batch: llm::fake::Script,
    allow: bool,
) -> (Value, Prompted, (u64, u64), Option<String>, Option<String>) {
    let slots = llm::fake::json(200, &json!({"total_slots":1}));
    let server = llm::fake::serve(vec![batch, slots, response("Done.", None)]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let root = agent.workspace.root().to_owned();
        std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
        agent.approval = Approval::Ask;
        let mut ui = Prompted {
            allow,
            asked: Vec::new(),
            diagnostics: Vec::new(),
        };
        agent.turn("Update b.txt and inspect.", &mut ui).unwrap();
        let scheduling = &agent.efficiency_metrics.scheduling;
        let counters = (
            scheduling.early_reads_started,
            scheduling.early_reads_reused,
        );
        let requests = completion_requests(&server);
        (
            requests[1].clone(),
            ui,
            counters,
            std::fs::read_to_string(root.join("b.txt")).ok(),
            std::fs::read_to_string(root.join("c.txt")).ok(),
        )
    })
}

#[test]
fn an_approved_barrier_overlaps_only_disjoint_later_reads_and_conflicts_rerun_after_it() {
    let (followup, ui, (started, reused), b, _) = approval_turn(
        calls(&[
            ("write", json!({"path":"b.txt","content":"new\n"})),
            ("read", json!({"path":"b.txt"})),
            ("read", json!({"path":"a.txt"})),
        ]),
        true,
    );
    assert_eq!(ui.asked, ["write"]);
    assert!(
        ui.diagnostics
            .iter()
            .any(|text| text.contains("1 later read(s) started while write awaits approval")),
        "{:?}",
        ui.diagnostics
    );
    assert_eq!(b.as_deref(), Some("new\n"));
    assert_eq!((started, reused), (1, 1), "the held disjoint read was used");
    assert!(tool_result(&followup, "call-2").contains("alpha"));
    assert!(
        tool_result(&followup, "call-1").contains("new"),
        "a conflicting read runs after the approved write"
    );
}

#[test]
fn a_denied_barrier_never_runs_and_its_held_reads_stay_valid() {
    let (followup, ui, (started, reused), b, _) = approval_turn(
        calls(&[
            ("write", json!({"path":"b.txt","content":"new\n"})),
            ("read", json!({"path":"b.txt"})),
            ("read", json!({"path":"a.txt"})),
        ]),
        false,
    );
    assert_eq!(ui.asked, ["write"]);
    assert_eq!(b, None, "a denied call never runs");
    assert_eq!((started, reused), (1, 1));
    assert!(tool_result(&followup, "call-2").contains("alpha"));
    assert!(tool_result(&followup, "call-1").contains("no such file"));
}

#[test]
fn a_native_wave_awaiting_approval_overlaps_reads_disjoint_from_every_member() {
    let (followup, ui, (started, reused), b, c) = approval_turn(
        calls(&[
            ("write", json!({"path":"b.txt","content":"new b\n"})),
            ("write", json!({"path":"c.txt","content":"new c\n"})),
            ("read", json!({"path":"c.txt"})),
            ("read", json!({"path":"a.txt"})),
        ]),
        true,
    );
    assert_eq!(ui.asked, ["write", "write"]);
    assert!(
        ui.diagnostics
            .iter()
            .any(|text| text.contains("while the native wave awaits approval")),
        "{:?}",
        ui.diagnostics
    );
    assert_eq!(
        (b.as_deref(), c.as_deref()),
        (Some("new b\n"), Some("new c\n"))
    );
    assert_eq!((started, reused), (1, 1));
    assert!(tool_result(&followup, "call-3").contains("alpha"));
    assert!(tool_result(&followup, "call-2").contains("new c"));
}

#[test]
fn calls_that_run_without_a_prompt_start_no_reads_beside_them() {
    let server = llm::fake::serve(vec![
        calls(&[
            ("write", json!({"path":"b.txt","content":"new\n"})),
            ("read", json!({"path":"b.txt"})),
            ("read", json!({"path":"a.txt"})),
        ]),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("Done.", None),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        std::fs::write(agent.workspace.root().join("a.txt"), "alpha\n").unwrap();
        agent.approval = Approval::Write;
        let mut ui = Prompted {
            allow: true,
            asked: Vec::new(),
            diagnostics: Vec::new(),
        };
        agent.turn("Update b.txt and inspect.", &mut ui).unwrap();
        assert!(ui.asked.is_empty());
        assert_eq!(agent.efficiency_metrics.scheduling.early_reads_started, 0);
    });
}
