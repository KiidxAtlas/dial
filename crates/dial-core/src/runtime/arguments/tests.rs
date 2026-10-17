use super::*;
use serde_json::json;

fn edit() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},
        "new_string":{"type":"string"},"limit":{"type":"integer","minimum":1},"replace_all":{"type":"boolean"}},
        "required":["path","old_string","new_string"],"additionalProperties":false})
}

#[test]
fn accepts_valid_and_rejects_each_form_defect() {
    let p = edit();
    assert!(
        validate(
            &p,
            &json!({"path":"a","old_string":"b","new_string":"c","limit":3,"replace_all":true})
        )
        .is_ok()
    );
    assert!(
        validate(&p, &json!({"path":"a","new_string":"c"}))
            .unwrap_err()
            .contains("missing")
    );
    assert!(
        validate(&p, &json!({"path":"a","old_string":2,"new_string":"c"}))
            .unwrap_err()
            .contains("string")
    );
    assert!(
        validate(
            &p,
            &json!({"path":"a","old_string":"b","new_string":"c","extra":1})
        )
        .unwrap_err()
        .contains("unexpected property")
    );
    assert!(
        validate(
            &p,
            &json!({"path":"a","old_string":"b","new_string":"c","limit":0})
        )
        .unwrap_err()
        .contains(">=")
    );
    assert!(
        validate(
            &p,
            &json!({"path":"a","old_string":"b","new_string":"c","limit":1.5})
        )
        .unwrap_err()
        .contains("integer")
    );
    assert!(validate(&p, &json!("path")).is_err());
}

#[test]
fn invalid_calls_include_schema_guidance_without_submitted_values() {
    let args = json!({"path":"private-user-value", "old_string":3, "new_string":"c"});
    let before = args.clone();
    let error = validate(&edit(), &args).unwrap_err();
    assert!(!error.contains("private-user-value"));
    assert_eq!(args, before);
    let error = validate(&edit(), &json!("private-user-value")).unwrap_err();
    assert!(error.contains("JSON object") && error.contains("Argument guide"));
    assert!(!error.contains("private-user-value"));
}

#[test]
fn argument_guide_is_bounded_for_large_schemas() {
    let mut properties = serde_json::Map::new();
    for n in 0..100 {
        properties.insert(
            format!("{n:03}{}", "x".repeat(1000)),
            json!({"type":"string"}),
        );
    }
    let guide = argument_guide(&json!({"properties":properties}));
    assert!(guide.len() < 2000);
    assert_eq!(guide.matches("(optional)").count(), 12);
    assert!(guide.contains("more fields"));
    assert!(argument_guide(&json!({})).is_empty());
}
