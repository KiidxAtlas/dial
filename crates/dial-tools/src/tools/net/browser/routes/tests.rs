use super::*;
use std::cell::Cell;

fn rule(id: u64, pattern: &str, resource_type: Option<&str>, kind: Kind) -> Rule {
    Rule {
        id,
        pattern: pattern.into(),
        resource_type: resource_type.map(str::to_owned),
        kind,
    }
}

#[test]
fn glob_star_stays_within_a_segment_and_double_star_crosses_it() {
    assert!(glob_matches("https://a.test/*.js", "https://a.test/app.js"));
    assert!(!glob_matches(
        "https://a.test/*.js",
        "https://a.test/x/app.js"
    ));
    assert!(glob_matches(
        "https://a.test/**/app.js",
        "https://a.test/x/y/app.js"
    ));
    assert!(glob_matches("**", "https://anything.test/a/b?c=d"));
}

#[test]
fn glob_must_match_the_whole_url() {
    assert!(!glob_matches("https://a.test/", "https://a.test/page"));
    assert!(!glob_matches("a.test", "https://a.test/"));
    assert!(glob_matches("https://a.test/?x", "https://a.test/?x"));
}

#[test]
fn guard_refusal_wins_over_a_matching_route_and_routes_are_not_consulted_past_it() {
    let rules = [rule(1, "**", None, Kind::Fulfill)];
    let decision = decide("https://blocked.test/", "xhr", &rules, || {
        Err("refusing browser request to blocked.test".into())
    });
    assert_eq!(
        decision,
        Decision::Blocked("refusing browser request to blocked.test".into())
    );
}

#[test]
fn guard_runs_before_routes_and_is_skipped_without_rules() {
    let guard_calls = Cell::new(0);
    let rules = [rule(1, "**", None, Kind::Abort)];
    let decision = decide("https://ok.test/", "document", &rules, || {
        guard_calls.set(guard_calls.get() + 1);
        Ok(())
    });
    assert_eq!(
        decision,
        Decision::Matched {
            id: 1,
            kind: Kind::Abort
        }
    );
    assert_eq!(guard_calls.get(), 1);

    let decision = decide("https://ok.test/", "document", &[], || {
        guard_calls.set(guard_calls.get() + 1);
        Ok(())
    });
    assert_eq!(decision, Decision::Continue);
    assert_eq!(guard_calls.get(), 1);
}

#[test]
fn first_registered_matching_route_wins_and_resource_type_filters() {
    let rules = [
        rule(1, "https://a.test/**", Some("image"), Kind::Abort),
        rule(2, "https://a.test/**", None, Kind::Fulfill),
    ];
    assert_eq!(
        decide("https://a.test/logo.png", "image", &rules, || Ok(())),
        Decision::Matched {
            id: 1,
            kind: Kind::Abort
        }
    );
    assert_eq!(
        decide("https://a.test/data", "xhr", &rules, || Ok(())),
        Decision::Matched {
            id: 2,
            kind: Kind::Fulfill
        }
    );
    assert_eq!(
        decide("https://b.test/", "xhr", &rules, || Ok(())),
        Decision::Continue
    );
}

#[test]
fn rules_from_json_rejects_unknown_kinds_and_oversized_lists() {
    let ok = rules_from_json(
        &json!([{"id": 3, "pattern": "**", "resource_type": null, "kind": "abort"}]),
    )
    .unwrap();
    assert_eq!(ok, vec![rule(3, "**", None, Kind::Abort)]);
    assert!(rules_from_json(&json!([{"id": 3, "pattern": "**", "kind": "redirect"}])).is_err());
    let many: Vec<Value> = (0..=ROUTES_MAX)
        .map(|id| json!({"id": id, "pattern": "**", "kind": "continue"}))
        .collect();
    assert!(rules_from_json(&Value::Array(many)).is_err());
}

#[test]
fn route_defaults_to_fulfill_only_when_response_fields_are_given() {
    let continued = parse("route", &json!({"pattern": "https://a.test/**"})).unwrap();
    assert_eq!(continued["route_kind"], "continue");
    assert!(continued["response"].is_null());

    let fulfilled = parse(
        "route",
        &json!({"pattern": "https://a.test/api", "body": {"ok": true}}),
    )
    .unwrap();
    assert_eq!(fulfilled["route_kind"], "fulfill");
    assert_eq!(fulfilled["response"]["status"], 200);
    assert_eq!(fulfilled["response"]["content_type"], "application/json");
}

#[test]
fn route_rejects_fulfill_fields_on_abort_and_unsafe_values() {
    assert!(
        parse(
            "route",
            &json!({"pattern": "**", "route_action": "abort", "status": 404})
        )
        .is_err()
    );
    assert!(parse("route", &json!({"pattern": "**", "status": 99})).is_err());
    assert!(parse("route", &json!({"pattern": ""})).is_err());
    assert!(
        parse(
            "route",
            &json!({"pattern": "**", "headers": {"X-A": "ok\r\nSet-Cookie: x"}})
        )
        .is_err()
    );
    assert!(parse("route", &json!({"pattern": "**", "resource_type": "XHR"})).is_err());
    let big = "z".repeat(FULFILL_BODY_MAX_BYTES + 1);
    assert!(parse("route", &json!({"pattern": "**", "body": big})).is_err());
}

#[test]
fn unroute_pattern_is_optional_and_validated_when_present() {
    assert!(parse("unroute", &json!({})).unwrap()["pattern"].is_null());
    assert!(parse("unroute", &json!({"pattern": ""})).is_err());
}
