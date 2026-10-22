use super::*;

fn user(text: &str) -> Value {
    json!({"role":"user","content":[{"type":"input_text","text":text}]})
}

#[test]
fn delta_requires_strict_prefix_and_equal_options() {
    let previous = json!({"model":"m","input":[user("a")]});
    let output = vec![
        json!({"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[]}),
    ];
    let replayed = json!({"type":"message","role":"assistant","content":[]});
    let current = json!({"model":"m","input":[user("a"), replayed, user("b")]});
    assert_eq!(
        delta_input(&previous, &output, &current),
        Some(vec![user("b")])
    );
    let mutated = json!({"model":"m","input":[user("x"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
    assert_eq!(delta_input(&previous, &output, &mutated), None);
    let other_model = json!({"model":"n","input":[user("a"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
    assert_eq!(delta_input(&previous, &output, &other_model), None);
}

#[test]
fn steered_plan_attaches_creates_or_discards() {
    let steer = vec![user("stop and do b")];
    let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
    assert!(matches!(
        plan_steered(Some(&[user("stop and do b")]), &steer),
        Plan::Attach
    ));
    assert!(
        matches!(plan_steered(Some(&[output.clone(), user("stop and do b")]), &steer), Plan::Create(rest) if rest == vec![output])
    );
    assert!(matches!(
        plan_steered(Some(&[user("stop and do b"), user("unrelated")]), &steer),
        Plan::Discard
    ));
    assert!(matches!(
        plan_steered(Some(&[user("other")]), &steer),
        Plan::Discard
    ));
    assert!(matches!(plan_steered(None, &steer), Plan::Discard));
}

#[test]
fn broken_pipe_write_is_retryable_but_protocol_errors_are_not() {
    let error = write_error(tungstenite::Error::Io(std::io::Error::from(
        ErrorKind::BrokenPipe,
    )));
    assert!(
        error
            .downcast_ref::<super::super::stream::IncompleteStream>()
            .is_some()
    );
    assert!(super::super::errors::classify(&error).is_retryable());
    let error = write_error(tungstenite::Error::Protocol(
        tungstenite::error::ProtocolError::InvalidOpcode(15),
    ));
    assert!(!super::super::errors::classify(&error).is_retryable());
}

#[test]
fn only_plain_user_messages_can_steer() {
    assert!(steer_items(&json!([user("a")])).is_some());
    assert!(
        steer_items(&json!([{"type":"function_call_output","call_id":"c","output":""}])).is_none()
    );
    assert!(steer_items(&json!([])).is_none());
}

#[test]
fn failed_websocket_usage_is_retained_for_created_and_attached_responses() {
    use super::super::usage::{Billing, Scope, Transport};
    for attached in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut socket = tungstenite::accept(listener.accept().unwrap().0).unwrap();
            if !attached {
                let request = socket.read().unwrap();
                assert!(request.into_text().unwrap().contains("response.create"));
            }
            socket.send(Message::Text(json!({
                "type":"response.failed",
                "response":{"usage":{"input_tokens":19,"output_tokens":2},"error":{"code":"server_error"}}
            }).to_string().into())).unwrap();
        });
        let tcp = TcpStream::connect(address).unwrap();
        let (mut socket, _) = tungstenite::client(
            format!("ws://{address}/responses"),
            MaybeTlsStream::Plain(tcp),
        )
        .unwrap();
        let scope = Scope::default().next_turn(0).unwrap();
        scope.begin(Billing::Subscription).unwrap();
        let transport = Transport::enter(Some(scope.clone()));
        let (_tx, rx) = std::sync::mpsc::sync_channel(STEER_QUEUE);
        let body = json!({"model":"m","input":[user("hello")]});
        let mut run = Run {
            pending: VecDeque::new(),
            accepted: Vec::new(),
            tainted: false,
            response_id: None,
        };
        assert!(
            stream(
                &mut socket,
                (!attached).then_some(&body),
                &body,
                &HashSet::new(),
                &rx,
                &mut run,
                5,
                &AtomicBool::new(false),
                &mut None,
                &mut |_| true,
            )
            .is_err()
        );
        transport.settle(false).unwrap();
        scope
            .finish(None, None, Billing::Subscription, transport.attempts())
            .unwrap();
        let counts = scope.snapshot().1.all;
        assert_eq!(counts.transport_attempts, 1);
        assert_eq!(counts.partial_reported_attempts, 1);
        assert_eq!((counts.input, counts.output), (19, 2));
        assert_eq!(counts.reported_responses, 0);
        assert_eq!(counts.observed_cost_usd, None);
        server.join().unwrap();
    }
}
