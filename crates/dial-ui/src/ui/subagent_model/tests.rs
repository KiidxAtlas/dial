use super::*;
use crate::agent::workers::child_transcript::ChildTranscript;

fn entry(id: &str, parent: Option<&str>, status: AgentStatus, last: Option<u64>) -> AgentEntry {
    AgentEntry {
        id: id.into(),
        name: id.into(),
        parent: parent.map(str::to_owned),
        status,
        last_activity_ms: last,
        ..AgentEntry::default()
    }
}

#[test]
fn status_mapping_covers_every_phase() {
    assert_eq!(
        AgentStatus::from_phase(Phase::Running),
        AgentStatus::Running
    );
    assert_eq!(
        AgentStatus::from_phase(Phase::Blocked),
        AgentStatus::Waiting
    );
    assert_eq!(
        AgentStatus::from_phase(Phase::NeedsInput),
        AgentStatus::Waiting
    );
    assert_eq!(AgentStatus::from_phase(Phase::Completed), AgentStatus::Done);
    assert_eq!(AgentStatus::from_phase(Phase::Failed), AgentStatus::Failed);
    assert_eq!(
        AgentStatus::from_phase(Phase::Incomplete),
        AgentStatus::Failed
    );
    assert_eq!(
        AgentStatus::from_phase(Phase::Interrupted),
        AgentStatus::Cancelled
    );
    assert_eq!(AgentStatus::from_wire("done"), AgentStatus::Done);
    assert_eq!(AgentStatus::from_wire("cancelled"), AgentStatus::Cancelled);
}

#[test]
fn sort_puts_attention_first_then_live_work_then_recent_then_id() {
    let mut agents = vec![
        entry("c", None, AgentStatus::Done, Some(10)),
        entry("b", None, AgentStatus::Running, Some(1)),
        entry("a", None, AgentStatus::Done, Some(50)),
        entry("d", None, AgentStatus::Waiting, Some(5)),
        entry("e", None, AgentStatus::Done, Some(50)),
        entry("f", None, AgentStatus::Failed, Some(2)),
    ];
    sort_agents(&mut agents);
    let order: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(order, ["d", "f", "b", "a", "e", "c"]);
}

#[test]
fn tree_rows_nest_children_under_parents_and_keep_orphans() {
    let agents = vec![
        entry("root", None, AgentStatus::Running, None),
        entry("kid", Some("root"), AgentStatus::Running, None),
        entry("grand", Some("kid"), AgentStatus::Done, None),
        entry("other", Some("missing"), AgentStatus::Done, None),
        entry("kid2", Some("root"), AgentStatus::Done, None),
    ];
    let rows = tree_rows(&agents);
    let shape: Vec<(usize, usize, bool)> = rows
        .iter()
        .map(|row| (row.index, row.depth, row.last))
        .collect();
    assert_eq!(
        shape,
        [
            (0, 0, false),
            (1, 1, false),
            (2, 2, true),
            (4, 1, true),
            (3, 0, true)
        ]
    );
}

#[test]
fn tree_rows_terminate_on_parent_cycles() {
    let agents = vec![
        entry("a", Some("b"), AgentStatus::Running, None),
        entry("b", Some("a"), AgentStatus::Running, None),
    ];
    let rows = tree_rows(&agents);
    assert_eq!(rows.len(), 2, "every worker is listed exactly once");
}

#[test]
fn subtree_collects_descendants_transitively() {
    let agents = vec![
        entry("root", None, AgentStatus::Running, None),
        entry("kid", Some("root"), AgentStatus::Running, None),
        entry("grand", Some("kid"), AgentStatus::Done, None),
        entry("other", None, AgentStatus::Done, None),
    ];
    let ids = subtree(&agents, "kid");
    assert!(ids.contains("kid") && ids.contains("grand"));
    assert!(!ids.contains("root") && !ids.contains("other"));
}

#[test]
fn filter_matches_terms_as_subsequences_across_name_id_and_task() {
    let mut agent = entry("task-3-review", None, AgentStatus::Running, None);
    agent.name = "reviewer".into();
    agent.task = "check the parser".into();
    assert!(matches_filter(&agent, ""));
    assert!(matches_filter(&agent, "rvw"));
    assert!(matches_filter(&agent, "PARSER"));
    assert!(matches_filter(&agent, "review task"));
    assert!(!matches_filter(&agent, "zebra"));
}

#[test]
fn totals_sum_metrics_and_count_live_workers() {
    let mut a = entry("a", None, AgentStatus::Running, None);
    a.cost_usd = Some(0.25);
    a.tokens_in = Some(100);
    a.tokens_out = Some(20);
    a.requests = 3;
    a.tool_calls = 4;
    a.elapsed_ms = Some(1_000);
    let mut b = entry("b", None, AgentStatus::Done, None);
    b.cost_usd = Some(0.5);
    b.requests = 2;
    b.elapsed_ms = Some(500);
    let totals = totals(&[a, b]);
    assert!((totals.cost_usd - 0.75).abs() < 1e-9);
    assert_eq!(totals.tokens_in, 100);
    assert_eq!(totals.tokens_out, 20);
    assert_eq!(totals.requests, 5);
    assert_eq!(totals.tool_calls, 4);
    assert_eq!(totals.agent_ms, 1_500);
    assert_eq!(totals.running, 1);
    assert_eq!(totals.total, 2);
}

#[test]
fn activity_filter_and_scope_cycle_and_admit() {
    assert_eq!(ActivityFilter::All.next(), ActivityFilter::Errors);
    assert_eq!(ActivityFilter::Messages.next(), ActivityFilter::All);
    assert!(ActivityFilter::Tools.admits(ActivityKind::Tool));
    assert!(!ActivityFilter::Tools.admits(ActivityKind::Error));
    let agents = vec![
        entry("root", None, AgentStatus::Running, None),
        entry("kid", Some("root"), AgentStatus::Done, None),
    ];
    let event = ActivityEvent {
        agent_id: "kid".into(),
        at_ms: 1,
        kind: ActivityKind::Response,
        title: String::new(),
        summary: String::new(),
        entry: None,
    };
    assert!(ActivityScope::Subtree("root".into()).admits(&event, &agents));
    assert!(!ActivityScope::Selected("root".into()).admits(&event, &agents));
    assert_eq!(
        ActivityScope::All.next(Some("root")),
        ActivityScope::Selected("root".into())
    );
    assert_eq!(
        ActivityScope::Selected("root".into()).next(Some("root")),
        ActivityScope::Subtree("root".into())
    );
}

#[test]
fn act_routes_cancel_message_and_resume_strings() {
    assert!(act("open:roles", None).is_none());
    let cancel = act("subagent-cancel:task-1", None).expect("recognised");
    assert!(cancel.is_err(), "no live control means a clear error");
    let resume = act("subagent-resume:task-1\nplease continue", None)
        .expect("recognised")
        .expect("resume never needs live control");
    let ActionOutcome::Resume { id, message } = resume else {
        panic!("resume returns a resume action");
    };
    assert!(id == "task-1" && message == "please continue");
    let message = act("subagent-message:task-9\nhi", None)
        .expect("recognised")
        .expect("finished or unknown workers resume");
    assert!(matches!(message, ActionOutcome::Resume { .. }));
}

#[test]
fn transcript_projection_reads_header_status_and_metrics() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let sink = ChildTranscript::create(
        &layout,
        "parent",
        "task-1-scout",
        serde_json::json!({
            "id": "task-1-scout",
            "name": "scout",
            "agent": "scout",
            "role": "tester",
            "model": "m",
            "model_source": "assigned",
            "assignment": {"task": "Find the parser\nsecond line"},
            "started_ms": 1000,
        }),
    )
    .unwrap();
    sink.record(&serde_json::json!({"role":"user","content":"go"}))
        .unwrap();
    sink.record(&serde_json::json!({"role":"assistant","content":[
        {"type":"text","text":"reading the parser"},
        {"type":"tool_use","name":"read"}
    ]}))
    .unwrap();
    sink.record(&serde_json::json!({"role":"tool","name":"read","content":"boom","is_error":true}))
        .unwrap();
    sink.finish(serde_json::json!({
        "status": "done",
        "steps": 4,
        "tools": 2,
        "requests": 3,
        "tokens_in": 900,
        "tokens_out": 120,
        "cost_usd": 0.02,
        "elapsed_ms": 4000,
        "finished_ms": 9000,
        "artifacts": [["result", "artifact://abc"]],
        "isolation": "worktree:branch-1",
    }))
    .unwrap();

    let runtime = Tree::default();
    let snap = snapshot(&layout, "parent", &runtime, None);
    assert_eq!(snap.agents.len(), 1);
    let agent = &snap.agents[0];
    assert_eq!(agent.status, AgentStatus::Done);
    assert_eq!(agent.task, "Find the parser");
    assert_eq!(agent.steps, 4);
    assert_eq!(agent.requests, 3);
    assert_eq!(agent.tool_calls, 2);
    assert_eq!(agent.tokens_in, Some(900));
    assert_eq!(agent.cost_usd, Some(0.02));
    assert_eq!(agent.elapsed_ms, Some(4000));
    assert_eq!(
        agent.artifacts,
        [("result".to_owned(), "artifact://abc".to_owned())]
    );
    assert_eq!(agent.isolation.as_deref(), Some("worktree:branch-1"));
    assert!(agent.resumable && !agent.steerable && !agent.cancellable);
    assert!(agent.transcript.is_some());
    assert_eq!(snap.totals.total, 1);
    assert_eq!(snap.totals.running, 0);

    let kinds: Vec<ActivityKind> = snap.activity.iter().map(|event| event.kind).collect();
    assert!(kinds.contains(&ActivityKind::Response));
    assert!(kinds.contains(&ActivityKind::Tool));
    assert!(kinds.contains(&ActivityKind::Error));
    assert!(kinds.contains(&ActivityKind::Message));
    assert!(kinds.contains(&ActivityKind::Lifecycle));
    let response = snap
        .activity
        .iter()
        .find(|event| event.kind == ActivityKind::Response)
        .unwrap();
    assert_eq!(
        response.entry,
        Some(1),
        "entry is the index into messages()"
    );
}

#[test]
fn transcript_without_final_record_is_cancelled_unless_still_live() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    ChildTranscript::create(
        &layout,
        "parent",
        "task-2",
        serde_json::json!({"id": "task-2", "name": "w", "agent": "worker"}),
    )
    .unwrap();
    let snap = snapshot(&layout, "parent", &Tree::default(), None);
    assert_eq!(snap.agents[0].status, AgentStatus::Cancelled);
    assert!(!snap.agents[0].cancellable);
}

#[test]
fn runtime_body_overrides_transcript_and_reports_live_state() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let tree = Tree::default();
    let body = tree.start("worker", "review", None);
    body.settle(
        String::new(),
        Some(serde_json::json!({
            "id": "task-7-review",
            "name": "review",
            "agent": "reviewer",
            "role": "tester",
            "model": "m",
            "model_source": "assigned",
            "phase": "tool: read",
            "current_tool": "read",
            "task": "Review the diff",
            "steps": 2,
            "tools": 5,
            "requests": 2,
            "context": [1200, 8000],
            "retry": [1, 3],
            "started_ms": 1,
            "parent": "task-1-scout",
            "artifacts": [["patch", "artifact://p"]],
        })),
        Phase::Running,
    );
    let snap = snapshot(&layout, "parent", &tree, None);
    let agent = snap
        .agents
        .iter()
        .find(|a| a.id == "task-7-review")
        .unwrap();
    assert_eq!(agent.status, AgentStatus::Running);
    assert_eq!(agent.current.as_deref(), Some("tool · read"));
    assert_eq!(agent.context, Some((1200, 8000)));
    assert_eq!(agent.retry, Some((1, 3)));
    assert_eq!(agent.parent.as_deref(), Some("task-1-scout"));
    assert_eq!(agent.tool_calls, 5);
    assert_eq!(agent.requests, 2);
    assert_eq!(agent.artifacts.len(), 1);
    assert!(agent.elapsed_ms.is_some());
    assert_eq!(snap.totals.running, 1);
    // Without a live owner a running body is not steerable or cancellable.
    assert!(!agent.steerable && !agent.cancellable);
}
