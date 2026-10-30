use super::*;
use crate::{storage::artifact::ArtifactStore, tools::testkit};

fn entry(url: &str, status: u16) -> Value {
    json!({"request":{"method":"GET","url":url},"response":{"status":status},"time":-1,
        "timings":{"blocked":-1,"dns":-1,"connect":10,"send":0,"wait":3,"receive":2,"ssl":4}})
}
fn capture(entries: Vec<Value>) -> Value {
    json!({"log":{"version":"1.2","entries":entries}})
}
fn write_json(root: &std::path::Path, path: &str, value: &Value) {
    std::fs::write(root.join(path), serde_json::to_vec(value).unwrap()).unwrap();
}
fn inspect(ws: &Workspace, args: Value) -> Value {
    run(ws, &args).unwrap().data.unwrap()
}

#[test]
fn all_captured_strings_and_source_names_are_omitted_from_output_and_retention() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let secret = "CANARY_NETWORK_SECRET";
    let mut request = entry(
        &format!("https://{secret}:{secret}@{secret}.test/{secret}?{secret}={secret}#{secret}"),
        200,
    );
    request["request"]["method"] = json!(secret);
    request["request"]["headers"] = json!([{"name":secret,"value":secret}]);
    request["request"]["cookies"] = json!([{"name":secret,"value":secret}]);
    request["request"]["postData"] = json!({"text":secret});
    request["response"]["statusText"] = json!(secret);
    request["response"]["content"] = json!({"text":secret});
    request["response"]["headers"] = json!([{"name":secret,"value":secret}]);
    request["_webSocketMessages"] = json!([{"type":secret,"opcode":1,"time":4,"data":secret}]);
    write_json(
        work.path(),
        &format!("{secret}.har"),
        &capture(vec![request]),
    );
    write_json(
        work.path(),
        &format!("{secret}.json"),
        &json!({"openapi":"3.1.0","paths":{format!("/{secret}"):{"get":{"operationId":secret,"responses":{"200":{"description":secret}}}}}}),
    );
    let out = run(&ws, &json!({"action":"har","path":format!("{secret}.har"),"openapi_path":format!("{secret}.json")})).unwrap();
    let data = out.data.as_ref().unwrap();
    let complete = format!("{}{}", out.text, serde_json::to_string(data).unwrap());
    assert!(!complete.contains(secret), "secret in model output");
    assert_eq!(data["entries"][0]["method"], "OTHER");
    let store = ws.artifacts().unwrap();
    let record = store
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    let bytes = store.evidence_bytes(&record, MAX_OUTPUT).unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains(secret));
    assert!(
        !serde_json::to_string(&record.evidence)
            .unwrap()
            .contains(secret)
    );
    assert!(record.evidence.context.snapshot.unwrap().artifact.is_none());
}

#[test]
fn capture_order_unknown_timings_status_groups_and_explicit_redirects() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let mut first = entry("https://example.test/a?token=one", 302);
    first["response"]["redirectURL"] = json!("/b?token=two");
    let second = entry("https://example.test/b?token=two", 503);
    let third = entry("https://example.test/b?token=three", 200);
    write_json(
        work.path(),
        "capture.har",
        &capture(vec![first, second, third]),
    );
    let data = inspect(&ws, json!({"action":"har","path":"capture.har"}));
    assert_eq!(data["entries"][0]["explicit_redirect_target_sequence"], 2);
    assert_eq!(
        data["entries"][1]["explicit_redirect_target_sequence"],
        Value::Null
    );
    assert_eq!(
        data["entries"][2]["retry_heuristic"]["previous_same_method_endpoint_sequence"],
        2
    );
    assert_eq!(
        data["entries"][0]["total_time"]["state"],
        "unknown_minus_one"
    );
    assert_eq!(data["entries"][0]["timings"]["dns"]["ms"], Value::Null);
    assert_eq!(data["entries"][0]["timings"]["ssl"]["ms"], 4.0);
    assert_eq!(data["status_groups"]["5xx"], 1);
    let partial = inspect(
        &ws,
        json!({"action":"har","path":"capture.har","start":3,"count":1}),
    );
    assert_eq!(partial["coverage"]["omitted"], 2);
    assert_eq!(
        partial["entries"][0]["retry_heuristic"]["previous_same_method_endpoint_sequence"],
        Value::Null
    );
}

#[test]
fn openapi_status_matching_is_limited_and_template_ambiguity_is_not_conformance() {
    let spec = openapi::Spec::parse(json!({"openapi":"3.0.3","paths":{
        "/users/me":{"get":{"responses":{"200":{},"default":{}}}},
        "/users/{id}":{"get":{"responses":{"2XX":{}}}},
        "/ambiguous/{a}":{"get":{"responses":{"200":{}}}},
        "/ambiguous/{b}":{"get":{"responses":{"200":{}}}},
        "/refs":{"$ref":"https://must-not-fetch.test/path"}
    }}))
    .unwrap();
    let compare = |path, method, status| {
        spec.compare(
            Some(&Url::parse(&format!("https://example.test{path}")).unwrap()),
            method,
            Some(status),
        )
    };
    assert_eq!(
        compare("/users/me", "GET", 200)["path_match"],
        "literal_path"
    );
    assert_eq!(
        compare("/users/me", "GET", 503)["status_declaration"],
        "default"
    );
    assert_eq!(
        compare("/users/123", "GET", 204)["status_declaration"],
        "range"
    );
    assert_eq!(
        compare("/users/123", "GET", 404)["state"],
        "status_not_declared"
    );
    assert_eq!(
        compare("/users/123", "POST", 200)["state"],
        "method_not_declared"
    );
    assert_eq!(
        compare("/ambiguous/123", "GET", 200)["state"],
        "ambiguous_template"
    );
    assert_eq!(
        compare("/refs", "GET", 200)["state"],
        "path_ref_not_resolved"
    );
    assert_eq!(
        compare("/users/123/more", "GET", 200)["state"],
        "path_not_declared"
    );
}

#[test]
fn websocket_metadata_is_bounded_and_never_copies_message_data() {
    let mut entry = entry("wss://example.test/socket", 101);
    entry["_webSocketMessages"] = json!(
        (0..MAX_MESSAGES + 3)
            .map(|_| json!({"type":"receive","opcode":1,"time":1.5,"data":"SECRET"}))
            .collect::<Vec<_>>()
    );
    let result = websocket(&entry);
    assert_eq!(result["returned"], MAX_MESSAGES);
    assert_eq!(result["omitted"], 3);
    assert_eq!(result["messages"][0]["data_text_bytes"], 6);
    assert_eq!(result["messages"][0]["opcode"], 1);
    assert!(!result.to_string().contains("SECRET"));
}

#[test]
fn confinement_malformed_input_and_bounds_fail_without_secret_echo() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let outside = tempfile::tempdir().unwrap();
    write_json(outside.path(), "outside.har", &capture(vec![]));
    assert!(
        run(
            &ws,
            &json!({"action":"har","path":outside.path().join("outside.har")})
        )
        .is_err()
    );
    std::fs::write(
        work.path().join("bad.har"),
        br#"{"log": "SECRET", "SECRET": SECRET}"#,
    )
    .unwrap();
    let error = run(&ws, &json!({"action":"har","path":"bad.har"}))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("SECRET"));
    write_json(work.path(), "capture.har", &capture(vec![]));
    for args in [
        json!({"action":"har","path":"capture.har","start":0}),
        json!({"action":"har","path":"capture.har","count":201}),
        json!({"action":"SECRET","path":"capture.har"}),
        json!({"action":"har","path":"capture.har","SECRET":true}),
    ] {
        assert!(run(&ws, &args).is_err());
    }
    std::fs::write(work.path().join("large.har"), vec![b' '; MAX_INPUT + 1]).unwrap();
    assert!(
        run(&ws, &json!({"action":"har","path":"large.har"}))
            .unwrap_err()
            .to_string()
            .contains("16 MiB")
    );
    write_json(
        work.path(),
        "many.har",
        &capture(vec![entry("https://example.test", 200); MAX_ENTRIES + 1]),
    );
    assert!(
        run(&ws, &json!({"action":"har","path":"many.har"}))
            .unwrap_err()
            .to_string()
            .contains("entry limit")
    );
}

#[test]
fn captures_and_specs_honor_deny_read_policy() {
    use crate::tools::exec::sandbox::Policy;
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    write_json(work.path(), "capture.har", &capture(vec![]));
    write_json(
        work.path(),
        "api.json",
        &json!({"openapi":"3.0.3","paths":{}}),
    );
    for denied in ["capture.har", "api.json"] {
        let path = work.path().join(denied);
        for policy in [
            Policy::Dev {
                deny_read: vec![path.clone()],
                extra_write: vec![],
                allow_network: false,
            },
            Policy::Strict {
                deny_read: vec![path.clone()],
                extra_write: vec![],
                allow_network: false,
            },
        ] {
            let restricted = ws.clone().with_policy(policy);
            assert!(
                run(
                    &restricted,
                    &json!({"action":"har","path":"capture.har","openapi_path":"api.json"})
                )
                .is_err()
            );
        }
    }
}

#[test]
fn openapi_partial_segment_templates_match_literals_and_nonempty_parameters() {
    let spec = openapi::Spec::parse(json!({"openapi":"3.1.0","paths":{
        "/reports/{id}.json":{"get":{"responses":{"200":{}}}},
        "/files/{name}.{ext}":{"get":{"responses":{"200":{}}}},
        "/literal/a+({id})[x].json":{"get":{"responses":{"200":{}}}},
        "/reports/latest.json":{"get":{"responses":{"204":{}}}}
    }}))
    .unwrap();
    let compare = |path| {
        spec.compare(
            Some(&Url::parse(&format!("https://example.test{path}")).unwrap()),
            "GET",
            Some(200),
        )
    };
    for path in [
        "/reports/123.json",
        "/files/report.tar.gz",
        "/files/report.json",
        "/literal/a+(123)[x].json",
    ] {
        let result = compare(path);
        assert_eq!(result["state"], "status_declared", "path {path}");
        assert_eq!(result["path_match"], "path_template_only");
    }
    for path in [
        "/reports/.json",
        "/reports/123.xml",
        "/reports/123/456.json",
        "/files/.json",
        "/files/report.",
        "/files/report/json",
        "/literal/aaaa123x.json",
    ] {
        assert_eq!(compare(path)["state"], "path_not_declared", "path {path}");
    }
    assert_eq!(
        compare("/reports/latest.json")["path_match"],
        "literal_path"
    );
    assert_eq!(
        compare("/reports/latest.json")["state"],
        "status_not_declared"
    );
    let ambiguous = openapi::Spec::parse(json!({"openapi":"3.0.3","paths":{
        "/files/{name}.{ext}":{"get":{"responses":{"200":{}}}},
        "/files/{stem}.json":{"get":{"responses":{"200":{}}}}
    }}))
    .unwrap();
    assert_eq!(
        ambiguous.compare(
            Some(&Url::parse("https://example.test/files/report.json").unwrap()),
            "GET",
            Some(200)
        )["state"],
        "ambiguous_template"
    );
}

#[test]
fn openapi_paths_extensions_are_ignored_before_validation_counting_and_matching() {
    let mut paths = serde_json::Map::new();
    // More extensions than the route limit, including an oversized extension key, are valid.
    for index in 0..=openapi::MAX_PATHS {
        paths.insert(format!("x-metadata-{index}"), json!({"private":"SECRET"}));
    }
    paths.insert(format!("x-{}", "SECRET".repeat(1000)), json!(null));
    paths.insert("/real".into(), json!({"get":{"responses":{"200":{}}}}));
    let spec = openapi::Spec::parse(json!({"openapi":"3.1.0","paths":paths})).unwrap();
    let result = spec.compare(
        Some(&Url::parse("https://example.test/real").unwrap()),
        "GET",
        Some(200),
    );
    assert_eq!(result["state"], "status_declared");
    assert_eq!(result["route_id"], 1);
    assert!(!result.to_string().contains("SECRET"));
    let only_extensions =
        openapi::Spec::parse(json!({"openapi":"3.0.3","paths":{"x-private":{"/fake":{"get":{}}}}}))
            .unwrap();
    assert_eq!(
        only_extensions.compare(
            Some(&Url::parse("https://example.test/fake").unwrap()),
            "GET",
            Some(200)
        )["state"],
        "path_not_declared"
    );
    let too_many_routes: serde_json::Map<String, Value> = (0..=openapi::MAX_PATHS)
        .map(|i| (format!("/route-{i}"), json!({})))
        .collect();
    assert!(openapi::Spec::parse(json!({"openapi":"3.0.3","paths":too_many_routes})).is_err());
}

#[test]
fn malformed_templates_fail_closed_without_echoing_source_strings() {
    for path in [
        "/SECRET/{",
        "/SECRET/}",
        "/SECRET/{}",
        "/SECRET/{nested{id}}",
        "/SECRET/{cross/segment}",
    ] {
        let result = openapi::Spec::parse(
            json!({"openapi":"3.1.0","paths":{path:{"get":{"responses":{"200":{}}}}}}),
        );
        let error = result
            .err()
            .expect("malformed template accepted")
            .to_string();
        assert!(!error.contains("SECRET"));
    }
}
