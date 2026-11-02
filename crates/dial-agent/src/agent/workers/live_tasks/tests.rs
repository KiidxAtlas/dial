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
fn running_status_keeps_lifecycle_without_an_empty_result_projection() {
    let value = json!({"id":"task-1","status":"running","phase":"tool: read","steps":2});
    let rendered = output(Ok(value.clone()));
    assert_eq!(rendered.data, Some(value));
    assert_eq!(
        rendered.text,
        "Worker task-1: running (2 requests); tool: read"
    );
}

#[test]
fn polling_reports_are_compact_only_after_delivery_and_remain_recoverable() {
    let mut tasks = LiveTasks::default();
    let body = tasks.tree.start("worker", "report", None);
    let report = "distinct narrative\nBLOCKER: confirm migration approval";
    let value = json!({"id":"task-1","status":"done","result":report,"artifact":"artifact://full",
        "isolation":{"conflicts":["src/changed.rs"]},"steps":3});
    body.settle(
        report.into(),
        Some(value.clone()),
        crate::runtime::Phase::from_status("done"),
    );
    tasks.jobs.insert(
        "task-1".into(),
        JobState {
            control: None,
            body: body.clone(),
            interrupt: Arc::new(AtomicBool::new(false)),
            worker: None,
            active_tool: Arc::default(),
            delivered: false,
        },
    );
    let first = tasks.polling_output(tasks.status(None));
    assert!(first.text.contains("distinct narrative"));
    let repeated = tasks.polling_output(tasks.status(None));
    assert_eq!(repeated.data, first.data, "typed reports are never reduced");
    assert!(!repeated.text.contains("distinct narrative"));
    for required in [
        "already delivered",
        "task-1: done",
        "confirm migration approval",
        "src/changed.rs",
        "artifact://full",
    ] {
        assert!(
            repeated.text.contains(required),
            "missing {required}: {}",
            repeated.text
        );
    }
    let recovered = output(tasks.status(Some("task-1")));
    assert!(recovered.text.contains("distinct narrative"));
    assert_eq!(body_status(&body), value);
    // A different report with the same identity is not mistaken for the delivered result.
    let mut changed = value.clone();
    changed["result"] = json!("new narrative");
    assert!(
        tasks
            .polling_output(Ok(changed))
            .text
            .contains("new narrative")
    );
}

#[test]
fn polling_never_elides_failures_or_unretained_reports() {
    for (status, artifact) in [("failed", Some("artifact://full")), ("done", None)] {
        let mut tasks = LiveTasks::default();
        let body = tasks.tree.start("worker", "report", None);
        let mut value = json!({"id":"task-1","status":status,"result":"important narrative"});
        if let Some(artifact) = artifact {
            value["artifact"] = json!(artifact);
        }
        body.settle(
            "important narrative".into(),
            Some(value.clone()),
            crate::runtime::Phase::from_status(status),
        );
        tasks.jobs.insert(
            "task-1".into(),
            JobState {
                control: None,
                body,
                interrupt: Arc::new(AtomicBool::new(false)),
                worker: None,
                active_tool: Arc::default(),
                delivered: true,
            },
        );
        assert!(
            tasks
                .polling_output(Ok(value))
                .text
                .contains("important narrative"),
            "{status}"
        );
    }
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
            control: None,
            body: old.clone(),
            interrupt: cancel.clone(),
            worker: None,
            active_tool: Arc::default(),
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
        PumpPolicy {
            read_only: false,
            rules: &mut Vec::new(),
            root: std::path::Path::new("."),
            extra_roots: &[],
        },
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
            control: None,
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
        PumpPolicy {
            read_only: false,
            rules: &mut Vec::new(),
            root: std::path::Path::new("."),
            extra_roots: &[],
        },
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
        let candidate = json!({"answer":"candidate", "count":7,
            "acceptance":{"state":"accepted"}, "evidence_refs":["artifact://obsolete-or-unrelated"]});
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
        assert_ne!(
            outcome.acceptance.state,
            crate::agent::verify::worker_acceptance::State::Accepted,
            "worker claims and references never grant controller acceptance"
        );
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
                control: None,
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
    for (cancelled, provenance) in [
        (false, approval::CallProvenance::Structured),
        (true, approval::CallProvenance::Structured),
        (false, approval::CallProvenance::RecoveredText),
        (true, approval::CallProvenance::RecoveredText),
    ] {
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
                provenance,
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
            PumpPolicy {
                read_only: true,
                rules: &mut rules,
                root: std::path::Path::new("."),
                extra_roots: &[],
            },
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

#[test]
fn a_worker_message_to_the_parent_ends_a_wait_early_and_stays_queued() {
    struct View;
    impl Ui for View {}
    let mut tasks = LiveTasks::default();
    let body = tasks.tree.start("worker", "still working", None);
    body.settle(
        String::new(),
        Some(json!({"id":"task-1","status":"running"})),
        crate::runtime::Phase::Running,
    );
    tasks.jobs.insert(
        "task-1".into(),
        JobState {
            control: None,
            body,
            interrupt: Arc::new(AtomicBool::new(false)),
            worker: None,
            active_tool: Arc::default(),
            delivered: false,
        },
    );
    tasks
        .team
        .enlist("task-1", "scout", Box::new(|| {}))
        .unwrap();
    tasks
        .team
        .send("task-1", mailbox::PARENT, "found it")
        .unwrap();
    let started = Instant::now();
    let (status, stopped) = tasks
        .wait(
            Some("task-1"),
            30,
            ApprovalGate {
                approval: Approval::Ask,
                read_only: false,
                rules: &mut Vec::new(),
                root: std::path::Path::new("."),
                extra_roots: &[],
                layout: None,
                cost: &mut None,
            },
            &AtomicBool::new(false),
            &mut View,
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(stopped, Some(WaitStop::Message));
    assert_eq!(status["status"], "running", "the worker keeps running");
    // The wait only observes: the parent's next pump delivers the letter.
    assert!(tasks.team.has_mail(mailbox::PARENT));
    assert_eq!(tasks.take_parent_messages()[0].text, "found it");
}

#[test]
fn event_wait_wakes_for_a_worker_terminal_result_without_delivering_it_twice() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    struct Release(mpsc::Sender<()>);
    impl Ui for Release {
        fn notice(&mut self, text: &str) {
            if text.starts_with("Waiting for running work without model polling:") {
                self.0.send(()).unwrap();
            }
        }
    }
    with_agent("", |agent| {
        agent.live_tasks.bind(agent.session.runtime());
        let body = agent.live_tasks.tree.start("worker", "event wait", None);
        body.settle(
            String::new(),
            Some(json!({"id":"task-1","status":"running"})),
            crate::runtime::Phase::Running,
        );
        agent.live_tasks.jobs.insert(
            "task-1".into(),
            JobState {
                control: None,
                body,
                interrupt: Arc::new(AtomicBool::new(false)),
                worker: None,
                active_tool: Arc::default(),
                delivered: false,
            },
        );
        let (send, receive) = mpsc::channel();
        let tx = agent.live_tasks.tx.clone();
        let worker = std::thread::spawn(move || {
            receive.recv_timeout(Duration::from_secs(3)).unwrap();
            tx.send(Event::Done(
                "task-1".into(),
                json!({"id":"task-1","status":"failed","result":"deterministic worker failure"}),
            ))
            .unwrap();
            crate::platform::wake::signal();
        });
        assert!(agent.wait_for_background_event(&mut Release(send)).unwrap());
        worker.join().unwrap();
        assert!(agent.live_tasks.running_ids().is_empty());
        assert_eq!(agent.live_tasks.undelivered().len(), 1);
        agent.deliver_worker_results().unwrap();
        agent.deliver_worker_results().unwrap();
        assert_eq!(
            agent
                .session
                .messages()
                .iter()
                .filter(|message| message["dial_worker_result"] == "task-1")
                .count(),
            1
        );
        assert!(
            !agent
                .wait_for_background_event(&mut crate::agent::NullUi)
                .unwrap()
        );
    });
}

fn running_job(tasks: &mut LiveTasks, id: &str, data: Value) -> crate::runtime::Body {
    let body = tasks.tree.start("worker", "watched", None);
    body.settle(String::new(), Some(data), crate::runtime::Phase::Running);
    tasks.jobs.insert(
        id.into(),
        JobState {
            control: None,
            body: body.clone(),
            interrupt: Arc::new(AtomicBool::new(false)),
            worker: None,
            active_tool: Arc::default(),
            delivered: false,
        },
    );
    body
}

#[test]
fn health_separates_active_slow_and_stalled_workers_from_the_last_progress_stamp() {
    let status = |started: u64, updated: Option<u64>| {
        let mut value = json!({"status":"running","started_ms":started});
        if let Some(updated) = updated {
            value["updated_ms"] = json!(updated);
        }
        value
    };
    let now = 10_000_000;
    // Progress a moment ago: active, however long it has run before the slow limit.
    assert_eq!(
        health(&status(now - 100_000, Some(now - 5_000)), now, 300),
        Some((100, 5, "active"))
    );
    // Running past three stall periods but still progressing: slow.
    assert_eq!(
        health(&status(now - 900_000, Some(now - 5_000)), now, 300),
        Some((900, 5, "slow"))
    );
    // Silent for the stall limit: stalled, measured from the start when nothing was ever stamped.
    assert_eq!(
        health(&status(now - 400_000, None), now, 300),
        Some((400, 400, "stalled"))
    );
    assert_eq!(
        health(&status(now - 400_000, Some(now - 301_000)), now, 300),
        Some((400, 301, "stalled"))
    );
    // Disabled, settled and unstamped jobs carry no health.
    assert_eq!(health(&status(now - 400_000, None), now, 0), None);
    assert_eq!(
        health(&json!({"status":"done","started_ms":1}), now, 300),
        None
    );
    assert_eq!(health(&json!({"status":"running"}), now, 300), None);
}

#[test]
fn status_reports_health_for_running_workers_only() {
    let mut tasks = LiveTasks::default();
    let now = epoch_ms();
    running_job(
        &mut tasks,
        "task-1",
        json!({"id":"task-1","status":"running","started_ms":now.saturating_sub(1_000_000)}),
    );
    running_job(
        &mut tasks,
        "task-2",
        json!({"id":"task-2","status":"running","started_ms":now,"updated_ms":now}),
    );
    let all = tasks.status(None).unwrap();
    assert_eq!(all[0]["health"], "stalled");
    assert!(all[0]["idle_secs"].as_u64().unwrap() >= 999);
    assert_eq!(all[1]["health"], "active");
    assert_eq!(tasks.status(Some("task-1")).unwrap()["health"], "stalled");
    // The rendered line the model reads names the problem.
    let text = model_summary(&tasks.status(Some("task-1")).unwrap());
    assert!(text.contains("stalled, idle"), "{text}");
    assert!(!model_summary(&all[1]).contains("idle"));
    // The raw body is untouched: health is computed at read time.
    assert!(tasks.jobs["task-1"].status().get("health").is_none());
    tasks.stall_secs = 0;
    assert!(
        tasks
            .status(Some("task-1"))
            .unwrap()
            .get("health")
            .is_none()
    );
}

#[test]
fn a_stall_is_reported_once_until_the_worker_makes_progress_again() {
    let mut tasks = LiveTasks::default();
    let now = epoch_ms();
    let body = running_job(
        &mut tasks,
        "task-1",
        json!({"id":"task-1","status":"running","started_ms":now.saturating_sub(1_000_000)}),
    );
    assert_eq!(tasks.scan_stalls(now), vec!["task-1".to_string()]);
    assert!(tasks.scan_stalls(now).is_empty(), "already reported");
    assert!(!tasks.take_stall_alert(Some("task-2")));
    assert!(tasks.take_stall_alert(Some("task-1")));
    assert!(!tasks.take_stall_alert(None), "a wait reports it once");
    // Progress stamps the body; the next scan forgets the stall, so a later one is new.
    body.progress("generating", 1);
    assert!(tasks.scan_stalls(epoch_ms()).is_empty());
    assert!(tasks.flagged.is_empty());
    assert_eq!(
        tasks.scan_stalls(epoch_ms() + 301_000),
        vec!["task-1".to_string()]
    );
}

#[test]
fn a_wait_ends_early_when_a_worker_stalls_and_leaves_it_running() {
    struct View;
    impl Ui for View {}
    let mut tasks = LiveTasks::default();
    let now = epoch_ms();
    running_job(
        &mut tasks,
        "task-1",
        json!({"id":"task-1","status":"running","started_ms":now.saturating_sub(1_000_000)}),
    );
    let started = Instant::now();
    let (status, stopped) = tasks
        .wait(
            Some("task-1"),
            30,
            ApprovalGate {
                approval: Approval::Ask,
                read_only: false,
                rules: &mut Vec::new(),
                root: std::path::Path::new("."),
                extra_roots: &[],
                layout: None,
                cost: &mut None,
            },
            &AtomicBool::new(false),
            &mut View,
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(stopped, Some(WaitStop::Stalled));
    assert_eq!(status["status"], "running", "nothing is cancelled");
    assert_eq!(status["health"], "stalled");
    assert!(WaitStop::Stalled.note().contains("no progress"));
}

#[test]
fn contracted_candidate_without_declared_inputs_stays_unmerged_and_unverified() {
    struct NoExecution;
    impl Approver for NoExecution {
        fn authorize(&mut self, _: &ToolCall, _: &str, _: bool) -> Result<(), Denial> {
            panic!("missing input contract cannot reach execution authorization")
        }
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("source.txt"), "original").unwrap();
    let workspace = Workspace::new(project.path()).unwrap();
    let mut config = Config::default();
    config.isolation.backend = crate::config::isolation::Preference::Copy;
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "behavior".into(),
            command: "configured-check".into(),
            file_extensions: vec![],
            timeout_secs: 10,
        });
    let request = subagents::parse(&json!({"agent":"worker","task":"edit","solution_space":"s","acceptance_checks":["behavior"]}), &config).unwrap();
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
    let outcome = subagents::run_job(env, &request.jobs[0], &mut NoExecution, |env, _| {
        std::fs::write(env.workspace.root().join("source.txt"), "candidate").unwrap();
        let mut result = Outcome::incomplete(1, "unused");
        result.status = subagents::Status::done("all checks passed; artifact://unrelated".into());
        result
    });
    assert_eq!(
        outcome.status.wire(),
        "done",
        "execution completed independently of acceptance"
    );
    assert_eq!(
        outcome.acceptance.state,
        crate::agent::verify::worker_acceptance::State::NeedsVerification
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("source.txt")).unwrap(),
        "original"
    );
    let isolation = outcome.isolation.unwrap();
    assert!(!isolation.integrated);
    let kept = isolation.kept.expect("unaccepted candidate retained");
    assert_eq!(
        std::fs::read_to_string(kept.join("source.txt")).unwrap(),
        "candidate"
    );
    std::fs::remove_dir_all(kept).unwrap();
}

#[test]
fn status_polling_does_not_revalidate_or_mutate_acceptance() {
    use crate::agent::verify::worker_acceptance::{Acceptance, State};
    let root = tempfile::tempdir().unwrap();
    let tree = crate::runtime::Tree::default();
    let body = tree.start("worker", "candidate", None);
    body.settle(
        "done".into(),
        Some(json!({"status":"completed"})),
        crate::runtime::Phase::from_status("done"),
    );
    let mut acceptance = Acceptance::unverified("snapshot at explicit verification boundary");
    acceptance.state = State::Accepted;
    let job = ControlJob {
        interrupt: Arc::new(AtomicBool::new(false)),
        active_tool: Default::default(),
        body,
        finished: AtomicBool::new(true),
        base: Config::default(),
        settings: Default::default(),
        workspace: Workspace::new(root.path()).unwrap(),
        acceptance: Mutex::new(Some(acceptance)),
        owner_shutdown: AtomicBool::new(false),
    };
    let before = job_status(&job);
    for _ in 0..3 {
        assert_eq!(job_status(&job), before);
        assert_eq!(
            job.acceptance.lock().unwrap().as_ref().unwrap().state,
            State::Accepted
        );
    }
}

#[test]
fn recovered_safe_reads_prompt_through_the_live_approval_owner() {
    struct View(usize);
    impl Ui for View {
        fn approve(&mut self, _: &str, _: &str) -> crate::agent::Decision {
            self.0 += 1;
            crate::agent::Decision::AllowAlways
        }
    }
    let mut tasks = LiveTasks::default();
    let mut rules = vec![];
    let mut view = View(0);
    for provenance in [
        approval::CallProvenance::Structured,
        approval::CallProvenance::RecoveredText,
    ] {
        let (answer, reply) = mpsc::channel();
        tasks
            .tx
            .send(Event::Approve {
                call: ToolCall {
                    id: "read".into(),
                    name: "read".into(),
                    arguments: json!({"path":"file"}),
                    malformed_arguments: false,
                },
                summary: "read".into(),
                needs: false,
                provenance,
                cancel: Arc::new(AtomicBool::new(false)),
                interrupt: Arc::new(AtomicBool::new(false)),
                answer,
            })
            .unwrap();
        tasks.pump(
            Approval::Auto,
            PumpPolicy {
                read_only: true,
                rules: &mut rules,
                root: std::path::Path::new("."),
                extra_roots: &[],
            },
            None,
            &mut None,
            &mut view,
        );
        assert!(reply.recv().unwrap().is_ok());
        assert_eq!(
            view.0,
            usize::from(provenance == approval::CallProvenance::RecoveredText)
        );
        assert!(
            rules.is_empty(),
            "a recovered live call must not create grants"
        );
    }
}
