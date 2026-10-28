use super::*;

fn panel(elements: Vec<Element>) -> Panel {
    Panel {
        title: "Pick".into(),
        generation: 1,
        elements,
    }
}

fn button(id: &str) -> Element {
    Element::Button {
        id: id.into(),
        label: id.into(),
        primary: false,
    }
}

fn input(id: &str) -> Element {
    Element::Input {
        id: id.into(),
        label: id.into(),
        value: "start".into(),
    }
}

fn select(id: &str) -> Element {
    Element::Select {
        id: id.into(),
        label: id.into(),
        options: vec!["a".into(), "b".into()],
        selected: 1,
    }
}

#[test]
fn panels_without_a_title_or_a_button_are_rejected() {
    let mut p = panel(vec![Element::Text("no button".into())]);
    assert!(p.validate().is_err());
    p.elements.push(button("ok"));
    p.title = "  ".into();
    assert!(p.validate().is_err());
}

#[test]
fn ids_must_be_safe_and_unique_across_inputs_selects_and_buttons() {
    assert!(panel(vec![input("a"), button("a")]).validate().is_err());
    assert!(panel(vec![input("a b"), button("ok")]).validate().is_err());
    assert!(panel(vec![input(""), button("ok")]).validate().is_err());
    assert!(
        panel(vec![input("ok_1-x"), button("ok")])
            .validate()
            .is_ok()
    );
}

#[test]
fn selects_need_options_and_an_in_range_start() {
    let empty = Element::Select {
        id: "m".into(),
        label: "m".into(),
        options: Vec::new(),
        selected: 0,
    };
    assert!(panel(vec![empty, button("ok")]).validate().is_err());
    let off = Element::Select {
        id: "m".into(),
        label: "m".into(),
        options: vec!["a".into()],
        selected: 3,
    };
    assert!(panel(vec![off, button("ok")]).validate().is_err());
}

#[test]
fn oversized_layouts_are_rejected() {
    let mut elements: Vec<Element> = (0..MAX_ELEMENTS)
        .map(|n| Element::Text(n.to_string()))
        .collect();
    elements.push(button("ok"));
    assert!(panel(elements).validate().is_err());
}

#[test]
fn nested_rows_and_choices_cannot_bypass_the_layout_budget() {
    for nested in [
        serde_json::json!({"type":"list","items":vec![""; MAX_ELEMENTS]}),
        serde_json::json!({"type":"select","id":"mode","label":"Mode",
            "options":vec![""; MAX_ELEMENTS],"selected":0}),
    ] {
        let wire = serde_json::json!({
            "title":"Bounded panel","generation":1,
            "elements":[nested,{"type":"button","id":"ok","label":"OK"}]
        });
        assert!(
            panel_from_json(&wire).is_err(),
            "nested layout was admitted"
        );
    }
}

#[test]
fn submitted_values_must_match_their_elements() {
    let p = panel(vec![input("name"), select("mode"), button("ok")]);
    let mut values = p.initial_values();
    assert!(p.check_values(&values).is_ok());
    values.insert("mode".into(), Value::Choice(9));
    assert!(p.check_values(&values).is_err());
    values.insert("mode".into(), Value::Choice(0));
    values.insert("name".into(), Value::Choice(0));
    assert!(p.check_values(&values).is_err());
    values.remove("name");
    assert!(p.check_values(&values).is_err());
}

#[test]
fn late_updates_apply_only_to_the_shown_generation() {
    assert!(accepts_generation(4, 4));
    assert!(!accepts_generation(4, 3));
    assert!(!accepts_generation(4, 5));
}
