use super::*;
use std::cell::RefCell;

#[test]
fn panics_reply_to_pending_request_then_fatal_and_stop() {
    for id in [None, Some(json!(17)), Some(json!("request"))] {
        let events = RefCell::new(Vec::new());
        let outcome = run_boundary(
            |pending| {
                *pending = id.clone();
                panic!("private panic payload");
            },
            |id, result| {
                let error = result.unwrap_err();
                assert_eq!(
                    error.downcast_ref::<RequestError>().unwrap().code,
                    "internal_error"
                );
                events
                    .borrow_mut()
                    .push(json!({"reply":id,"error":error.to_string()}));
            },
            |message| events.borrow_mut().push(json!({"fatal":message})),
        );
        assert!(outcome.is_err());
        let events = events.into_inner();
        assert_eq!(events.len(), if id.is_some() { 2 } else { 1 });
        if let Some(id) = id {
            assert_eq!(events[0]["reply"], id);
        }
        assert!(events.last().unwrap()["fatal"].is_string());
        assert!(
            !events
                .iter()
                .any(|event| event.to_string().contains("private panic payload"))
        );
    }
}

#[test]
fn normal_actor_failure_emits_fatal_without_duplicate_request_reply() {
    let events = RefCell::new(Vec::new());
    let outcome = run_boundary(
        |_| anyhow::bail!("initialization failed"),
        |_, _| panic!("normal errors already have request replies"),
        |message| events.borrow_mut().push(message.to_owned()),
    );
    assert!(outcome.is_err());
    assert_eq!(*events.borrow(), ["initialization failed"]);
    assert!(
        run_boundary(
            |_| Ok(()),
            |_, _| panic!("unexpected reply"),
            |_| panic!("unexpected fatal")
        )
        .is_ok()
    );
}
