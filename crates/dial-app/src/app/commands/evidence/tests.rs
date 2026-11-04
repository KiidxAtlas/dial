use super::*;
#[test]
fn commands_parse_citations_without_reopening_paths() {
    let (tool, args) = parse("compare artifact://a artifact://b").unwrap().unwrap();
    assert_eq!(tool, "investigate");
    assert_eq!(args["reference"], "artifact://a");
    assert_eq!(parse("inspect").unwrap(), None);
    assert_eq!(parse("compare a").unwrap(), None);
    let (tool, args) = parse("correct artifact://a   this happened later")
        .unwrap()
        .unwrap();
    assert_eq!(tool, "workbench");
    assert_eq!(args["correction"], "this happened later");
    assert!(parse("fetch https://example.test").is_err());
}

#[test]
fn named_checks_use_only_the_recorded_immutable_reference() {
    let reference = format!("artifact://{}", "a".repeat(64));
    for current in [true, false] {
        let summary = json!({"checks_current": current, "recorded_checks": [
            {"name":"cargo test", "status":"passed", "evidence_ref":reference, "reused":true},
            {"name":"lint", "status":"failed", "evidence_ref":"artifact://other"}
        ]});
        assert_eq!(
            check_request(&summary, "cargo test").unwrap(),
            json!({"action":"inspect", "references":[reference]})
        );
        let list = check_list(&summary);
        assert!(list.contains(&reference));
        assert!(list.contains("reused, not rerun"));
        assert!(list.contains("/evidence check cargo test"));
        assert!(list.contains(if current {
            "live freshness unknown"
        } else {
            "Not current"
        }));
    }
}

#[test]
fn absent_and_ambiguous_checks_do_not_fall_back_to_other_output() {
    for summary in [
        Value::Null,
        json!({"recorded_checks":[{"name":"lint", "evidence_ref":"artifact://other"}]}),
        json!({"recorded_checks":[{"name":"test", "status":"passed"}]}),
        json!({"recorded_checks":[{"name":"test", "evidence_ref":""}]}),
        json!({"recorded_checks":[{"name":"test", "evidence_ref":"artifact://a"}, {"name":"test", "evidence_ref":"artifact://b"}]}),
    ] {
        assert!(check_request(&summary, "test").is_err(), "{summary}");
    }
    assert!(check_list(&Value::Null).contains("Exact logs unavailable"));
    assert!(
        check_list(&json!({"recorded_checks":[{"name":"test"}]}))
            .contains("exact retained log unavailable")
    );
}
