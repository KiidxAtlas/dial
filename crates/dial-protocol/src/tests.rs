use super::*;
use std::collections::BTreeSet;

fn schema() -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/rpc-schema.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn consts(schema: &serde_json::Value, def: &str, field: &str) -> BTreeSet<String> {
    schema["$defs"][def]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|variant| variant["properties"][field]["const"].as_str())
        .map(str::to_owned)
        .collect()
}

#[test]
fn method_and_event_tables_are_unique() {
    let methods: BTreeSet<_> = METHODS.iter().map(|m| m.name).collect();
    assert_eq!(methods.len(), METHODS.len(), "duplicate method");
    let events: BTreeSet<_> = EVENTS.iter().collect();
    assert_eq!(events.len(), EVENTS.len(), "duplicate event");
}

#[test]
fn every_event_in_the_schema_is_named_and_carries_the_envelope() {
    let schema = schema();
    let named: BTreeSet<String> = EVENTS.iter().map(|e| (*e).to_owned()).collect();
    assert_eq!(consts(&schema, "event", "event"), named);
    for variant in schema["$defs"]["event"]["oneOf"].as_array().unwrap() {
        for field in ENVELOPE_FIELDS {
            assert!(
                variant["properties"].get(*field).is_some(),
                "{} lacks envelope field {field}",
                variant["properties"]["event"]["const"]
            );
        }
    }
}

#[test]
fn schema_requests_match_the_method_table() {
    let schema = schema();
    let methods: BTreeSet<String> = METHODS.iter().map(|m| m.name.to_owned()).collect();
    let requested = consts(&schema, "request", "method");
    // The schema documents every served method; it may also hold ACP-only or internal entries.
    let missing: Vec<_> = methods.difference(&requested).collect();
    assert!(
        missing.is_empty(),
        "methods missing from schema: {missing:?}"
    );
}

#[test]
fn error_codes_round_trip_and_match_the_schema() {
    for code in ErrorCode::ALL {
        assert_eq!(ErrorCode::parse(code.as_str()), *code);
    }
    assert_eq!(ErrorCode::parse("from_a_newer_engine"), ErrorCode::Failed);
    let schema = schema();
    let documented: Vec<&str> = schema["$defs"]["response"]["oneOf"][1]["properties"]["error"]
        ["properties"]["code"]["x-known-values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let ours: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(documented, ours);
}

#[test]
fn read_only_classification_is_conservative() {
    assert!(is_read_only("state") && is_read_only("list_sessions"));
    assert!(!is_read_only("send") && !is_read_only("set_setting"));
    assert!(!is_read_only("not_a_method"));
    assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&PROTOCOL_VERSION));
}
