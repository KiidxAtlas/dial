use crate::config::telemetry::TelemetryConfig;

use super::*;
use std::io::{BufRead, BufReader, Read, Write};

fn settings(base: &str) -> OtlpSettings {
    let config = TelemetryConfig {
        enabled: true,
        endpoint: Some(base.into()),
        ..Default::default()
    };
    OtlpSettings::resolve_with(&config, |_| None).unwrap()
}

fn request_event(session: &str, error: bool) -> Event {
    Event::Request(Box::new(Request {
        session: session.into(),
        project: "proj-1234abcd".into(),
        model: "test-model".into(),
        input: Some(1200),
        output: Some(300),
        cache_read: Some(1000),
        cache_write: None,
        reasoning: Some(0),
        cost_usd: Some(0.25),
        duration: Duration::from_millis(1500),
        finish_reason: "stop".into(),
        tool_calls: 1,
        error,
        end: SystemTime::now(),
        wire: None,
    }))
}

#[test]
fn disabled_or_endpointless_config_exports_nothing() {
    assert!(
        OtlpSettings::resolve_with(&TelemetryConfig::default(), |_| Some(
            "http://x:4318".into()
        ))
        .is_none()
    );
    let enabled = TelemetryConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(OtlpSettings::resolve_with(&enabled, |_| None).is_none());
    let from_env = OtlpSettings::resolve_with(&enabled, |name| {
        (name == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://collector:4318/".to_owned())
    })
    .unwrap();
    assert_eq!(
        from_env.traces_url.as_deref(),
        Some("http://collector:4318/v1/traces")
    );
    let bad = TelemetryConfig {
        enabled: true,
        endpoint: Some("ftp://x".into()),
        ..Default::default()
    };
    assert!(OtlpSettings::resolve_with(&bad, |_| None).is_none());
}

#[test]
fn env_headers_and_resource_attributes_are_parsed() {
    let config = TelemetryConfig {
        enabled: true,
        endpoint: Some("https://otel.example".into()),
        headers: BTreeMap::from([("x-team".to_owned(), "core".to_owned())]),
        ..Default::default()
    };
    let resolved = OtlpSettings::resolve_with(&config, |name| match name {
        "OTEL_EXPORTER_OTLP_HEADERS" => Some("authorization=Bearer%20abc, x-team=old".into()),
        "OTEL_RESOURCE_ATTRIBUTES" => Some("deployment.environment=dev".into()),
        "OTEL_SERVICE_NAME" => Some("dial-ci".into()),
        _ => None,
    })
    .unwrap();
    assert!(
        resolved
            .headers
            .contains(&("authorization".into(), "Bearer abc".into()))
    );
    assert!(resolved.headers.contains(&("x-team".into(), "core".into())));
    assert!(!resolved.headers.contains(&("x-team".into(), "old".into())));
    assert!(
        resolved
            .resource
            .contains(&("service.name".into(), "dial-ci".into()))
    );
    assert!(
        resolved
            .resource
            .contains(&("deployment.environment".into(), "dev".into()))
    );
}

#[test]
fn otlp_json_payloads_have_the_protocol_shape() {
    let mut exporter = Exporter::new(settings("http://127.0.0.1:4318"));
    exporter.observe(&request_event("s1", false));
    exporter.observe(&Event::Tool(Box::new(Tool {
        facts: ExecutionFacts::default(),
        status: crate::tools::ToolStatus::Error,
        effects: crate::tools::surface::outcome::Effects::Known,
        session: "s1".into(),
        project: "proj-1234abcd".into(),
        tool: "read".into(),
        duration: Duration::from_millis(12),
        error: true,
        end: SystemTime::now(),
    })));
    exporter.observe(&Event::Turn(Turn {
        session: "s1".into(),
        project: "proj-1234abcd".into(),
        outcome: "completed".into(),
        steps: 2,
        duration: Duration::from_secs(3),
        end: SystemTime::now(),
    }));
    let payloads: BTreeMap<String, Value> = exporter
        .take_payloads(unix_ns(SystemTime::now()))
        .into_iter()
        .collect();
    assert_eq!(payloads.len(), 3);

    let traces = &payloads["http://127.0.0.1:4318/v1/traces"];
    let resource = &traces["resourceSpans"][0]["resource"]["attributes"];
    assert!(
        resource
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "service.name" && a["value"]["stringValue"] == "dial")
    );
    let spans = traces["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap();
    assert_eq!(spans.len(), 3);
    let root = spans
        .iter()
        .find(|s| s["name"] == "invoke_agent dial")
        .unwrap();
    assert_eq!(root["parentSpanId"], "");
    for span in spans {
        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(span["traceId"], root["traceId"]);
        assert!(
            span["startTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                <= span["endTimeUnixNano"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
        );
        if span != root {
            assert_eq!(span["parentSpanId"], root["spanId"]);
        }
    }
    let chat = spans
        .iter()
        .find(|s| s["name"] == "chat test-model")
        .unwrap();
    assert_eq!(chat["kind"], 3);
    let input = chat["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "gen_ai.usage.input_tokens")
        .unwrap();
    assert_eq!(input["value"]["intValue"], "1200");
    let tool = spans
        .iter()
        .find(|s| s["name"] == "execute_tool read")
        .unwrap();
    assert_eq!(tool["status"]["code"], 2);

    let metrics = &payloads["http://127.0.0.1:4318/v1/metrics"]["resourceMetrics"][0]["scopeMetrics"]
        [0]["metrics"];
    let find = |name: &str| {
        metrics
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .cloned()
            .unwrap()
    };
    let tokens = find("gen_ai.client.token.usage");
    assert_eq!(tokens["histogram"]["aggregationTemporality"], 1);
    let points = tokens["histogram"]["dataPoints"].as_array().unwrap();
    assert_eq!(
        points.len(),
        3,
        "zero-valued reasoning and absent cache writes are not recorded"
    );
    for point in points {
        assert_eq!(
            point["bucketCounts"].as_array().unwrap().len(),
            TOKEN_BOUNDS.len() + 1
        );
        assert_eq!(point["count"], "1");
    }
    let cost = find("dial.agent.chat.cost.usd");
    assert_eq!(cost["sum"]["dataPoints"][0]["asDouble"], 0.25);
    assert_eq!(cost["sum"]["isMonotonic"], true);
    let tool_calls = find("dial.agent.tool.calls");
    assert_eq!(tool_calls["sum"]["dataPoints"][0]["asInt"], "1");
    assert_eq!(
        find("dial.agent.steps")["sum"]["dataPoints"][0]["asInt"],
        "2"
    );

    let logs =
        &payloads["http://127.0.0.1:4318/v1/logs"]["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
    assert_eq!(logs[0]["body"]["stringValue"], "agent run completed");
    assert_eq!(logs[0]["severityNumber"], 9);
    assert_eq!(logs[0]["traceId"], root["traceId"]);

    assert!(
        exporter
            .take_payloads(unix_ns(SystemTime::now()))
            .is_empty(),
        "buffers drain"
    );
}

#[test]
fn histogram_buckets_follow_explicit_bounds() {
    let mut hist = Hist::new(&[1.0, 10.0]);
    for value in [0.5, 1.0, 5.0, 10.0, 11.0] {
        hist.record(&[1.0, 10.0], value);
    }
    assert_eq!(hist.buckets, vec![2, 2, 1]);
    assert_eq!(hist.count, 5);
    assert_eq!(hist.min, 0.5);
    assert_eq!(hist.max, 11.0);
}

#[test]
fn flush_posts_json_to_the_signal_path() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (&stream)
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
            .unwrap();
        (line, body)
    });
    let config = TelemetryConfig {
        enabled: true,
        endpoint: Some(format!("http://127.0.0.1:{port}")),
        metrics: false,
        logs: false,
        ..Default::default()
    };
    let mut exporter = Exporter::new(OtlpSettings::resolve_with(&config, |_| None).unwrap());
    exporter.observe(&request_event("s2", true));
    exporter.flush();
    let (line, body) = server.join().unwrap();
    assert!(line.starts_with("POST /v1/traces "), "{line}");
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
        2
    );
    assert!(exporter.last_error.is_none());
}

fn execution_event(execution: &crate::runtime::execution::Execution) -> Event {
    Event::Tool(Box::new(Tool {
        facts: ExecutionFacts::from(execution),
        status: crate::tools::ToolStatus::Ok,
        effects: execution.effects,
        session: "facts-test".into(),
        project: "project".into(),
        tool: "bg".into(),
        duration: Duration::ZERO,
        error: false,
        end: SystemTime::now(),
    }))
}

#[test]
fn successful_observation_keeps_failed_child_and_incomplete_output_distinct() {
    use crate::runtime::execution::*;
    let execution = Execution {
        observation: Some(Observation {
            kind: ObservationKind::Wait,
            satisfied: true,
        }),
        process: Some(ProcessOutcome {
            started: true,
            exit_code: Some(7),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: false,
        }),
        output: Some(OutputIntegrity {
            capture: Completeness::Partial,
            retention: Completeness::Complete,
            presentation: Completeness::Partial,
            original_bytes: Some(100),
            retained_bytes: Some(100),
            omitted_bytes: Some(80),
            presented_bytes: Some(20),
            artifact: Some("artifact://private-secret".into()),
        }),
        changed_paths: vec!["/private/secret".into()],
        ..Default::default()
    };
    let mut exporter = Exporter::new(settings("http://127.0.0.1:4318"));
    exporter.observe(&execution_event(&execution));
    let serialized =
        serde_json::to_string(&exporter.take_payloads(unix_ns(SystemTime::now()))).unwrap();
    for expected in [
        "dial.observation.satisfied",
        "true",
        "dial.process.outcome",
        "failed",
        "dial.output.capture",
        "partial",
        "dial.output.retention",
        "complete",
        "dial.agent.output.bytes",
        "80",
    ] {
        assert!(serialized.contains(expected), "missing {expected}");
    }
    assert!(!serialized.contains("private"));
    assert!(!serialized.contains("artifact://"));
    assert!(!serialized.contains("dial.agent.errors"));
}

#[test]
fn integration_health_dimensions_and_failure_are_independent() {
    use crate::runtime::{execution::Execution, integration::*};
    let execution = Execution {
        integration: Some(IntegrationExecution {
            health: IntegrationHealth {
                connection: ConnectionHealth::Connected,
                notifications: NotificationHealth::Degraded,
                catalog: CatalogFreshness::Stale,
                generation: 3,
                transitions: 4,
            },
            state_changes: 2,
            dispatch: DispatchState::Dispatched,
            settlement: Settlement::Reply,
            failure: Some(IntegrationFailure::Remote),
        }),
        ..Default::default()
    };
    let facts = ExecutionFacts::from(&execution);
    assert_eq!(facts.connection.as_deref(), Some("connected"));
    assert_eq!(facts.notifications.as_deref(), Some("degraded"));
    assert_eq!(facts.catalog.as_deref(), Some("stale"));
    let mut exporter = Exporter::new(settings("http://127.0.0.1:4318"));
    exporter.observe(&execution_event(&execution));
    let serialized =
        serde_json::to_string(&exporter.take_payloads(unix_ns(SystemTime::now()))).unwrap();
    for expected in [
        "dial.integration.failure",
        "remote",
        "dial.integration.dispatch",
        "dispatched",
        "dial.integration.settlement",
        "reply",
        "dial.agent.integration.state_changes",
    ] {
        assert!(serialized.contains(expected), "missing {expected}");
    }
}

#[test]
fn full_or_disconnected_ingress_drops_without_blocking_and_counts_loss() {
    let before = loss_counters().ingress;
    let (tx, rx) = mpsc::sync_channel(1);
    let route = Route {
        stats: None,
        otlp: None,
    };
    enqueue(
        &tx,
        Msg::Event(Box::new(route.clone()), request_event("first", false)),
    );
    enqueue(
        &tx,
        Msg::Event(Box::new(route.clone()), request_event("overflow", false)),
    );
    assert!(loss_counters().ingress > before);
    drop(rx);
    enqueue(
        &tx,
        Msg::Event(Box::new(route), request_event("disconnected", false)),
    );
    assert!(loss_counters().ingress >= before + 2);
}

#[test]
fn unavailable_stats_are_counted_without_stopping_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let before = loss_counters().stats;
    let (tx, rx) = mpsc::sync_channel(4);
    let hub = thread::spawn(move || run(rx));
    tx.send(Msg::Event(
        Box::new(Route {
            stats: Some(dir.path().to_path_buf()),
            otlp: None,
        }),
        request_event("stats-loss", false),
    ))
    .unwrap();
    let (done, wait) = mpsc::channel();
    tx.send(Msg::Stop(done)).unwrap();
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    hub.join().unwrap();
    assert!(loss_counters().stats > before);
}

#[test]
fn failed_export_is_observable_and_batch_does_not_accumulate() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut config = settings(&format!("http://{address}"));
    config.metrics_url = None;
    config.logs_url = None;
    config.timeout = Duration::from_secs(1);
    let mut exporter = Exporter::new(config);
    let before = loss_counters().export;
    exporter.observe(&request_event("export-loss", false));
    exporter.flush();
    assert!(loss_counters().export > before);
    assert!(exporter.last_error.is_some());
    assert!(exporter.spans.is_empty());
    assert!(
        exporter
            .take_payloads(unix_ns(SystemTime::now()))
            .is_empty()
    );
}
