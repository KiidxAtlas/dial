use super::*;
#[test]
fn worker_budget_is_shared_and_late_usage_stays_in_its_original_turn() {
    let scope = Scope::default().next_turn(2).unwrap();
    let worker = scope.worker();
    worker.begin(Billing::Api).unwrap();
    worker.attempt(false).unwrap();
    worker.attempt(true).unwrap();
    let second = worker.clone();
    second.begin(Billing::Api).unwrap();
    second.attempt(false).unwrap();
    assert!(second.begin(Billing::Api).is_err());
    let next = scope.next_turn(2).unwrap();
    worker
        .finish(
            Some(&Reply {
                prompt_tokens: Some(12),
                completion_tokens: Some(3),
                ..Reply::default()
            }),
            None,
            Billing::Api,
            2,
        )
        .unwrap();
    second.finish(None, None, Billing::Api, 1).unwrap();
    let (total, current) = next.snapshot();
    assert_eq!(total.workers.requests, 2);
    assert_eq!(total.workers.input, 12);
    assert_eq!(total.workers.retries, 1);
    assert_eq!(total.workers.reported_responses, 1);
    assert_eq!(total.workers.failed_requests, 1);
    assert_eq!(current.workers.requests, 0);
    assert_eq!(scope.snapshot().1.workers.input, 12);
}

#[test]
fn models_keep_separate_partial_usage_and_late_workers_keep_their_task() {
    let task = Scope::default().next_turn(2).unwrap();
    let main = task.for_model("openai-compatible/a".into());
    let worker = task.worker().for_model("anthropic/b".into());
    main.begin(Billing::Api).unwrap();
    main.attempt(false).unwrap();
    main.finish(
        Some(&Reply {
            prompt_tokens: Some(10),
            completion_tokens: Some(2),
            ..Reply::default()
        }),
        None,
        Billing::Api,
        1,
    )
    .unwrap();
    worker.begin(Billing::Api).unwrap();
    worker.attempt(false).unwrap();
    worker.attempt(true).unwrap();
    let next = task.next_turn(2).unwrap();
    worker
        .finish(
            Some(&Reply {
                completion_tokens: Some(3),
                cached_prompt_tokens: Some(40),
                ..Reply::default()
            }),
            None,
            Billing::Api,
            2,
        )
        .unwrap();
    let (total, current) = task.snapshot();
    assert_eq!(current.by_model["openai-compatible/a"].main.input, 10);
    assert_eq!(current.by_model["anthropic/b"].workers.output, 3);
    assert_eq!(
        current.by_model["anthropic/b"].workers.reported_responses,
        0
    );
    assert_eq!(
        current.by_model["anthropic/b"]
            .workers
            .cache_reported_responses,
        1
    );
    assert_eq!(current.by_model["anthropic/b"].workers.retries, 1);
    assert_eq!(total.workers.cache_read, 40);
    assert!(next.snapshot().1.by_model.is_empty());
}

#[test]
fn purposes_partition_usage_without_changing_worker_budgets_or_turn_ownership() {
    let root = Scope::default().next_turn(2).unwrap();
    let rank = root
        .worker()
        .for_purpose(Purpose::Retrieval)
        .for_model("shared".into());
    let compact = root
        .worker()
        .for_model("shared".into())
        .for_purpose(Purpose::Compaction);
    rank.begin(Billing::Api).unwrap();
    rank.attempt(false).unwrap();
    rank.attempt(true).unwrap();
    compact.begin(Billing::Api).unwrap();
    compact.attempt(false).unwrap();
    assert!(
        root.worker()
            .for_purpose(Purpose::Memory)
            .begin(Billing::Api)
            .is_err()
    );
    let next = root.next_turn(2).unwrap();
    rank.finish(
        Some(&Reply {
            prompt_tokens: Some(30),
            completion_tokens: Some(5),
            cached_prompt_tokens: Some(20),
            cache_creation_tokens: Some(3),
            reasoning_tokens: Some(2),
            ..Reply::default()
        }),
        Some(0.25),
        Billing::Api,
        2,
    )
    .unwrap();
    compact.finish(None, None, Billing::Api, 1).unwrap();
    let (total, turn) = root.snapshot();
    let ranked = turn.by_purpose[&Purpose::Retrieval];
    assert_eq!(ranked.requests, 1);
    assert_eq!(ranked.retries, 1);
    assert_eq!(ranked.unreported_attempts, 1);
    assert_eq!(ranked.cache_read, 20);
    assert_eq!(ranked.cache_write, 3);
    assert_eq!(ranked.reasoning, 2);
    assert_eq!(ranked.observed_cost_usd, Some(0.25));
    assert!(matches!(ranked.cost_coverage, CostCoverage::Partial));
    assert_eq!(turn.by_purpose[&Purpose::Compaction].failed_requests, 1);
    assert_eq!(turn.by_model["shared"].workers.requests, 2);
    let sum = turn
        .by_purpose
        .values()
        .try_fold(Counts::default(), |sum, &c| sum.add(c))
        .unwrap();
    assert_eq!(sum.requests, total.all.requests);
    assert_eq!(sum.input, total.all.input);
    assert_eq!(sum.output, total.all.output);
    assert_eq!(sum.observed_cost_usd, total.all.observed_cost_usd);
    assert!(next.snapshot().1.by_purpose.is_empty());
    assert_eq!(
        serde_json::to_value(&turn).unwrap()["by_purpose"]["retrieval"]["input"],
        30
    );
}

#[test]
fn external_receipts_preserve_purpose_and_release_unstarted_reservations() {
    let root = Scope::default().next_turn(2).unwrap();
    let memory = root.worker().for_purpose(Purpose::Memory);
    drop(memory.reserve_external("memory-service".into()).unwrap());
    assert!(root.snapshot().0.by_purpose.is_empty());
    let mut received = memory.reserve_external("memory-service".into()).unwrap();
    received.mark_started().unwrap();
    let mut lost = memory.reserve_external("memory-service".into()).unwrap();
    lost.mark_started().unwrap();
    assert!(memory.reserve_external("memory-service".into()).is_err());
    let next = root.next_turn(2).unwrap();
    received
        .account(
            &serde_json::json!({
                "prompt_tokens": 9, "completion_tokens": 4, "cost": 0.5
            }),
            true,
            true,
        )
        .unwrap();
    drop(lost);
    let total = next.snapshot().0;
    let observed = total.by_purpose[&Purpose::Memory];
    assert_eq!(observed.requests, 2);
    assert_eq!(observed.successful_requests, 1);
    assert_eq!(observed.failed_requests, 1);
    assert_eq!(observed.discarded_attempts, 2);
    assert_eq!(observed.reported_responses, 1);
    assert_eq!(observed.unreported_attempts, 1);
    assert_eq!(observed.observed_cost_usd, Some(0.5));
    assert!(matches!(observed.cost_coverage, CostCoverage::Partial));
    assert_eq!(root.snapshot().1.by_purpose[&Purpose::Memory].input, 9);
    assert!(next.snapshot().1.by_purpose.is_empty());
}

#[test]
fn wrong_purpose_or_model_cannot_settle_another_scopes_transport_attempt() {
    let root = Scope::default()
        .next_turn(0)
        .unwrap()
        .for_model("model-a".into());
    root.begin(Billing::Api).unwrap();
    root.attempt(false).unwrap();
    let reply = Reply {
        prompt_tokens: Some(3),
        completion_tokens: Some(2),
        ..Reply::default()
    };
    let before = serde_json::to_value(root.snapshot().0).unwrap();
    for wrong in [
        root.for_purpose(Purpose::Repair),
        root.for_model("model-b".into()),
    ] {
        assert!(wrong.finish(Some(&reply), None, Billing::Api, 1).is_err());
        assert_eq!(serde_json::to_value(root.snapshot().0).unwrap(), before);
    }
    root.finish(Some(&reply), None, Billing::Api, 1).unwrap();
    assert_eq!(
        root.snapshot().0.by_purpose[&Purpose::Agent].unreported_attempts,
        0
    );
}

#[test]
fn purpose_overflow_does_not_partially_update_any_group() {
    let root = Scope::default().next_turn(0).unwrap();
    {
        let mut state = root.state.lock().unwrap();
        state
            .total
            .by_purpose
            .entry(Purpose::Agent)
            .or_default()
            .requests = u64::MAX;
    }
    let before = serde_json::to_value(root.snapshot()).unwrap();
    assert!(root.begin(Billing::Api).is_err());
    assert_eq!(serde_json::to_value(root.snapshot()).unwrap(), before);
}

#[test]
fn retries_retain_latest_snapshot_once_and_do_not_duplicate_accepted_usage() {
    let root = Scope::default().next_turn(0).unwrap();
    let scope = root
        .worker()
        .for_model("provider/model".into())
        .for_purpose(Purpose::Repair);
    scope.begin(Billing::Api).unwrap();
    let transport = Transport::enter(Some(scope.clone()));
    transport_attempt().unwrap();
    capture(Snapshot {
        input: Some(20),
        output: Some(0),
        ..Default::default()
    });
    capture(Snapshot {
        output: Some(3),
        cache_read: Some(4),
        ..Default::default()
    });
    transport_attempt().unwrap();
    let reply = Reply {
        prompt_tokens: Some(25),
        completion_tokens: Some(5),
        ..Default::default()
    };
    capture_reply(&reply);
    transport.settle(true).unwrap();
    transport.settle(true).unwrap();
    scope
        .finish(Some(&reply), Some(0.1), Billing::Api, transport.attempts())
        .unwrap();
    let groups = root.snapshot().1;
    let counts = groups.workers;
    assert_eq!((counts.input, counts.output, counts.cache_read), (45, 8, 4));
    assert_eq!(counts.partial_reported_attempts, 1);
    assert_eq!(counts.reported_responses, 1);
    assert_eq!(counts.unreported_attempts, 1);
    assert_eq!(counts.transport_attempts, 2);
    assert_eq!(counts.retries, 1);
    assert_eq!(counts.discarded_attempts, 1);
    assert_eq!(counts.observed_cost_usd, Some(0.1));
    assert!(matches!(counts.cost_coverage, CostCoverage::Partial));
    assert_eq!(groups.by_purpose[&Purpose::Repair].input, 45);
    assert_eq!(groups.by_model["provider/model"].workers.output, 8);
}

#[test]
fn failed_partial_usage_stays_unpriced_in_its_original_turn() {
    let root = Scope::default().next_turn(0).unwrap();
    root.begin(Billing::Api).unwrap();
    let transport = Transport::enter(Some(root.clone()));
    transport_attempt().unwrap();
    capture(Snapshot {
        input: Some(12),
        cache_write: Some(8),
        ..Default::default()
    });
    let next = root.next_turn(0).unwrap();
    transport.settle(false).unwrap();
    transport.settle(false).unwrap();
    root.finish(None, None, Billing::Api, transport.attempts())
        .unwrap();
    let counts = root.snapshot().1.all;
    assert_eq!(
        (counts.input, counts.output, counts.cache_write),
        (12, 0, 8)
    );
    assert_eq!(counts.partial_reported_attempts, 1);
    assert_eq!(counts.reported_responses, 0);
    assert_eq!(counts.unreported_attempts, 1);
    assert_eq!(counts.failed_requests, 1);
    assert_eq!(counts.observed_cost_usd, None);
    assert_eq!(next.snapshot().1.all.input, 0);
}

#[test]
fn nested_and_suspended_transports_do_not_cross_attribute_snapshots() {
    let scope = Scope::default().next_turn(0).unwrap();
    scope.begin(Billing::Api).unwrap();
    let outer = Transport::enter(Some(scope.clone()));
    transport_attempt().unwrap();
    capture(Snapshot {
        input: Some(9),
        ..Default::default()
    });
    {
        let _suspended = Transport::suspend();
        transport_attempt().unwrap();
        capture(Snapshot {
            input: Some(999),
            ..Default::default()
        });
    }
    outer.settle(false).unwrap();
    scope
        .finish(None, None, Billing::Api, outer.attempts())
        .unwrap();
    assert_eq!(scope.snapshot().1.all.input, 9);
    assert_eq!(scope.snapshot().1.all.transport_attempts, 1);
}
