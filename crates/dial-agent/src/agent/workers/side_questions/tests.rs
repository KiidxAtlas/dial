use super::*;

fn query(questions: &mut Questions, args: &Value, interrupt: &AtomicBool) -> Result<Value> {
    questions
        .query(args, interrupt, &mut || false)
        .map(|(value, stopped)| {
            assert!(!stopped, "no input was pending");
            value
        })
}

#[test]
fn pending_user_input_ends_a_wait_without_cancelling_the_question() {
    let mut questions = Questions::default();
    let (send, receive) = mpsc::channel::<String>();
    let id = questions
        .start(move |_| receive.recv().map_err(|e| e.to_string()))
        .unwrap();
    let started = Instant::now();
    let mut checks = 0;
    let (result, stopped) = questions
        .query(
            &json!({"action":"wait","id":id,"timeout":30}),
            &AtomicBool::new(false),
            &mut || {
                checks += 1;
                checks > 2
            },
        )
        .unwrap();
    assert!(stopped);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(result[0]["status"], "running");
    // The question keeps running and still delivers its answer.
    send.send("answer".into()).unwrap();
    let result = query(
        &mut questions,
        &json!({"action":"wait","id":id,"timeout":2}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(result[0]["delivery"], "pending");
    assert!(result[0].get("answer").is_none());
}

#[test]
fn pending_input_is_not_checked_once_the_wait_is_already_satisfied() {
    let mut questions = Questions::default();
    let id = questions.start(|_| Ok("done".into())).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let (result, stopped) = questions
        .query(
            &json!({"action":"wait","id":id,"timeout":2}),
            &AtomicBool::new(false),
            &mut || true,
        )
        .unwrap();
    assert!(
        !stopped,
        "a finished answer is reported as a normal completion"
    );
    assert_eq!(result[0]["status"], "completed");
    assert_eq!(result[0]["delivery"], "pending");
}

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
    let result = query(
        &mut questions,
        &json!({"action":"wait","id":id,"timeout":2}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(result[0].get("answer").is_none());
    assert_eq!(questions.undelivered()[0].1["answer"], "answer");
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
        let result = query(
            &mut questions,
            &json!({"action":"wait","id":id,"timeout":2}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result[0]["status"], "failed");
        assert!(result[0].get("error").is_none());
        assert_eq!(
            questions.undelivered().last().unwrap().1["error"],
            "side question cancelled"
        );
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
        assert!(query(&mut questions, &args, &interrupt).is_err(), "{args}");
    }
    assert!(questions.running_ids().is_empty());
}

#[test]
fn undelivered_results_are_not_evicted_and_delivered_results_are_bounded() {
    let mut questions = Questions::default();
    for _ in 0..MAX_RETAINED {
        let id = questions.start(|_| Err("provider failure".into())).unwrap();
        let result = query(
            &mut questions,
            &json!({"action":"wait","id":id,"timeout":2}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result[0]["status"], "failed");
        assert_eq!(
            questions.undelivered().last().unwrap().1["error"],
            "provider failure"
        );
    }
    assert!(questions.start(|_| Ok("overflow".into())).is_err());
    let first = questions.jobs.front().unwrap().id.clone();
    questions.deliver(&first);
    questions.start(|_| Ok("new".into())).unwrap();
    assert_eq!(questions.jobs.len(), MAX_RETAINED);
    assert!(!questions.jobs.iter().any(|job| job.id == first));
}
