use super::*;

fn fake(source: &str) -> JsonProcess {
    let mut command = Command::new("python3");
    command.args(["-u", "-c", source]);
    JsonProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap()
}

#[test]
fn json_transport_round_trip_and_owner_drop() {
    let mut process = fake(
        "import sys,json\nfor line in sys.stdin:\n r=json.loads(line);print(json.dumps({'id':r['id'],'result':{'value':r['value']}}),flush=True)",
    );
    let pid = process.pid.clone();
    assert_eq!(
        process
            .request(
                json!({"value":42}),
                Duration::from_secs(2),
                |_| unreachable!()
            )
            .unwrap()["value"],
        42
    );
    drop(process);
    assert_eq!(pid.load(Ordering::Acquire), 0);
}

#[test]
fn json_transport_stalled_writes_observe_deadline_and_cancel() {
    for cancelled in [false, true] {
        let mut process = fake("import time; time.sleep(30)");
        let start = Instant::now();
        let error = process
            .request_guarded(
                json!({"value":"x".repeat(1024 * 1024)}),
                Some(start + Duration::from_millis(200)),
                || {
                    if cancelled && start.elapsed() >= Duration::from_millis(50) {
                        Err("cancelled".into())
                    } else {
                        Ok(())
                    }
                },
                0,
                |_| unreachable!(),
            )
            .unwrap_err();
        assert!(
            error.contains(if cancelled { "cancelled" } else { "deadline" }),
            "{error}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn json_transport_rejects_unterminated_oversized_frame() {
    let mut process = fake(
        "import sys,time; sys.stdin.readline(); sys.stdout.write('x' * (13 * 1024 * 1024)); sys.stdout.flush(); time.sleep(30)",
    );
    let error = process
        .request(json!({}), Duration::from_secs(5), |_| unreachable!())
        .unwrap_err();
    assert!(error.contains("frame limit"), "{error}");
}

#[test]
fn json_transport_stale_callback_never_reaches_controller() {
    let mut process = fake(
        "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'callback':'tool','operation_id':0,'callback_id':1}),flush=True)\na=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':a}),flush=True)",
    );
    let result = process
        .request(json!({}), Duration::from_secs(2), |_| {
            panic!("stale callback dispatched")
        })
        .unwrap();
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("expired operation")
    );
}

#[test]
fn yieldable_request_hands_off_and_resumes_to_the_same_reply() {
    let mut process = fake(
        "import sys,json,time\nfor line in sys.stdin:\n r=json.loads(line);time.sleep(0.5);print(json.dumps({'id':r['id'],'result':{'value':r['value']}}),flush=True)",
    );
    let handed = process
        .request_yieldable(
            json!({"value":7}),
            None,
            || Ok(()),
            || true,
            usize::MAX,
            |_| unreachable!(),
        )
        .unwrap();
    assert!(handed.is_none(), "the request should still be in flight");
    let reply = process
        .resume_yieldable(
            Some(Instant::now() + Duration::from_secs(5)),
            || Ok(()),
            || false,
            usize::MAX,
            |_| unreachable!(),
        )
        .unwrap();
    assert_eq!(reply.unwrap()["value"], 7);
}

#[test]
fn resume_without_an_in_flight_request_is_refused() {
    let mut process = fake("import sys\nfor line in sys.stdin:\n pass");
    let error = process
        .resume_yieldable(None, || Ok(()), || false, usize::MAX, |_| unreachable!())
        .unwrap_err();
    assert!(error.contains("no adapter request is in flight"), "{error}");
}
