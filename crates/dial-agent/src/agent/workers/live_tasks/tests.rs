use super::*;

#[test]
fn consistent_ux_worker_projection_keeps_findings_and_typed_state_without_assignment_noise() {
    let value = json!([{"id":"task-1","name":"Parser scout","status":"done","steps":3,"result":"The parser drops the final line.","artifact":"artifact://report","assignment":{"task":"Inspect parser","context":"Repeated assignment"},"portable_rules":{"regenerations":0}}]);
    let result = output(Ok(value.clone()));
    assert_eq!(result.data, Some(value));
    assert!(
        result.text.contains("task-1: done")
            && result.text.contains("drops the final line")
            && result.text.contains("artifact://report")
    );
    assert!(!result.text.contains("assignment") && !result.text.contains("portable_rules"));
}
#[test]
fn rebinding_cancels_old_workers_without_projecting_them_into_the_new_session() {
    #[derive(Default)]
    struct View {
        results: usize,
    }
    impl Ui for View {
        fn result_node(&mut self, _: &crate::runtime::Body) {
            self.results += 1;
        }
    }
    let mut tasks = LiveTasks::default();
    let old = tasks.tree.start("worker", "old session", None);
    old.settle(
        String::new(),
        Some(json!({"id":"task-1","status":"running"})),
        crate::runtime::Phase::Running,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    tasks.jobs.insert(
        "task-1".into(),
        JobState {
            body: old.clone(),
            interrupt: cancel.clone(),
            worker: None,
            active_tool: Arc::new(Mutex::new(None)),
            delivered: false,
        },
    );
    let new = crate::runtime::Tree::default();
    let current = new.start("worker", "new session", None);
    assert_eq!(old.read(|n| n.id.clone()), current.read(|n| n.id.clone()));
    tasks
        .completed
        .push(json!({"id":"previous-result","status":"done"}));
    tasks.bind(new.clone());
    assert!(tasks.undelivered().is_empty());
    assert!(cancel.load(Ordering::SeqCst));
    assert_eq!(
        old.read(|n| n.result.phase),
        crate::runtime::Phase::Interrupted
    );
    assert!(!new.owns(&old));
    tasks
        .tx
        .send(Event::Done(
            "task-1".into(),
            json!({"id":"task-1","status":"cancelled","result":"interrupted"}),
        ))
        .unwrap();
    let mut view = View::default();
    tasks.pump(
        Approval::Ask,
        false,
        &mut Vec::new(),
        std::path::Path::new("."),
        &[],
        None,
        &mut None,
        &mut view,
    );
    assert_eq!(view.results, 0);
    assert!(
        tasks.undelivered().is_empty(),
        "old-session results cannot enter the new parent's request"
    );
    assert_eq!(
        current.read(|n| n.result.phase),
        crate::runtime::Phase::Running
    );
}

#[test]
fn finished_results_are_delivered_once_through_any_model_view() {
    let mut tasks = LiveTasks::default();
    tasks
        .completed
        .push(json!({"id":"task-1","status":"completed","result":"found it"}));
    tasks
        .completed
        .push(json!({"id":"task-2","status":"completed","result":"also"}));
    // A `wait` or `status` reply that already carries task-1 consumes its queued result.
    tasks.acknowledge(&json!({"id":"task-1","status":"completed","result":"found it"}));
    assert_eq!(tasks.undelivered().len(), 1);
    assert_eq!(tasks.undelivered()[0]["id"], "task-2");
    // A running job in a reply is not a delivered result.
    tasks.acknowledge(&json!([{"id":"task-2","status":"running"}]));
    assert_eq!(tasks.undelivered().len(), 1);
    tasks.deliver("task-2");
    assert!(tasks.undelivered().is_empty());
    // Delivering again is a no-op: the result never re-enters a request.
    tasks.deliver("task-2");
    assert!(tasks.undelivered().is_empty());
}

#[test]
fn immediate_preview_surfaces_trailing_blockers_before_narrative() {
    let report = format!(
        "{}BLOCKER: migration needs approval",
        "ordinary detail ".repeat(2000)
    );
    let value = json!({"id":"task-1","status":"done","result":report,"artifact":"artifact://full", "assignment":{"work_packet":{"acceptance":["migration verified"]}}});
    let result = output(Ok(value.clone()));
    assert_eq!(result.data, Some(value));
    assert!(
        result.text.find("migration needs approval").unwrap()
            < result.text.find("ordinary detail").unwrap()
    );
    assert!(result.text.contains("not_verified"));
    assert!(result.text.contains("artifact://full"));
}

#[test]
fn completion_is_visible_before_pumping_and_an_early_ack_is_not_redelivered() {
    let mut tasks = LiveTasks::default();
    let body = tasks.tree.start("worker", "published result", None);
    body.settle(
        String::new(),
        Some(json!({"id":"task-1","status":"running","assignment":{"task":"inspect"},"started_ms":12})),
        crate::runtime::Phase::Running,
    );
    tasks.jobs.insert(
        "task-1".into(),
        JobState {
            body: body.clone(),
            interrupt: Arc::new(AtomicBool::new(false)),
            worker: None,
            active_tool: Arc::default(),
            delivered: false,
        },
    );
    let value = json!({"id":"task-1","status":"failed","result":"retention failed","data":{"dial_effects":"unknown"}});
    settle_result(&body, &value);
    let status = tasks.status(Some("task-1")).unwrap();
    assert_eq!(status["status"], "failed");
    assert_eq!(status["assignment"]["task"], "inspect");
    assert_eq!(status["started_ms"], 12);
    assert_eq!(status["data"]["dial_effects"], "unknown");
    assert!(tasks.running_ids().is_empty());
    tasks.acknowledge(&status);
    tasks.tx.send(Event::Done("task-1".into(), value)).unwrap();
    struct View;
    impl Ui for View {}
    tasks.pump(
        Approval::Ask,
        false,
        &mut Vec::new(),
        std::path::Path::new("."),
        &[],
        None,
        &mut None,
        &mut View,
    );
    assert!(tasks.undelivered().is_empty());
    assert_eq!(
        body.read(|node| node.result.phase),
        crate::runtime::Phase::Failed
    );
}

#[test]
fn transcript_persistence_policy_applies_while_live_and_after_every_terminal_phase() {
    use crate::agent::workers::child_transcript::{messages, scan};
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let historic = create_transcript(
        true,
        Some(&layout),
        "parent",
        "historic",
        json!({"id":"historic"}),
    )
    .unwrap()
    .unwrap();
    historic
        .record(&json!({"role":"assistant","content":"historic answer"}))
        .unwrap();
    historic.finish(json!({"status":"done"})).unwrap();
    let historic_path = scan(&layout, "parent")[0].path.clone();
    let historic_bytes = std::fs::read(&historic_path).unwrap();
    for persist in [false, true] {
        for status in ["done", "cancelled", "failed", "incomplete"] {
            let id = format!("{persist}-{status}");
            let sink = create_transcript(
                persist,
                Some(&layout),
                "parent",
                &id,
                json!({"id":id,"status":"running"}),
            )
            .unwrap();
            assert_eq!(sink.is_some(), persist);
            let retained = scan(&layout, "parent")
                .into_iter()
                .find(|entry| entry.header["id"] == id);
            assert_eq!(
                retained.is_some(),
                persist,
                "live capture follows the policy"
            );
            if let Some(sink) = &sink {
                sink.record(&json!({"role":"assistant","content":"candidate answer"}))
                    .unwrap();
                sink.finish(json!({"status":status})).unwrap();
            }
            // Durable capture must not control the worker UI's runtime history.
            let tree = crate::runtime::Tree::default();
            let body = tree.start("worker", &id, None);
            let value =
                json!({"id":id,"status":status,"result":"candidate answer","data":{"candidate":7}});
            settle_result(&body, &value);
            assert_eq!(body_status(&body)["data"]["candidate"], 7);
            assert_eq!(
                body.read(|node| node.result.phase),
                crate::runtime::Phase::from_status(status)
            );
            drop(sink);
            let retained = scan(&layout, "parent")
                .into_iter()
                .find(|entry| entry.header["id"] == id);
            assert_eq!(
                retained.is_some(),
                persist,
                "terminal capture follows the policy"
            );
            if let Some(retained) = retained {
                assert_eq!(retained.status.unwrap()["status"], status);
                assert_eq!(
                    messages(&retained.path).unwrap()[0]["content"],
                    "candidate answer"
                );
            }
            assert_eq!(std::fs::read(&historic_path).unwrap(), historic_bytes);
        }
    }
    assert!(
        create_transcript(true, None, "parent", "no-storage", json!({}))
            .unwrap()
            .is_none()
    );
}

#[test]
fn isolated_conflict_publishes_incomplete_and_preserves_candidate_and_parent_bytes() {
    struct NoTools;
    impl Approver for NoTools {
        fn authorize(&mut self, _: &ToolCall, _: &str, _: bool) -> Result<(), Denial> {
            panic!("this test does not execute tools")
        }
    }
    for conflict in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let parent_path = project.path().join("source.txt");
        std::fs::write(&parent_path, b"original\0bytes").unwrap();
        let workspace = Workspace::new(project.path()).unwrap();
        let mut config = Config::default();
        config.isolation.backend = crate::config::isolation::Preference::Copy;
        let request = subagents::parse(&json!({"agent":"worker","task":"edit source","solution_space":"source only","isolation":"copy"}), &config).unwrap();
        let registry = Registry::new();
        let harness = Harness::default();
        let interrupt = Arc::new(AtomicBool::new(false));
        let env = Env {
            config: &config,
            workspace: &workspace,
            ext: &registry,
            layout: None,
            harness: &harness,
            interrupt: &interrupt,
            usage: None,
            context_cap: None,
            memory: None,
            plan_mode: false,
            agents_messages: &[],
        };
        let candidate = json!({"answer":"candidate", "count":7});
        let outcome = subagents::run_job(env, &request.jobs[0], &mut NoTools, |env, _| {
            std::fs::write(env.workspace.root().join("source.txt"), b"worker\0bytes").unwrap();
            if conflict {
                std::fs::write(&parent_path, b"parent\0bytes").unwrap();
            }
            let mut outcome = Outcome::incomplete(3, "unused");
            outcome.status = subagents::Status::done(candidate.to_string());
            outcome.data = Some(candidate.clone());
            outcome
        });
        let expected = if conflict { "incomplete" } else { "done" };
        assert_eq!(outcome.status.wire(), expected);
        assert_eq!(outcome.data.as_ref(), Some(&candidate));
        let isolation = outcome.isolation.as_ref().unwrap();
        assert!(isolation.errors.is_empty());
        assert_eq!(isolation.conflicts.is_empty(), !conflict);
        assert_eq!(
            std::fs::read(&parent_path).unwrap(),
            if conflict {
                b"parent\0bytes"
            } else {
                b"worker\0bytes"
            }
        );
        if conflict {
            assert!(outcome.status.text.contains("computation completed"));
            assert!(outcome.status.text.contains("integration is incomplete"));
            let recovery = isolation.kept.as_ref().unwrap();
            assert_eq!(
                std::fs::read(recovery.join("source.txt")).unwrap(),
                b"worker\0bytes"
            );
        }
        // The live publication boundary must not promote a candidate result to success.
        let mut tasks = LiveTasks::default();
        let body = tasks.tree.start("worker", "isolated", None);
        tasks.jobs.insert(
            "task-1".into(),
            JobState {
                body: body.clone(),
                interrupt: interrupt.clone(),
                worker: None,
                active_tool: Arc::default(),
                delivered: false,
            },
        );
        let value = json!({"id":"task-1","status":outcome.status.wire(),"result":outcome.status.text,"data":outcome.data,"isolation":isolation});
        settle_result(&body, &value);
        let published = tasks.status(Some("task-1")).unwrap();
        assert_eq!(published["status"], expected);
        assert_eq!(published["data"], candidate);
        assert_eq!(body.read(|node| node.result.phase), outcome.status.phase);
        if conflict {
            assert_eq!(published["isolation"]["conflicts"][0], "source.txt");
            assert!(published["isolation"]["recovery_location"].is_string());
            std::fs::remove_dir_all(isolation.kept.as_ref().unwrap()).unwrap();
        }
    }
}

#[test]
fn later_lease_failure_preserves_the_earlier_launched_job_in_batch_reply() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let workspace = Workspace::new(project.path()).unwrap();
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"finished\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    )]);
    let mut config = Config::default();
    config.task.persist = true;
    config.model = "test".into();
    config.endpoint =
        Some(serde_json::from_value(json!({"base_url":format!("{}/v1", server.url)})).unwrap());
    config.compaction.enabled = false;
    config.codebase.enabled = false;
    let ext = Registry::new();
    let harness = Harness::default();
    let _second_lease = super::super::worker_resume::lease(&layout, "parent", "task-2").unwrap();
    let mut tasks = LiveTasks::default();
    let reply = tasks
        .start(
            StartContext {
                config: &config,
                workspace: &workspace,
                ext: &ext,
                layout: Some(&layout),
                session_id: "parent",
                harness: &harness,
                usage: None,
                context_cap: None,
                memory: None,
                plan_mode: false,
                agents_messages: &[],
                handoff: subagents::Handoff::capture(&json!({}), &[]),
            },
            &json!({"tasks":[
                {"agent":"worker","task":"inspect one","solution_space":"fixture"},
                {"agent":"worker","task":"inspect two","solution_space":"fixture"}
            ]}),
        )
        .expect("a per-job lease failure cannot discard the batch reply");
    assert_eq!(reply[0]["id"], "task-1");
    assert_eq!(reply[0]["status"], "running");
    assert!(tasks.jobs.contains_key("task-1"));
    assert_eq!(reply[1]["id"], "task-2");
    assert_eq!(reply[1]["status"], "failed");
    assert_eq!(reply[1]["data"]["dial_effects"], "none");
    assert!(
        reply[1]["result"]
            .as_str()
            .unwrap()
            .contains("worker is still owned")
    );
    assert!(!tasks.jobs.contains_key("task-2"));
    let projected = output(Ok(reply));
    assert!(!projected.is_error, "the batch operation itself succeeded");
    assert!(projected.text.contains("task-1"));
    assert!(projected.text.contains("task-2"));
    assert!(projected.text.contains("worker is still owned"));
    assert_eq!(projected.data.as_ref().unwrap()[1]["status"], "failed");
}

#[test]
fn persistence_errors_are_explicit_bounded_and_separate_from_long_candidate_report() {
    let value = json!({"id":"task-1","status":"failed","steps":2,
        "result":format!("Candidate report: {}", "candidate detail ".repeat(2000)),
        "artifact_error":format!("artifact disk full {}", "x".repeat(4000)),
        "transcript_error":"transcript flush permission denied"});
    let result = output(Ok(value.clone()));
    assert_eq!(result.data, Some(value));
    assert!(
        result
            .text
            .contains("Worker report retention failed: artifact disk full")
    );
    assert!(
        result
            .text
            .contains("Worker transcript persistence failed: transcript flush permission denied")
    );
    assert!(
        result.text.find("artifact disk full").unwrap()
            < result.text.find("Candidate report:").unwrap()
    );
    assert!(result.text.contains("candidate detail"));
    assert!(!result.text.contains(&"x".repeat(1001)));
    assert!(!result.text.contains("Complete worker report:"));
}

#[test]
fn worker_policy_provenance_keeps_deny_before_readonly_and_cancel_before_deny() {
    #[derive(Default)]
    struct View(Vec<approval::PolicyExplanation>);
    impl Ui for View {
        fn policy_explanation(
            &mut self,
            _: &str,
            _: &str,
            explanation: &approval::PolicyExplanation,
        ) {
            self.0.push(explanation.clone());
        }
    }
    for cancelled in [false, true] {
        let mut tasks = LiveTasks::default();
        let (answer, reply) = mpsc::channel();
        tasks
            .tx
            .send(Event::Approve {
                call: ToolCall {
                    id: "worker-call".into(),
                    name: "write".into(),
                    arguments: json!({"path":"file", "content":"data"}),
                    malformed_arguments: false,
                },
                summary: "write".into(),
                needs: true,
                cancel: Arc::new(AtomicBool::new(cancelled)),
                interrupt: Arc::new(AtomicBool::new(false)),
                answer,
            })
            .unwrap();
        let mut rules = vec![approval::PermissionRule {
            effect: approval::Effect::Deny,
            tool: "write".into(),
            command_prefix: vec![],
            paths: vec![],
        }];
        let mut view = View::default();
        tasks.pump(
            Approval::Auto,
            true,
            &mut rules,
            std::path::Path::new("."),
            &[],
            None,
            &mut None,
            &mut view,
        );
        let result = reply.recv().unwrap();
        if cancelled {
            assert!(matches!(result, Err(Denial::User)));
            assert!(
                view.0
                    .iter()
                    .all(|e| e.reason == approval::PolicyReason::Cancelled)
            );
        } else {
            assert!(matches!(result, Err(Denial::Rule)));
            assert_eq!(view.0.len(), 1);
            assert_eq!(view.0[0].reason, approval::PolicyReason::DenyRule);
            assert!(view.0[0].rule_scope.is_some());
        }
        assert_eq!(rules.len(), 1);
    }
}
