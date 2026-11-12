use super::*;
use crate::storage::session::SessionMeta;

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    (project, home, layout)
}

/// A closed journal holding one user request, so the listing counts it as a conversation.
fn conversation(layout: &Layout, text: &str) -> String {
    let mut session = Session::create(
        layout,
        SessionMeta {
            model: "test-model".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role":"user","content":text}))
        .unwrap();
    session.id().to_owned()
}

fn ids(page: &Value) -> Vec<String> {
    page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn list_pages_pins_first_and_marks_the_current_session() {
    let (_project, _home, layout) = layout();
    let first = conversation(&layout, "first");
    let second = conversation(&layout, "second");
    let empty = Session::create(
        &layout,
        SessionMeta {
            model: "test-model".into(),
        },
    )
    .unwrap()
    .id()
    .to_owned();

    let all = list(&layout, Some(&first), &json!({})).unwrap();
    assert_eq!(all["total"], 2, "{all}");
    assert!(!ids(&all).contains(&empty));
    let with_empty = list(&layout, None, &json!({"include_empty":true})).unwrap();
    assert_eq!(with_empty["total"], 3);

    pin(&layout, &json!({"session_id":second,"pinned":true})).unwrap();
    let pinned = list(&layout, Some(&first), &json!({})).unwrap();
    assert_eq!(ids(&pinned)[0], second);
    assert_eq!(pinned["sessions"][0]["pinned"], true);
    let current = pinned["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_id"] == first.as_str())
        .unwrap();
    assert_eq!(current["current"], true);
    assert_eq!(current["first_message"], "first");

    let page = list(&layout, None, &json!({"offset":1,"limit":1})).unwrap();
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(page["offset"], 1);
    assert_eq!(page["next_offset"], Value::Null);
    assert_eq!(
        list(&layout, None, &json!({"limit":1})).unwrap()["next_offset"],
        1
    );
    assert!(list(&layout, None, &json!({"offset":3})).is_err());
    assert!(list(&layout, None, &json!({"limit":"many"})).is_err());
}

#[test]
fn pin_is_idempotent_and_refuses_unknown_sessions() {
    let (_project, _home, layout) = layout();
    let id = conversation(&layout, "hello");
    let first = pin(&layout, &json!({"session_id":id,"pinned":true})).unwrap();
    assert_eq!(first["changed"], true);
    let again = pin(&layout, &json!({"session_id":id,"pinned":true})).unwrap();
    assert_eq!(again["changed"], false);
    let off = pin(&layout, &json!({"session_id":id,"pinned":false})).unwrap();
    assert_eq!(off["changed"], true);
    assert!(pin(&layout, &json!({"session_id":"missing","pinned":true})).is_err());
    assert!(pin(&layout, &json!({"session_id":id})).is_err());
    // A prefix is not an id.
    assert!(pin(&layout, &json!({"session_id":&id[..4],"pinned":true})).is_err());
}

#[test]
fn delete_removes_another_session_and_its_pin_but_refuses_the_active_one() {
    let (_project, _home, layout) = layout();
    let active = conversation(&layout, "active");
    let other = conversation(&layout, "other");
    pin(&layout, &json!({"session_id":other,"pinned":true})).unwrap();

    let refused = delete(&layout, Some(&active), &json!({"session_id":active})).unwrap_err();
    assert!(refused.to_string().contains("active session"), "{refused}");
    assert!(delete(&layout, Some(&active), &json!({"session_id":"missing"})).is_err());

    let deleted = delete(&layout, Some(&active), &json!({"session_id":other})).unwrap();
    assert_eq!(deleted["deleted"], true);
    assert_eq!(
        ids(&list(&layout, Some(&active), &json!({})).unwrap()),
        [active]
    );
    assert!(
        !session_pins::load(&layout.user_dir)
            .unwrap()
            .contains(&other)
    );
}

fn code(error: anyhow::Error) -> &'static str {
    error
        .downcast_ref::<super::super::RequestError>()
        .map_or("failed", |error| error.code.as_str())
}

#[test]
fn administration_refusals_carry_codes_a_client_can_branch_on() {
    let (_project, _home, layout) = layout();
    let current = conversation(&layout, "current");
    let other = conversation(&layout, "other");
    assert_eq!(
        code(pin(&layout, &json!({"session_id":"missing","pinned":true})).unwrap_err()),
        "not_found"
    );
    assert_eq!(
        code(pin(&layout, &json!({"session_id":other})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(delete(&layout, Some(&current), &json!({"session_id":current})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(list(&layout, None, &json!({"offset":99})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(list(&layout, None, &json!("x")).unwrap_err()),
        "invalid_params"
    );
}
