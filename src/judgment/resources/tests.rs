use super::*;
#[test]
fn prompt_inputs_are_validated_before_render_and_schema_changes_fail_closed() {
    let spec = QuestionSpec::parse(r#"{"schema_version":1,"instructions":"Review {{target}}","criteria":{},"required":["target"]}"#).unwrap();
    assert!(spec.render(&serde_json::json!({})).is_err());
    assert_eq!(
        spec.render(&serde_json::json!({"target":"src/a.rs"}))
            .unwrap(),
        "Review src/a.rs"
    );
    assert!(
        QuestionSpec::parse(
            r#"{"schema_version":2,"instructions":"x","criteria":{},"required":[]}"#
        )
        .is_err()
    );
}
