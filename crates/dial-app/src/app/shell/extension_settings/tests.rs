use super::*;
#[test]
fn schema_fields_decode_typed_values_without_exposing_secrets() {
    let schema = json!({"type":"object","properties":{
        "enabled":{"type":"boolean","default":true},
        "limit":{"type":"integer","minimum":1},
        "mode":{"type":"string","enum":["fast","safe"]},
        "name":{"type":"string"},
        "token":{"type":"string","format":"password","default":"never-display-this"}
    }});
    let form = Form::new(
        "demo",
        &schema,
        json!({"enabled":true,"limit":3,"mode":"fast","name":"old"}),
    )
    .unwrap();
    assert!(!format!("{:?}", form.panel).contains("never-display-this"));
    let mut values = form.panel.initial_values();
    for field in &form.fields {
        values.insert(
            field.id.clone(),
            match field.name.as_str() {
                "enabled" => InputValue::Choice(0),
                "mode" => InputValue::Choice(1),
                "limit" => InputValue::Text("42".into()),
                "name" => InputValue::Text("new".into()),
                _ => panic!("unexpected editable secret"),
            },
        );
    }
    let decoded = form.decode(&values).unwrap();
    assert_eq!(
        decoded,
        json!({"enabled":false,"limit":42,"mode":"safe","name":"new"})
    );
    crate::tools::integrations::plugin::contributions::validate_values(&schema, &decoded).unwrap();
    let limit = form.fields.iter().find(|f| f.name == "limit").unwrap();
    values.insert(limit.id.clone(), InputValue::Text("not a number".into()));
    assert!(form.decode(&values).is_err());
}
#[test]
fn optional_json_fields_can_be_removed_without_coercing_to_strings() {
    let schema =
        json!({"type":"object","properties":{"data":{"type":"object"},"count":{"type":"integer"}}});
    let form = Form::new("demo", &schema, json!({"data":{"x":1},"count":3})).unwrap();
    let mut values = form.panel.initial_values();
    for field in &form.fields {
        values.insert(
            field.id.clone(),
            InputValue::Text(if field.name == "count" {
                String::new()
            } else {
                "{\"x\":2}".into()
            }),
        );
    }
    assert_eq!(form.decode(&values).unwrap(), json!({"data":{"x":2}}));
}

#[test]
fn incomplete_layer_can_open_without_importing_other_scope_or_secret_defaults() {
    let schema = json!({"type":"object","required":["name"],"properties":{
        "name":{"type":"string","minLength":1},
        "count":{"type":"integer","default":4},
        "token":{"type":"string","writeOnly":true,"default":"hidden"}
    }});
    let form = Form::new("demo", &schema, json!({})).unwrap();
    assert!(!format!("{:?}", form.panel).contains("hidden"));
    let mut inputs = form.panel.initial_values();
    let name = form
        .fields
        .iter()
        .find(|field| field.name == "name")
        .unwrap();
    inputs.insert(name.id.clone(), InputValue::Text("chosen".into()));
    assert_eq!(
        form.decode(&inputs).unwrap(),
        json!({"name":"chosen","count":4})
    );
}

#[test]
fn nested_secret_defaults_and_legacy_values_are_rejected_before_form_rendering() {
    let schema = json!({"type":"object","properties":{"nested":{"type":"object","properties":{
        "name":{"type":"string"},"token":{"type":"string","writeOnly":true}
    }}}});
    let form = Form::new("demo", &schema, json!({"nested":{"name":"public"}})).unwrap();
    assert!(format!("{:?}", form.panel).contains("public"));
    assert!(Form::new("demo", &schema, json!({"nested":{"token":"private"}})).is_err());
    for key in ["default", "const", "enum"] {
        let mut schema = schema.clone();
        schema["properties"]["nested"][key] = if key == "enum" {
            json!([{"token":"private"}])
        } else {
            json!({"token":"private"})
        };
        assert!(Form::new("demo", &schema, json!({})).is_err());
    }
}
