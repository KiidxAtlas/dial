use super::*;

#[test]
fn start_returns_while_work_is_blocked_and_delivers_once_acknowledged() {
    let mut questions = Questions::default();
    let (send, receive) = mpsc::channel();
    let id = questions
        .start(move |_| receive.recv().map_err(|e| e.to_string()))
        .unwrap();
    assert_eq!(questions.running_ids(), std::slice::from_ref(&id));
    assert!(questions.undelivered().is_empty());
    send.send("answer".into()).unwrap();
    let result = questions
        .query(
            &json!({"action":"wait","id":id,"timeout":2}),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(result[0]["answer"], "answer");
    assert_eq!(questions.undelivered().len(), 1);
    questions.deliver(&id);
    assert!(questions.undelivered().is_empty());
    assert!(questions.running_ids().is_empty());
}

#[test]
fn concurrency_limit_cancel_and_failure_are_visible() {
    let mut questions = Questions::default();
    for _ in 0..MAX_LIVE {
        questions
            .start(|cancel| {
                while !cancel.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok("not a successful answer after cancellation".into())
            })
            .unwrap();
    }
    assert!(questions.start(|_| Ok("extra".into())).is_err());
    questions.cancel_all();
    for id in questions.running_ids() {
        let result = questions
            .query(
                &json!({"action":"wait","id":id,"timeout":2}),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(result[0]["status"], "failed");
        assert_eq!(result[0]["error"], "side question cancelled");
    }
}

#[test]
fn unknown_ids_invalid_wait_and_actions_fail_without_starting_work() {
    let mut questions = Questions::default();
    let interrupt = AtomicBool::new(false);
    for args in [
        json!({"action":"wait"}),
        json!({"action":"cancel","id":"absent"}),
        json!({"action":"status","timeout":31}),
        json!({"action":"bad"}),
    ] {
        assert!(questions.query(&args, &interrupt).is_err(), "{args}");
    }
    assert!(questions.running_ids().is_empty());
}

#[test]
fn undelivered_results_are_not_evicted_and_delivered_results_are_bounded() {
    let mut questions = Questions::default();
    for _ in 0..MAX_RETAINED {
        let id = questions.start(|_| Err("provider failure".into())).unwrap();
        let result = questions
            .query(
                &json!({"action":"wait","id":id,"timeout":2}),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(result[0]["error"], "provider failure");
    }
    assert!(questions.start(|_| Ok("overflow".into())).is_err());
    let first = questions.jobs.front().unwrap().id.clone();
    questions.deliver(&first);
    questions.start(|_| Ok("new".into())).unwrap();
    assert_eq!(questions.jobs.len(), MAX_RETAINED);
    assert!(!questions.jobs.iter().any(|job| job.id == first));
}
