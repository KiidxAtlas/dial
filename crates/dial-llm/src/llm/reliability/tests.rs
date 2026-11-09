use crate::llm::profiles::{ModelProfile, StructuredOutput};
use crate::runtime::schema::validate;
use serde_json::{Value, json};

const DOCKER: &str = "docker-model-runner";

fn tool(parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {"name": "write_notes", "description": "Record notes.", "parameters": parameters}
    })
}

/// Canonical schema: oversized bounds that Docker Model Runner's grammar compiler rejects, plus
/// bounds at or below the 1024 threshold and local-only `minLength`/`minItems` that must survive.
fn notes_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "maxItems": 5000,
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "minLength": 2, "maxLength": 8192},
                        "context": {"type": "string", "maxLength": 1025},
                        "tags": {
                            "type": "array",
                            "maxItems": 1024,
                            "items": {"type": "string", "maxLength": 1024}
                        }
                    },
                    "required": ["content"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["items"]
    })
}

/// Expected wire form of `notes_schema`: only the bounds above 1024 are gone.
fn notes_wire_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "minLength": 2},
                        "context": {"type": "string"},
                        "tags": {
                            "type": "array",
                            "maxItems": 1024,
                            "items": {"type": "string", "maxLength": 1024}
                        }
                    },
                    "required": ["content"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["items"]
    })
}

fn docker_profile() -> ModelProfile {
    ModelProfile::minimal(DOCKER, "test-model")
}

fn wire_body(profile: &ModelProfile, parameters: Value) -> Value {
    let mut body = json!({"model": "m", "messages": [], "tools": [tool(parameters)]});
    profile.apply_openai(&mut body);
    body
}

#[test]
fn docker_tool_parameters_drop_only_bounds_above_the_wire_threshold() {
    let body = wire_body(&docker_profile(), notes_schema());

    assert_eq!(
        body.pointer("/tools/0/function/parameters"),
        Some(&notes_wire_schema())
    );
}

#[test]
fn docker_tool_repair_schema_carries_the_same_wire_only_parameters() {
    let mut profile = docker_profile();
    profile.tool_repair = true;
    profile.endpoint_structured_output = Some(StructuredOutput::JsonSchema);

    let body = wire_body(&profile, notes_schema());

    assert!(body.get("tools").is_none());
    assert_eq!(
        body.pointer("/response_format/json_schema/schema/oneOf/0/properties/arguments"),
        Some(&notes_wire_schema())
    );
}

#[test]
fn every_schema_position_that_holds_a_subschema_is_stripped() {
    // (schema, pointer of an oversized keyword that must not survive on the wire)
    let cases = [
        (
            json!({"$defs": {"big": {"type": "string", "maxLength": 4096}}, "type": "object"}),
            "/$defs/big/maxLength",
        ),
        (
            json!({"definitions": {"big": {"type": "array", "maxItems": 4096}}, "type": "object"}),
            "/definitions/big/maxItems",
        ),
        (
            json!({"type": "object", "properties": {"a": {"allOf": [{"type": "string", "maxLength": 4096}]}}}),
            "/properties/a/allOf/0/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"anyOf": [{"type": "string", "maxLength": 4096}]}}}),
            "/properties/a/anyOf/0/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"oneOf": [{"type": "string", "maxLength": 4096}]}}}),
            "/properties/a/oneOf/0/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"prefixItems": [{"type": "string", "maxLength": 4096}]}}}),
            "/properties/a/prefixItems/0/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"items": [{"type": "string", "maxLength": 4096}]}}}),
            "/properties/a/items/0/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"items": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/items/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "object", "patternProperties": {"^x": {"type": "string", "maxLength": 4096}}}}}),
            "/properties/a/patternProperties/^x/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "object", "additionalProperties": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/additionalProperties/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "object", "propertyNames": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/propertyNames/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "array", "contains": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/contains/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"not": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/not/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"if": {"type": "string", "maxLength": 4096}, "then": {"maxLength": 4096}, "else": {"maxLength": 4096}}}}),
            "/properties/a/else/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "array", "additionalItems": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/additionalItems/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "array", "unevaluatedItems": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/unevaluatedItems/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "object", "unevaluatedProperties": {"type": "string", "maxLength": 4096}}}}),
            "/properties/a/unevaluatedProperties/maxLength",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "object", "dependentSchemas": {"k": {"type": "string", "maxLength": 4096}}}}}),
            "/properties/a/dependentSchemas/k/maxLength",
        ),
        (
            json!({"dependencies": {"name": {"type": "string", "maxLength": 4096}, "other": ["name"]}}),
            "/dependencies/name/maxLength",
        ),
        (
            json!({"type": "string", "contentSchema": {"type": "string", "maxLength": 4096}}),
            "/contentSchema/maxLength",
        ),
    ];

    for (schema, pointer) in cases {
        let body = wire_body(&docker_profile(), schema);
        let path = format!("/tools/0/function/parameters{pointer}");
        assert!(
            body.pointer(&path).is_none(),
            "oversized keyword survived at {pointer}"
        );
    }
}

#[test]
fn non_docker_providers_send_tool_parameters_unmodified() {
    let profile = ModelProfile::minimal("lm-studio", "test-model");
    let body = wire_body(&profile, notes_schema());

    assert_eq!(
        body.pointer("/tools/0/function/parameters"),
        Some(&notes_schema())
    );
}

#[test]
fn non_docker_tool_repair_schema_keeps_canonical_bounds() {
    let mut profile = ModelProfile::minimal("lm-studio", "test-model");
    profile.tool_repair = true;
    profile.endpoint_structured_output = Some(StructuredOutput::JsonSchema);

    let body = wire_body(&profile, notes_schema());

    assert_eq!(
        body.pointer("/response_format/json_schema/schema/oneOf/0/properties/arguments"),
        Some(&notes_schema())
    );
}

#[test]
fn canonical_validator_still_rejects_what_the_wire_schema_admits() {
    let canonical = notes_schema();
    let wire = notes_wire_schema();
    let oversized = json!({"items": [{"content": "x".repeat(9000)}]});
    let too_short = json!({"items": [{"content": ""}]});

    assert!(validate(&canonical, &oversized).is_err());
    assert!(validate(&wire, &oversized).is_ok());
    assert!(validate(&canonical, &too_short).is_err());
    assert!(validate(&wire, &too_short).is_err());
    assert!(validate(&canonical, &json!({"items": [{"content": "ok"}]})).is_ok());
}

#[test]
fn length_and_count_bounds_above_threshold_are_dropped_on_docker_wire() {
    let cases = [
        ("minLength", json!({"type": "string", "minLength": 1025})),
        ("maxLength", json!({"type": "string", "maxLength": 1025})),
        ("minItems", json!({"type": "array", "minItems": 1025})),
        ("maxItems", json!({"type": "array", "maxItems": 1025})),
    ];
    for (key, schema) in cases {
        let parameters = json!({"type": "object", "properties": {"v": schema}});
        let body = wire_body(&docker_profile(), parameters);
        let child = body
            .pointer("/tools/0/function/parameters/properties/v")
            .expect("property kept");
        assert!(
            child.get(key).is_none(),
            "{key} above 1024 survived: {child}"
        );
    }
}

#[test]
fn keyword_named_properties_and_payload_data_keep_their_keys() {
    let parameters = json!({
        "type": "object",
        "properties": {
            "maxLength": {"type": "integer", "maxLength": 4096, "default": {"maxLength": 4096}, "examples": [{"minItems": 2000}]},
            "minItems": {"type": "string", "enum": ["a", {"maxItems": 4096}], "const": {"maxLength": 4096}}
        },
        "required": ["maxLength"]
    });
    let expected = json!({
        "type": "object",
        "properties": {
            "maxLength": {"type": "integer", "default": {"maxLength": 4096}, "examples": [{"minItems": 2000}]},
            "minItems": {"type": "string", "enum": ["a", {"maxItems": 4096}], "const": {"maxLength": 4096}}
        },
        "required": ["maxLength"]
    });
    let canonical = tool(parameters.clone());
    let mut body = json!({"model": "m", "messages": [], "tools": [canonical.clone()]});
    docker_profile().apply_openai(&mut body);

    assert_eq!(
        body.pointer("/tools/0/function/parameters"),
        Some(&expected)
    );
    assert_eq!(canonical, tool(parameters));
}

#[test]
fn string_encoded_object_and_array_fields_are_decoded_only_where_the_schema_wants_them() {
    let schema = json!({
        "type": "object",
        "properties": {
            "repro": {"type": "object", "properties": {"steps": {"type": "array"}}},
            "targets": {"type": "array"},
            "command": {"type": "string"},
            "free": {}
        }
    });
    let mut args = json!({
        "repro": "{\"name\": \"x\", \"steps\": \"[1, 2]\"}",
        "targets": " [\"a\"] ",
        "command": "{\"not\": \"decoded\"}",
        "free": "[1]"
    });
    super::decode_encoded_containers(&mut args, &schema);
    assert_eq!(
        args,
        json!({
            "repro": {"name": "x", "steps": [1, 2]},
            "targets": ["a"],
            "command": "{\"not\": \"decoded\"}",
            "free": "[1]"
        })
    );
    let mut mismatched = json!({"repro": "[1]", "targets": "not json"});
    super::decode_encoded_containers(&mut mismatched, &schema);
    assert_eq!(mismatched, json!({"repro": "[1]", "targets": "not json"}));
}
