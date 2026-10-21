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
