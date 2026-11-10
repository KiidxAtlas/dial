use super::*;
use crate::tools::integrations::ext::Registry;
use crate::tools::integrations::mcp::load;
use crate::tools::integrations::mcp::tests::{config_with, fake_config};
use crate::tools::surface::contracts::identities;

#[test]
fn exact_legacy_aliases_preserve_remote_and_sanitized_server_collisions() {
    let mut loaded = load(&config_with(vec![
        ("a b", fake_config("context", 30)),
        ("a.b", fake_config("context", 30)),
    ]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    for session in &loaded.sessions {
        let tools = ["context", "get_prompt", "read_resource"].map(|name| RemoteTool {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type":"object"}),
            read_only_hint: false,
            declared_read_only: false,
        });
        session.apply_catalog(tools.to_vec());
    }
    // Released order: all capability wrappers first, then each remote catalog.
    let mut legacy = Namer::default();
    let mut old_wrappers = Vec::new();
    for session in &loaded.sessions {
        for (remote, properties, selector) in [
            (
                "read_resource",
                json!({"uri":{"type":"string"}}),
                "resource",
            ),
            (
                "get_prompt",
                json!({"name":{"type":"string"},"arguments":{"type":"object"}}),
                "prompt",
            ),
        ] {
            let name = legacy.name(session.name(), remote);
            let schema = json!({"type":"function","function":{
                "name":name,"parameters":{"type":"object","properties":properties}
            }});
            let identity = identities(&[schema], &Registry::new())
                .remove(&name)
                .unwrap();
            old_wrappers.push((session.name().to_owned(), identity, selector));
        }
    }
    let mut expected_remotes = Vec::new();
    for session in &loaded.sessions {
        for remote in ["context", "get_prompt", "read_resource"] {
            expected_remotes.push((
                session.name().to_owned(),
                remote,
                legacy.name(session.name(), remote),
            ));
        }
    }
    let before: Vec<_> = old_wrappers.iter().map(|(_, old, _)| {
        loaded.live.historical_surface_projection(old, &json!({"uri":"fake:///x","name":"chosen","arguments":{"x":"y"},"surface":"ignored"})).unwrap()
    }).collect();
    let unconsumed = loaded.live.clone();
    let registered: BTreeMap<_, _> = loaded.live.sync().upserted.into_iter().collect();
    for (server, remote, expected_name) in expected_remotes {
        let mounted = loaded
            .live
            .servers
            .iter()
            .find(|mounted| mounted.session.name() == server)
            .unwrap();
        assert_eq!(
            mounted.names[remote], expected_name,
            "remote {server}/{remote} keeps its released name"
        );
        assert!(registered.contains_key(&expected_name));
    }
    for ((server, old, selector), (new_name, arguments)) in old_wrappers.iter().zip(before) {
        let mounted = loaded
            .live
            .servers
            .iter()
            .find(|mounted| mounted.session.name() == server)
            .unwrap();
        assert_eq!(new_name, mounted.surfaces[0].name());
        assert_ne!(new_name, mounted.names["context"]);
        assert_ne!(new_name, old.name);
        assert_eq!(arguments["surface"], *selector);
        assert_eq!(arguments["uri"], "fake:///x");
        assert_eq!(arguments["name"], "chosen");
        assert_eq!(arguments["arguments"], json!({"x":"y"}));
        assert_eq!(
            loaded.live.historical_surface_projection(old, &json!({})),
            Some((new_name.clone(), json!({"surface":selector})))
        );
        let mut corrupt = old.clone();
        corrupt.schema_sha256 = "wrong".into();
        assert!(
            loaded
                .live
                .historical_surface_projection(&corrupt, &json!({}))
                .is_none()
        );
        corrupt = old.clone();
        corrupt.revision += 1;
        assert!(
            loaded
                .live
                .historical_surface_projection(&corrupt, &json!({}))
                .is_none()
        );
        corrupt = old.clone();
        corrupt.name = mounted.names["read_resource"].clone();
        assert!(
            loaded
                .live
                .historical_surface_projection(&corrupt, &json!({}))
                .is_none()
        );
        assert!(
            loaded
                .live
                .historical_surface_projection(old, &json!(null))
                .is_none()
        );
    }
    loaded.live = unconsumed;
    let mut registry = Registry::new();
    registry.attach_mcp(
        &crate::config::Config::default(),
        super::super::Startup::Ready(loaded),
    );
    let schemas: Vec<_> = registry
        .names()
        .map(|name| registry.get(name).unwrap().schema())
        .collect();
    for (_, old, selector) in &old_wrappers {
        let messages = vec![
            json!({"role":"assistant","tool_calls":[{"id":"old","type":"function","function":{
                "name":old.name,"arguments":"{}"
            }}],"dial_tool_contracts":{"old":old}}),
            json!({"role":"tool","tool_call_id":"old","content":"retained result"}),
        ];
        let original = messages.clone();
        let projected =
            crate::tools::surface::contracts::project_history(&messages, &schemas, &registry)
                .unwrap()
                .unwrap();
        assert_eq!(messages, original, "durable history is untouched");
        assert_eq!(
            projected[1], original[1],
            "settled output is never replayed"
        );
        let name = projected[0]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert!(registry.names().any(|offered| offered == name));
        let arguments: Value = serde_json::from_str(
            projected[0]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(arguments, json!({"surface":selector}));
        assert_eq!(projected[0]["dial_tool_contracts"]["old"]["name"], name);
        assert!(
            crate::tools::surface::contracts::project_history(&projected, &schemas, &registry)
                .unwrap()
                .is_none()
        );
    }
}
