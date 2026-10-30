use super::*;

fn view(elements: Value) -> Value {
    json!({"id":"main", "title":"Main", "location":"sidebar", "elements":elements})
}
#[test]
fn legacy_empty_and_complete_contributions_parse() {
    for value in [Value::Null, json!({})] {
        let parsed = Contributions::parse(value).unwrap();
        assert!(parsed.views.is_empty());
        assert_eq!(parsed.settings, json!({}));
    }
    let value = json!({"views":[view(json!([
        {"type":"text","text":"Hello"}, {"type":"markdown","text":"**safe**"},
        {"type":"code","text":"x = 1","language":"python"}, {"type":"diff","before":"a","after":"b","path":"a.py"},
        {"type":"list","items":[{"id":"item","label":"Item"}]},
        {"type":"table","columns":[{"id":"col","label":"Column"}],"rows":[{"id":"row","cells":["value"]}]},
        {"type":"tree","nodes":[{"id":"node","label":"Node","children":[]}]},
        {"type":"tabs","tabs":[{"id":"tab","title":"Tab","elements":[]}]},
        {"type":"progress","value":1,"total":2,"label":"Working"},
        {"type":"button","id":"go","label":"Go"}, {"type":"input","id":"query","label":"Query","value":""},
        {"type":"select","id":"pick","label":"Pick","options":[{"value":"a","label":"A"}],"value":"a"}
    ]))], "settings":{"type":"object"}, "commands":[{"name":"go","description":"Go","parameters":{}}],
    "agents":[{"id":"reviewer","title":"Reviewer","tools":[],"guidance":"Review"}],
    "workflows":[{"id":"review","title":"Review","steps":[{"id":"approve","title":"Approve","kind":"approval","prompt":"Continue?"}]}],
    "context_sources":[{"id":"context","title":"Context","description":"Project"}],
    "renderers":[{"id":"result","title":"Result","fields":[{"label":"Status","path":"status"}]}],
    "keybindings":[{"key":"ctrl+r","command":"go"}], "presets":[{"id":"default","title":"Default","values":{}}]});
    let parsed = Contributions::parse(value).unwrap();
    assert_eq!(parsed.views[0].elements.len(), 12);
    assert!(Contributions::parse(serde_json::to_value(parsed).unwrap()).is_ok());
}
#[test]
fn refuses_ambiguous_unbounded_or_terminal_controlled_views() {
    let invalid = [
        view(
            json!([{"type":"button","id":"same","label":"A"},{"type":"input","id":"same","label":"B","value":""}]),
        ),
        view(json!([{"type":"progress","value":3,"total":2,"label":"Bad"}])),
        view(json!([{"type":"text","text":"\u{1b}[2J"}])),
        view(
            json!([{"type":"table","columns":[{"id":"c","label":"C"}],"rows":[{"id":"r","cells":[]}]}]),
        ),
        view(json!([{"type":"input","id":"../escape","label":"x","value":""}])),
        view(json!([{"type":"text","text":"x".repeat(65536)}])),
        view(json!(
            (0..257)
                .map(|_| json!({"type":"text","text":"x"}))
                .collect::<Vec<_>>()
        )),
    ];
    for v in invalid {
        assert!(Contributions::parse(json!({"views":[v]})).is_err());
    }
    let mut node = json!({"id":"n0","label":"N"});
    for i in 1..9 {
        node = json!({"id":format!("n{i}"),"label":"N","children":[node]});
    }
    assert!(
        Contributions::parse(json!({"views":[view(json!([{"type":"tree","nodes":[node]}]))]}))
            .is_err()
    );
}
#[test]
fn refuses_invalid_registrations_and_schema_boundaries() {
    for value in [
        json!({"unknown":true}),
        json!({"settings":[]}),
        json!({"commands":[{"name":"go","description":"Go"},{"name":"go","description":"Go"}]}),
        json!({"settings":{"$ref":"https://untrusted.invalid/schema"}}),
        json!({"context_sources":(0..65).map(|i|json!({"id":format!("s{i}"),"title":"Source","description":""})).collect::<Vec<_>>()}),
    ] {
        assert!(Contributions::parse(value).is_err());
    }
}
