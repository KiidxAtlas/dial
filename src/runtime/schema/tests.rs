use super::*;
use serde_json::json;

fn findings() -> Value {
    json!({"type":"object","required":["summary","files"],"additionalProperties":false,"properties":{
        "summary":{"type":"string"},
        "severity":{"enum":["low","high"]},
        "files":{"type":"array","minItems":1,"items":{"type":"object","required":["path"],"properties":{"path":{"type":"string"},"line":{"type":"integer","minimum":1}}}}}})
}

#[test]
fn accepts_a_conforming_value() {
    let value =
        json!({"summary":"ok","severity":"low","files":[{"path":"a.rs","line":3},{"path":"b.rs"}]});
    assert_eq!(validate(&findings(), &value), Ok(()));
}

#[test]
fn names_the_location_of_each_kind_of_violation() {
    let s = findings();
    let cases = [
        (json!("text"), "$: expected object, got string"),
        (json!({"files":[]}), "missing required property \"summary\""),
        (
            json!({"summary":1,"files":[{"path":"a"}]}),
            "$.summary: expected string, got number",
        ),
        (
            json!({"summary":"x","severity":"mid","files":[{"path":"a"}]}),
            "$.severity",
        ),
        (json!({"summary":"x","files":[]}), "needs at least 1"),
        (
            json!({"summary":"x","files":[{"path":"a"},{"line":2}]}),
            "$.files[1]: missing required property \"path\"",
        ),
        (
            json!({"summary":"x","files":[{"path":"a","line":0}]}),
            "$.files[0].line: 0 is below the minimum 1",
        ),
        (
            json!({"summary":"x","files":[{"path":"a"}],"extra":true}),
            "unexpected property \"extra\"",
        ),
    ];
    for (value, expected) in cases {
        let error = validate(&s, &value).unwrap_err();
        assert!(error.contains(expected), "{value}: {error}");
    }
}

#[test]
fn integers_accept_whole_floats_and_types_may_be_unions() {
    assert!(validate(&json!({"type":"integer"}), &json!(3.0)).is_ok());
    assert!(validate(&json!({"type":"integer"}), &json!(3.5)).is_err());
    let nullable = json!({"type":["string","null"]});
    assert!(validate(&nullable, &json!(null)).is_ok() && validate(&nullable, &json!(4)).is_err());
}

#[test]
fn schemas_the_validator_cannot_honour_are_rejected_up_front() {
    assert!(check_schema(&findings()).is_ok());
    for bad in [
        json!("string"),
        json!({"type":"text"}),
        json!({"oneOf":[]}),
        json!({"properties":{"a":{"$ref":"#/x"}}}),
        json!({"required":"a"}),
        json!({"enum":[]}),
        json!({"items":{"type":7}}),
    ] {
        assert!(check_schema(&bad).is_err(), "{bad}");
    }
}

#[test]
fn refs_resolve_into_defs_and_recurse() {
    let tree = json!({
        "$defs": {"node": {"type":"object","required":["name"],"properties":{
            "name":{"type":"string"},
            "children":{"type":"array","items":{"$ref":"#/$defs/node"}}}}},
        "$ref": "#/$defs/node"
    });
    assert!(check_schema(&tree).is_ok());
    assert!(
        validate(
            &tree,
            &json!({"name":"a","children":[{"name":"b","children":[]}]})
        )
        .is_ok()
    );
    let error = validate(
        &tree,
        &json!({"name":"a","children":[{"name":"b","children":[{"label":1}]}]}),
    )
    .unwrap_err();
    assert!(error.starts_with("$.children[0].children[0]"), "{error}");
}

#[test]
fn json_pointer_escapes_and_percent_encoding_are_decoded() {
    let schema = json!({
        "$defs": {"a/b": {"type":"integer"}, "c~d": {"type":"string"}},
        "properties": {"x": {"$ref":"#/$defs/a~1b"}, "y": {"$ref":"#/%24defs/c~0d"}}
    });
    assert!(check_schema(&schema).is_ok());
    assert!(validate(&schema, &json!({"x":1,"y":"s"})).is_ok());
    assert!(validate(&schema, &json!({"x":"s"})).is_err());
    assert!(validate(&schema, &json!({"y":1})).is_err());
}

#[test]
fn cyclic_refs_are_rejected_or_bounded_instead_of_overflowing() {
    assert!(check_schema(&json!({"$ref": "#"})).is_err());
    let loop_via_defs = json!({
        "$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}},
        "$ref": "#/$defs/a"
    });
    assert!(check_schema(&loop_via_defs).is_err());
    // A cycle through a combinator is not a pure ref chain; it passes the check and must stop
    // at evaluation time.
    let through_all_of = json!({
        "$defs": {"a": {"allOf": [{"$ref": "#/$defs/a"}]}},
        "$ref": "#/$defs/a"
    });
    assert!(check_schema(&through_all_of).is_ok());
    let error = validate(&through_all_of, &json!(1)).unwrap_err();
    assert!(error.contains("$ref chain deeper"), "{error}");
    // Branching recursion must terminate within the evaluation budget.
    let branching = json!({
        "$defs": {"a": {"allOf": [{"$ref": "#/$defs/a"}, {"$ref": "#/$defs/a"}]}},
        "$ref": "#/$defs/a"
    });
    assert!(validate(&branching, &json!(1)).is_err());
}

#[test]
fn one_of_requires_exactly_one_branch() {
    let schema = json!({"oneOf": [{"type":"integer"}, {"minimum": 0}]});
    assert!(validate(&schema, &json!(-1)).is_ok());
    let both = validate(&schema, &json!(2)).unwrap_err();
    assert!(both.contains("matches 2 of the oneOf branches"), "{both}");
    let none = json!({"oneOf": [{"type":"string"}, {"type":"boolean"}]});
    let error = validate(&none, &json!(1)).unwrap_err();
    assert!(
        error.contains("matches none of the oneOf branches"),
        "{error}"
    );
    assert!(error.contains("expected string"), "{error}");
}

#[test]
fn any_all_not_and_conditionals_combine_branches() {
    let any = json!({"anyOf": [{"type":"string"}, {"type":"integer","minimum":5}]});
    assert!(validate(&any, &json!("x")).is_ok() && validate(&any, &json!(7)).is_ok());
    let error = validate(&any, &json!(2)).unwrap_err();
    assert!(
        error.contains("matches none of the anyOf branches") && error.contains("below the minimum"),
        "{error}"
    );
    let all = json!({"allOf": [{"type":"object","required":["a"]}, {"properties":{"a":{"type":"integer"}}}]});
    assert!(validate(&all, &json!({"a":1})).is_ok());
    assert!(validate(&all, &json!({"a":"x"})).is_err());
    assert!(validate(&all, &json!([])).is_err());
    let not = json!({"not": {"type":"null"}});
    assert!(validate(&not, &json!(null)).is_err() && validate(&not, &json!(0)).is_ok());
    let conditional = json!({
        "if": {"properties": {"kind": {"const":"file"}}, "required": ["kind"]},
        "then": {"required": ["path"]},
        "else": {"required": ["id"]}
    });
    assert!(validate(&conditional, &json!({"kind":"file","path":"a"})).is_ok());
    assert!(validate(&conditional, &json!({"kind":"file"})).is_err());
    assert!(validate(&conditional, &json!({"kind":"x","id":1})).is_ok());
    assert!(validate(&conditional, &json!({"kind":"x"})).is_err());
}

#[test]
fn strings_check_pattern_length_and_characters() {
    let code = json!({"type":"string","pattern":"^[a-z]+$","minLength":2,"maxLength":4});
    assert!(validate(&code, &json!("abc")).is_ok());
    assert!(validate(&code, &json!("A")).is_err());
    assert!(validate(&code, &json!("abcde")).is_err());
    assert!(validate(&json!({"type":"string","maxLength":2}), &json!("é€")).is_ok());
    assert!(check_schema(&json!({"pattern":"(unclosed"})).is_err());
    assert!(check_schema(&json!({"properties":{"a":{"pattern":"["}}})).is_err());
}

#[test]
fn known_formats_are_enforced_and_unknown_ones_are_annotations() {
    let date_time = json!({"format":"date-time"});
    assert!(validate(&date_time, &json!("2024-02-29T12:30:00.5+01:00")).is_ok());
    assert!(validate(&date_time, &json!("2023-02-29T12:30:00Z")).is_err());
    assert!(validate(&date_time, &json!("2024-02-29 12:30:00Z")).is_err());
    assert!(validate(&date_time, &json!("2024-02-29T12:30:00")).is_err());
    assert!(validate(&json!({"format":"date"}), &json!("2024-13-01")).is_err());
    assert!(validate(&json!({"format":"time"}), &json!("23:59:60Z")).is_ok());
    assert!(validate(&json!({"format":"email"}), &json!("a.b@example.com")).is_ok());
    assert!(validate(&json!({"format":"email"}), &json!("a@@example.com")).is_err());
    assert!(validate(&json!({"format":"email"}), &json!("no-at-sign")).is_err());
    assert!(
        validate(
            &json!({"format":"uuid"}),
            &json!("123e4567-e89b-12d3-a456-426614174000")
        )
        .is_ok()
    );
    assert!(
        validate(
            &json!({"format":"uuid"}),
            &json!("123e4567e89b12d3a456426614174000")
        )
        .is_err()
    );
    assert!(validate(&json!({"format":"ipv4"}), &json!("192.168.0.1")).is_ok());
    assert!(validate(&json!({"format":"ipv4"}), &json!("256.1.1.1")).is_err());
    assert!(validate(&json!({"format":"ipv6"}), &json!("::1")).is_ok());
    assert!(validate(&json!({"format":"uri"}), &json!("https://example.com/a")).is_ok());
    assert!(validate(&json!({"format":"uri"}), &json!("example.com")).is_err());
    assert!(validate(&json!({"format":"hostname"}), &json!("-bad.example")).is_err());
    assert!(validate(&json!({"format":"hostname"}), &json!("good.example")).is_ok());
    assert!(validate(&json!({"format":"uri-template"}), &json!("anything")).is_ok());
    assert!(check_schema(&json!({"format":"uri-template"})).is_ok());
}

#[test]
fn numeric_bounds_and_multiples() {
    let schema =
        json!({"type":"number","exclusiveMinimum":0,"exclusiveMaximum":10,"multipleOf":0.5});
    assert!(validate(&schema, &json!(0.5)).is_ok());
    assert!(validate(&schema, &json!(0)).is_err());
    assert!(validate(&schema, &json!(10)).is_err());
    assert!(validate(&schema, &json!(1.25)).is_err());
    assert!(check_schema(&json!({"multipleOf":0})).is_err());
    assert!(check_schema(&json!({"exclusiveMinimum":true})).is_err());
}

#[test]
fn pattern_properties_and_additional_properties_false() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type":"string"}},
        "patternProperties": {"^x-": {"type":"integer"}},
        "additionalProperties": false
    });
    assert!(validate(&schema, &json!({"a":"s","x-n":1})).is_ok());
    assert!(validate(&schema, &json!({"a":"s","x-n":"no"})).is_err());
    let error = validate(&schema, &json!({"a":"s","other":1})).unwrap_err();
    assert!(error.contains("unexpected property \"other\""), "{error}");
}

#[test]
fn object_counts_dependencies_and_property_names() {
    let counted = json!({"minProperties":1,"maxProperties":2,"propertyNames":{"maxLength":4}});
    assert!(validate(&counted, &json!({})).is_err());
    assert!(validate(&counted, &json!({"a":1,"b":2,"c":3})).is_err());
    assert!(validate(&counted, &json!({"toolong":1})).is_err());
    assert!(validate(&counted, &json!({"ok":1})).is_ok());
    let dependent = json!({"dependentRequired":{"a":["b"]}});
    assert!(validate(&dependent, &json!({"a":1})).is_err());
    assert!(validate(&dependent, &json!({"a":1,"b":2})).is_ok());
}

#[test]
fn arrays_support_prefix_items_tuples_uniqueness_and_contains() {
    let pair = json!({"prefixItems":[{"type":"string"},{"type":"integer"}],"items":false});
    assert!(validate(&pair, &json!(["a", 1])).is_ok());
    assert!(validate(&pair, &json!([1, "a"])).is_err());
    assert!(validate(&pair, &json!(["a", 1, true])).is_err());
    let tuple = json!({"items":[{"type":"string"}],"additionalItems":{"type":"integer"}});
    assert!(validate(&tuple, &json!(["a", 2, 3])).is_ok());
    assert!(validate(&tuple, &json!(["a", "b"])).is_err());
    assert!(validate(&json!({"uniqueItems":true}), &json!([1, 1.0])).is_err());
    assert!(validate(&json!({"uniqueItems":true}), &json!([1, "1"])).is_ok());
    let counted =
        json!({"contains":{"type":"integer","minimum":10},"minContains":2,"maxContains":3});
    assert!(validate(&counted, &json!([1, 10, 11])).is_ok());
    assert!(validate(&counted, &json!([1, 10])).is_err());
    assert!(validate(&counted, &json!([10, 11, 12, 13])).is_err());
}

#[test]
fn annotations_and_boolean_subschemas_are_accepted() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "urn:example",
        "$comment": "note",
        "title": "t",
        "description": "d",
        "default": {},
        "examples": [{}],
        "readOnly": false,
        "writeOnly": false,
        "deprecated": false,
        "definitions": {},
        "type": "object",
        "properties": {"any": true, "none": false}
    });
    assert!(check_schema(&schema).is_ok());
    assert!(validate(&schema, &json!({"any":1})).is_ok());
    assert!(validate(&schema, &json!({"none":1})).is_err());
}

#[test]
fn check_rejects_remote_and_unsupported_references_and_keywords() {
    assert!(check_schema(&json!({"$ref":"https://example.com/s.json"})).is_err());
    assert!(check_schema(&json!({"$ref":"#missing"})).is_err());
    assert!(check_schema(&json!({"unevaluatedProperties":false})).is_err());
    assert!(check_schema(&json!({"$dynamicRef":"#x"})).is_err());
    assert!(check_schema(&json!({"$defs":{"a":{"type":"text"}},"$ref":"#/$defs/a"})).is_err());
    assert!(check_schema(&json!({"type":[]})).is_err());
    assert!(check_schema(&json!({"anyOf":[{"type":"string"}],"not":5})).is_err());
}

#[test]
fn validate_all_reports_distinct_errors_up_to_max() {
    let schema =
        json!({"type":"object","required":["a","b","c"],"properties":{"a":{"type":"integer"}}});
    let errors = validate_all(&schema, &json!({"a":"x"}), 10);
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(validate_all(&schema, &json!({"a":"x"}), 2).len(), 2);
    assert!(validate_all(&schema, &json!({"a":1,"b":1,"c":1}), 5).is_empty());
}
