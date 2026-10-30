use super::*;

fn test_launch(model: &str) -> Arc<PoolLaunch> {
    let workspace = Workspace::new(std::env::temp_dir().as_path()).expect("workspace");
    let config = Config {
        model: model.into(),
        ..Config::default()
    };
    let ext = Registry::new();
    let harness = Harness::default();
    Arc::new(PoolLaunch::capture(&StartContext {
        config: &config,
        workspace: &workspace,
        ext: &ext,
        layout: None,
        session_id: "parent",
        harness: &harness,
        usage: None,
        context_cap: None,
        memory: None,
        plan_mode: false,
        agents_messages: &[],
        handoff: subagents::Handoff::capture(&json!({}), &[]),
    }))
}

fn item(state: ItemState) -> PoolItem {
    PoolItem {
        args: json!({}),
        launch: test_launch("test"),
        state,
    }
}

#[test]
fn item_fields_override_pool_fields() {
    let shared = json!({"agent": "explore", "max_steps": 8});
    let task = json!({"max_steps": 2, "context": "parse"});
    let args = merged(&shared, &task).expect("object task");
    assert_eq!(args["agent"], "explore");
    assert_eq!(args["max_steps"], 2);
    assert_eq!(args["context"], "parse");
}

#[test]
fn merged_rejects_non_object_items() {
    assert!(merged(&json!({}), &json!("inspect")).is_err());
}

#[test]
fn free_slot_skips_busy_slots() {
    let mut pool = WorkerPool::new("batch".into(), 2, json!({}));
    assert_eq!(pool.free_slot(), Some(0));
    pool.items.push(item(ItemState::Running {
        job: "task-1".into(),
        slot: 0,
    }));
    assert_eq!(pool.free_slot(), Some(1));
    pool.items.push(item(ItemState::Running {
        job: "task-2".into(),
        slot: 1,
    }));
    assert_eq!(pool.free_slot(), None);
}

#[test]
fn state_follows_pending_and_terminal_items() {
    let mut pool = WorkerPool::new("batch".into(), 1, json!({}));
    assert_eq!(pool.state(), "open");
    pool.items.push(item(ItemState::Queued));
    assert_eq!(pool.state(), "running");
    pool.items[0].state = ItemState::Settled(json!({"status": "done"}));
    assert_eq!(pool.state(), "done");
    pool.items
        .push(item(ItemState::Settled(json!({"status": "failed"}))));
    assert_eq!(pool.state(), "partial");
    pool.closed = true;
    assert_eq!(pool.state(), "closed");
}

#[test]
fn started_job_requires_a_running_entry() {
    let started = json!([
        {"id": "task-3", "status": "failed"},
        {"id": "task-4", "status": "running"}
    ]);
    assert_eq!(started_job(&started).as_deref(), Some("task-4"));
    assert_eq!(
        started_job(&json!([{"id": "task-5", "status": "failed"}])),
        None
    );
    assert_eq!(
        started_job(&json!({"id": "task-6", "status": "running"})),
        None
    );
}

#[test]
fn pool_polling_tracks_delivery_per_item_without_hiding_new_results() {
    let mut tasks = LiveTasks::default();
    let mut pool = WorkerPool::new("batch".into(), 1, json!({}));
    for index in 1..=2 {
        let id = format!("task-{index}");
        let body = tasks.tree.start("worker", &id, None);
        let record = json!({"id":id,"status":"done","result":format!("narrative {index}"),
            "artifact":format!("artifact://report-{index}")});
        body.settle(
            String::new(),
            Some(record.clone()),
            crate::runtime::Phase::from_status("done"),
        );
        tasks.jobs.insert(
            id,
            JobState {
                body,
                interrupt: Arc::new(AtomicBool::new(false)),
                worker: None,
                active_tool: Arc::default(),
                delivered: false,
            },
        );
        pool.items.push(item(ItemState::Settled(record)));
    }
    tasks.pools.insert("pool-1".into(), pool);
    // Canonical automatic delivery covers only the first item in this batch.
    let mut first_batch = tasks.report_from("pool-1", false, 0).unwrap();
    first_batch["items"].as_array_mut().unwrap().truncate(1);
    tasks.completed.push(aggregate(first_batch));
    tasks.deliver("pool-1");
    let report = tasks.pool_report("pool-1", false).unwrap();
    let polled = tasks.polling_output(Ok(report.clone()));
    assert!(!polled.text.contains("narrative 1"));
    assert!(polled.text.contains("artifact://report-1"));
    assert!(polled.text.contains("narrative 2"));
    let repeated = tasks.polling_output(Ok(report.clone()));
    assert!(!repeated.text.contains("narrative 1") && !repeated.text.contains("narrative 2"));
    assert_eq!(repeated.data, Some(report.clone()));
    let recovered = output(Ok(report));
    assert!(recovered.text.contains("narrative 1") && recovered.text.contains("narrative 2"));
}

#[test]
fn queued_items_keep_the_launch_they_were_pushed_with() {
    let alpha = test_launch("alpha");
    let beta = test_launch("beta");
    let task = json!({"agent": "worker", "task": "one", "solution_space": "s"});
    let mut first = WorkerPool::new("first".into(), 1, json!({}));
    first
        .enqueue(&alpha, std::slice::from_ref(&task))
        .expect("alpha item");
    let mut second = WorkerPool::new("second".into(), 1, json!({}));
    second
        .enqueue(&beta, std::slice::from_ref(&task))
        .expect("beta item");
    first
        .enqueue(&beta, std::slice::from_ref(&task))
        .expect("later push");
    assert_eq!(first.items[0].launch.config.model, "alpha");
    assert_eq!(first.items[1].launch.config.model, "beta");
    assert_eq!(second.items[0].launch.config.model, "beta");
}

#[test]
fn automatic_aggregates_cover_each_item_once_across_batches() {
    let launch = test_launch("alpha");
    let task = |name: &str| json!({"agent": "worker", "task": name, "solution_space": "s"});
    let mut tasks = LiveTasks::default();
    let mut pool = WorkerPool::new("batch".into(), 1, json!({}));
    pool.enqueue(&launch, &[task("one")]).expect("first item");
    pool.items[0].state = ItemState::Settled(json!({"status": "done", "result": "first"}));
    tasks.pools.insert("pool-1".into(), pool);
    tasks.announce_pools();
    let pool = tasks.pools.get_mut("pool-1").expect("pool");
    pool.enqueue(&launch, &[task("two")]).expect("second item");
    pool.items[1].state = ItemState::Settled(json!({"status": "done", "result": "second"}));
    tasks.announce_pools();
    assert_eq!(tasks.completed.len(), 2);
    let second = &tasks.completed[1]["items"];
    assert_eq!(second.as_array().expect("items").len(), 1);
    assert_eq!(second[0]["item"], 2);
    assert!(
        !tasks.completed[1]["result"]
            .as_str()
            .unwrap_or("")
            .contains("first")
    );
}
