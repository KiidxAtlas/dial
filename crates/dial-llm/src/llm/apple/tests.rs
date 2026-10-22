use super::*;

#[test]
fn availability_reply_maps_reasons_and_rejects_other_protocols() {
    let ok = parse_availability(
        r#"{"type":"availability","protocol":1,"available":true,"context_size":8192}"#,
    )
    .unwrap();
    assert_eq!(
        ok,
        Availability {
            available: true,
            reason: None,
            context_size: 8192
        }
    );
    let off = parse_availability(
        r#"{"type":"availability","protocol":1,"available":false,"reason":"apple_intelligence_not_enabled"}"#,
    )
    .unwrap();
    assert!(!off.available);
    assert_eq!(off.context_size, FALLBACK_CONTEXT);
    assert!(off.reason.unwrap().contains("turned off"));
    assert!(parse_availability(r#"{"protocol":2,"available":true}"#).is_err());
}

#[test]
fn request_body_strips_bookkeeping_and_stringifies_tool_arguments() {
    let req = Chat {
        messages: vec![
            json!({"role":"system","content":"be brief"}),
            json!({"role":"user","content":"hi","dial_transient":true}),
            json!({"role":"assistant","content":"","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"read","arguments":{"path":"a"}}}
            ]}),
            json!({"role":"tool","tool_call_id":"c1","content":"text"}),
        ],
        tools: vec![json!({"type":"function","function":{"name":"read","parameters":{}}})],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let body = request_body(&req);
    assert!(body["messages"][1].get("dial_transient").is_none());
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a"}"#
    );
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["max_tokens"], 64);
}

#[test]
fn events_fold_into_a_reply_and_errors_keep_their_class() {
    let mut texts = Vec::new();
    let mut collect = |piece: &str| {
        texts.push(piece.to_owned());
        true
    };
    let mut events = |_: &llm::StreamEvent| true;
    let mut fold = Fold {
        reply: Reply::default(),
        on_text: Some(&mut collect),
        on_event: &mut events,
        generating: false,
    };
    assert!(!fold.feed(r#"{"type":"text","delta":"Hel"}"#).unwrap());
    assert!(!fold.feed(r#"{"type":"text","delta":"lo"}"#).unwrap());
    assert!(
        !fold
            .feed(
                r#"{"type":"tool_call","id":"call_1","name":"read","arguments":"{\"path\":\"x\"}"}"#
            )
            .unwrap()
    );
    assert!(
        !fold
            .feed(r#"{"type":"tool_call","id":"call_2","name":"read","arguments":"[1]"}"#)
            .unwrap()
    );
    assert!(
        fold.feed(r#"{"type":"done","finish_reason":"tool_calls","usage":{"prompt_tokens":10,"completion_tokens":3}}"#)
            .unwrap()
    );
    let reply = std::mem::take(&mut fold.reply);
    assert_eq!(reply.text, "Hello");
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"x"}));
    assert!(reply.tool_calls[1].malformed_arguments);
    assert_eq!(reply.prompt_tokens, Some(10));
    let error = fold
        .feed(r#"{"type":"error","kind":"context_exceeded","message":"too long"}"#)
        .unwrap_err();
    assert!(llm::errors::is_context_overflow(&error));
    let busy = fold
        .feed(r#"{"type":"error","kind":"busy","message":""}"#)
        .unwrap_err();
    assert!(llm::errors::classify(&busy).is_retryable());
    drop(fold);
    assert_eq!(texts, ["Hel", "lo"]);
}

/// Exercises the real helper and system model; skipped where the model is unavailable.
#[test]
fn on_device_model_answers_when_available() {
    if !available() {
        return;
    }
    let req = Chat {
        messages: vec![json!({"role":"user","content":"Reply with the single word: pong"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 16,
        seed: None,
    };
    let scope = llm::usage::Scope::default().next_turn(0).unwrap();
    scope.begin(llm::usage::Billing::Local).unwrap();
    let transport = llm::usage::Transport::enter(Some(scope.clone()));
    let reply = chat(
        &req,
        60,
        &Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap();
    transport.settle(true).unwrap();
    scope
        .finish(
            Some(&reply),
            None,
            llm::usage::Billing::Local,
            transport.attempts(),
        )
        .unwrap();
    let counts = scope.snapshot().1.all;
    assert_eq!(counts.transport_attempts, 1);
    assert_eq!(counts.partial_reported_attempts, 0);
    assert_eq!(counts.input, reply.prompt_tokens.unwrap_or(0));
    assert_eq!(counts.output, reply.completion_tokens.unwrap_or(0));
    assert!(reply.text.to_lowercase().contains("pong"), "{reply:?}");
}

#[test]
fn helper_usage_is_retained_only_once_and_missing_fields_stay_incomplete() {
    for accepted in [false, true] {
        for reported in [false, true] {
            let scope = llm::usage::Scope::default().next_turn(0).unwrap();
            scope.begin(llm::usage::Billing::Local).unwrap();
            let transport = llm::usage::Transport::enter(Some(scope.clone()));
            llm::usage::transport_attempt().unwrap();
            let mut events = |_: &llm::StreamEvent| true;
            let mut fold = Fold {
                reply: Reply::default(),
                on_text: None,
                on_event: &mut events,
                generating: false,
            };
            let event = if reported {
                r#"{"type":"done","usage":{"prompt_tokens":8,"completion_tokens":2}}"#
            } else {
                r#"{"type":"done"}"#
            };
            assert!(fold.feed(event).unwrap());
            assert_eq!(fold.reply.prompt_tokens.is_some(), reported);
            transport.settle(accepted).unwrap();
            scope
                .finish(
                    accepted.then_some(&fold.reply),
                    None,
                    llm::usage::Billing::Local,
                    transport.attempts(),
                )
                .unwrap();
            let counts = scope.snapshot().1.all;
            assert_eq!(counts.input, if reported { 8 } else { 0 });
            assert_eq!(counts.output, if reported { 2 } else { 0 });
            assert_eq!(
                counts.partial_reported_attempts,
                u64::from(reported && !accepted)
            );
            assert_eq!(counts.reported_responses, u64::from(reported && accepted));
            assert_eq!(
                counts.unreported_attempts,
                u64::from(!reported || !accepted)
            );
            assert_eq!(counts.observed_cost_usd, None);
        }
    }
}
