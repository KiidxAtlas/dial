use super::*;
use crate::ui::overlay::extensions::model::{Field, Tab};

#[test]
fn intents_parse_by_op_and_risky_ones_need_confirmation() {
    let enable = intent(&json!({"intent":{"op":"toggle_mcp","id":"a","enable":true}})).unwrap();
    assert_eq!(
        enable,
        Intent::ToggleMcp {
            id: "a".into(),
            enable: true
        }
    );
    let remove = json!({"intent":{"op":"remove_mcp","id":"a"}});
    let refused = intent(&remove).unwrap_err();
    assert_eq!(
        refused
            .downcast_ref::<super::super::RequestError>()
            .map(|error| error.code),
        Some(ErrorCode::InvalidParams)
    );
    assert!(refused.to_string().contains("confirm: true"), "{refused}");
    let mut confirmed = remove.clone();
    confirmed["confirm"] = json!(true);
    assert_eq!(
        intent(&confirmed).unwrap(),
        Intent::RemoveMcp { id: "a".into() }
    );
    for bad in [
        json!({}),
        json!({"intent":{"op":"nope"}}),
        json!({"intent":"x"}),
    ] {
        assert!(intent(&bad).is_err(), "{bad}");
    }
}

#[test]
fn forms_name_the_intent_to_submit_and_never_carry_values() {
    let mut key = Form::key("srv", Some("bad key".into()));
    key.fields[0].value = "typed-secret".into();
    let shown = form(&key);
    assert_eq!(shown["submit"], "smithery_key");
    assert_eq!(shown["target"], "srv");
    assert_eq!(shown["error"], "bad key");
    assert_eq!(shown["fields"][0]["secret"], true);
    assert!(!shown.to_string().contains("typed-secret"));

    let inputs = Form::mcp_inputs(
        "github",
        "Set up",
        "note",
        vec![Field {
            key: "TOKEN".into(),
            label: "TOKEN".into(),
            hint: "a token".into(),
            required: true,
            secret: true,
            value: String::new(),
        }],
        None,
    );
    let shown = form(&inputs);
    assert_eq!(shown["submit"], "install_mcp");
    assert_eq!(shown["target"], "github");
    assert_eq!(shown["fields"][0]["required"], true);
}

#[test]
fn catalog_pages_report_rows_and_paging() {
    let update = CatalogUpdate {
        tab: Tab::Mcp,
        query: "git".into(),
        page: 2,
        rows: Vec::new(),
        more: true,
        message: "10 results".into(),
        error: None,
    };
    let shown = catalog(&update);
    assert_eq!(shown["tab"], "Mcp");
    assert_eq!(shown["page"], 2);
    assert_eq!(shown["more"], true);
    assert!(shown["rows"].as_array().unwrap().is_empty());
}
