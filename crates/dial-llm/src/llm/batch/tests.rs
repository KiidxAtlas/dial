use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

fn plan(provider: Provider) -> Plan {
    Plan {
        provider,
        model: "fixture".into(),
        max_tokens: 32,
        items: vec![
            Item {
                custom_id: "a".into(),
                prompt: "summarize A".into(),
            },
            Item {
                custom_id: "b".into(),
                prompt: "summarize B".into(),
            },
        ],
    }
}
#[test]
fn correlates_unordered_partial_results_and_rejects_invalid_ids() {
    for provider in [Provider::Openai, Provider::Anthropic] {
        let row = match provider {
            Provider::Openai => {
                json!({"custom_id":"b","response":{"status_code":200,"body":{"choices":[{"finish_reason":"stop","message":{"content":"B"}}]}}})
            }
            Provider::Anthropic => {
                json!({"custom_id":"b","result":{"type":"succeeded","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"B"}]}}})
            }
        };
        let bytes = serde_json::to_vec(&row).unwrap();
        let results = parse_results(provider, &bytes, &plan(provider)).unwrap();
        assert_eq!(results["a"], Outcome::Missing);
        assert_eq!(results["b"], Outcome::Succeeded("B".into()));
        let twice = format!("{row}\n{row}\n");
        assert!(parse_results(provider, twice.as_bytes(), &plan(provider)).is_err());
        assert!(
            parse_results(provider, b"{\"custom_id\":\"unexpected\"}", &plan(provider)).is_err()
        );
    }
}
#[test]
fn rejects_duplicate_items_and_oversized_prompts() {
    for (provider, limit) in [(Provider::Openai, 128), (Provider::Anthropic, 64)] {
        let mut bounded = plan(provider);
        bounded.items[0].custom_id = "x".repeat(limit);
        bounded.validate().unwrap();
        bounded.items[0].custom_id.push('x');
        assert!(bounded.validate().is_err(), "{provider:?} custom_id limit");
    }
    let mut plan = plan(Provider::Openai);
    plan.items[1].custom_id = "a".into();
    assert!(plan.validate().is_err());
    plan.items[1].custom_id = "b".into();
    plan.items[0].prompt = "x".repeat(MAX_BYTES);
    assert!(plan.validate().is_err());
}
fn server(replies: Vec<(&'static str, &'static str, String)>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for (method, path, body) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break bytes.len();
                }
            };
            let header = String::from_utf8(bytes).unwrap();
            assert!(
                header.starts_with(&format!("{method} {path} ")),
                "unexpected request {header}"
            );
            assert!(header.to_lowercase().contains("fixture-paid-key"));
            let length = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|n| n.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let mut request_body = vec![0; length];
            stream.read_exact(&mut request_body).unwrap();
            if path == "/files" {
                let wire = String::from_utf8(request_body).unwrap();
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("multipart/form-data; boundary=")
                );
                assert!(wire.contains("name=\"purpose\"\r\n\r\nbatch\r\n"));
                assert!(wire.contains("\"url\":\"/v1/chat/completions\""));
                assert!(wire.contains("\"custom_id\":\"a\""));
            } else if method == "POST" && !path.ends_with("/cancel") {
                let value: Value = serde_json::from_slice(&request_body).unwrap();
                if path == "/batches" {
                    assert_eq!(value["completion_window"], "24h");
                    assert_eq!(value["input_file_id"], "file_1");
                } else {
                    assert_eq!(
                        value["requests"][0]["params"]["messages"][0]["role"],
                        "user"
                    );
                    assert_eq!(value["requests"][0]["params"]["max_tokens"], 32);
                }
            }
            let _ = header_end;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (base, handle)
}
#[test]
fn native_submit_status_cancel_and_results_use_fixed_routes() {
    for provider in [Provider::Openai, Provider::Anthropic] {
        let mut replies = Vec::new();
        if provider == Provider::Openai {
            replies.push(("POST", "/files", json!({"id":"file_1"}).to_string()));
        }
        let route = provider.route();
        replies.push(("POST", route, json!({"id":"job_1"}).to_string()));
        let status_path = if provider == Provider::Openai {
            "/batches/job_1"
        } else {
            "/messages/batches/job_1"
        };
        let cancel_path = if provider == Provider::Openai {
            "/batches/job_1/cancel"
        } else {
            "/messages/batches/job_1/cancel"
        };
        replies.push(("POST", cancel_path, "{}".into()));
        let status = if provider == Provider::Openai {
            json!({"status":"completed","output_file_id":"file_out"})
        } else {
            json!({"processing_status":"ended"})
        };
        replies.push(("GET", status_path, status.to_string()));
        let (result_path, row) = if provider == Provider::Openai {
            (
                "/files/file_out/content",
                json!({"custom_id":"a","response":{"status_code":200,"body":{"choices":[{"finish_reason":"stop","message":{"content":"summary"}}]}}}),
            )
        } else {
            (
                "/messages/batches/job_1/results",
                json!({"custom_id":"a","result":{"type":"succeeded","message":{"stop_reason":"end_turn","content":[{"text":"summary"}]}}}),
            )
        };
        replies.push(("GET", result_path, row.to_string()));
        let (base, handle) = server(replies);
        let client = Client::new(provider, base, "fixture-paid-key".into());
        let plan = plan(provider);
        let id = client.submit(&plan).unwrap();
        assert_eq!(id, "job_1");
        client.cancel(&id).unwrap();
        assert_eq!(
            client.results(&id, &plan).unwrap()["a"],
            Outcome::Succeeded("summary".into())
        );
        handle.join().unwrap();
    }
}

#[test]
fn cancellation_remains_nonterminal_and_result_states_are_not_success() {
    let (base, handle) = server(vec![(
        "GET",
        "/messages/batches/job_1",
        json!({"processing_status":"canceling"}).to_string(),
    )]);
    let client = Client::new(Provider::Anthropic, base, "fixture-paid-key".into());
    assert!(client.results("job_1", &plan(Provider::Anthropic)).is_err());
    handle.join().unwrap();
    for (kind, expected) in [
        ("canceled", Outcome::Canceled),
        ("expired", Outcome::Expired),
        ("errored", Outcome::Errored),
    ] {
        let row = json!({"custom_id":"a","result":{"type":kind}});
        let result = parse_results(
            Provider::Anthropic,
            row.to_string().as_bytes(),
            &plan(Provider::Anthropic),
        )
        .unwrap();
        assert_eq!(result["a"], expected);
    }
}

#[test]
fn stop_reasons_and_refusals_retain_partial_output_without_success() {
    for (provider, reason, refused, complete) in [
        (Provider::Openai, "stop", false, true),
        (Provider::Openai, "length", false, false),
        (Provider::Openai, "content_filter", true, false),
        (Provider::Openai, "tool_calls", false, false),
        (Provider::Openai, "new_reason", false, false),
        (Provider::Openai, "", false, false),
        (Provider::Anthropic, "end_turn", false, true),
        (Provider::Anthropic, "stop_sequence", false, true),
        (Provider::Anthropic, "max_tokens", false, false),
        (Provider::Anthropic, "refusal", true, false),
        (Provider::Anthropic, "tool_use", false, false),
        (Provider::Anthropic, "pause_turn", false, false),
        (Provider::Anthropic, "new_reason", false, false),
        (Provider::Anthropic, "", false, false),
    ] {
        let stop = if reason.is_empty() {
            Value::Null
        } else {
            json!(reason)
        };
        let row = match provider {
            Provider::Openai => {
                json!({"custom_id":"a","response":{"status_code":200,"body":{"choices":[{"finish_reason":stop,"message":{"content":"partial"}}]}}})
            }
            Provider::Anthropic => {
                json!({"custom_id":"a","result":{"type":"succeeded","message":{"stop_reason":stop,"content":[{"text":"partial"}]}}})
            }
        };
        let result = parse_results(provider, row.to_string().as_bytes(), &plan(provider)).unwrap();
        let fallback = match provider {
            Provider::Openai => "missing_finish_reason",
            Provider::Anthropic => "missing_stop_reason",
        };
        let reason = if reason.is_empty() { fallback } else { reason }.to_owned();
        let expected = if complete {
            Outcome::Succeeded("partial".into())
        } else if refused {
            Outcome::Refused {
                output: "partial".into(),
                reason,
            }
        } else {
            Outcome::Incomplete {
                output: "partial".into(),
                reason,
            }
        };
        assert_eq!(result["a"], expected, "{provider:?} row {row}");
    }
    for content in [Value::Null, json!("partial")] {
        let row = json!({"custom_id":"a","response":{"status_code":200,"body":{"choices":[{"finish_reason":"stop","message":{"content":content,"refusal":"policy refusal"}}]}}});
        let result = parse_results(
            Provider::Openai,
            row.to_string().as_bytes(),
            &plan(Provider::Openai),
        )
        .unwrap();
        assert_eq!(
            result["a"],
            Outcome::Refused {
                output: content.as_str().unwrap_or_default().to_owned(),
                reason: "policy refusal".into()
            }
        );
    }
}

#[test]
fn legacy_success_serde_and_all_retained_text_redaction() {
    let legacy = json!({"state":"succeeded","text":"old summary"});
    let success: Outcome = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(success, Outcome::Succeeded("old summary".into()));
    assert_eq!(serde_json::to_value(success).unwrap(), legacy);
    for mut outcome in [
        Outcome::Succeeded("secret".into()),
        Outcome::Incomplete {
            output: "secret".into(),
            reason: "secret".into(),
        },
        Outcome::Refused {
            output: "secret".into(),
            reason: "secret".into(),
        },
    ] {
        outcome.redact(|s| s.replace("secret", "[REDACTED]"));
        let encoded = serde_json::to_string(&outcome).unwrap();
        assert!(!encoded.contains("secret"));
        assert_eq!(serde_json::from_str::<Outcome>(&encoded).unwrap(), outcome);
    }
}

#[test]
fn paid_admission_bounds_result_envelope_and_native_wire_before_network() {
    for provider in [Provider::Openai, Provider::Anthropic] {
        let mut bounded = plan(provider);
        bounded.items.truncate(1);
        // One-token boundary on the conservative 4 MiB collection envelope.
        bounded.max_tokens = 2719;
        bounded.validate_submission().unwrap();
        bounded.max_tokens = 2720;
        assert!(
            bounded
                .validate_submission()
                .unwrap_err()
                .to_string()
                .contains("output envelope")
        );
        // Formerly legal item/token maxima are still readable, but not paid admission.
        bounded.max_tokens = 16384;
        bounded.validate().unwrap();
        assert!(bounded.validate_submission().is_err());
        bounded.max_tokens = 1024;
        bounded.items.push(Item {
            custom_id: "b".into(),
            prompt: "summary".into(),
        });
        bounded.validate_submission().unwrap();
        bounded.items.push(Item {
            custom_id: "c".into(),
            prompt: "summary".into(),
        });
        assert!(bounded.validate_submission().is_err());

        bounded.items.truncate(1);
        bounded.max_tokens = 1;
        bounded.items[0].prompt.clear();
        let overhead = serde_json::to_vec(&bounded).unwrap().len();
        bounded.items[0].prompt = "x".repeat(MAX_BYTES - overhead);
        bounded.validate().unwrap();
        assert_eq!(serde_json::to_vec(&bounded).unwrap().len(), MAX_BYTES);
        let error = bounded.validate_submission().unwrap_err().to_string();
        assert!(error.contains("wire payload"), "{provider:?}: {error}");
        // A dead endpoint proves invalid work is rejected locally, not uploaded.
        let client = Client::new(
            provider,
            "http://127.0.0.1:0".into(),
            "fixture-paid-key".into(),
        );
        assert!(
            client
                .submit(&bounded)
                .unwrap_err()
                .to_string()
                .contains("wire payload")
        );
    }
}
