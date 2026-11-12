use super::*;
use crate::tools::integrations::ext::{Tool, ToolContext};

struct Fake {
    name: &'static str,
    actions: &'static [&'static str],
    approval: bool,
    read_only: bool,
}

impl Tool for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> Value {
        tool_schema(
            self.name,
            "fake",
            json!({"action":{"type":"string","enum":self.actions},"path":{"type":"string","maxLength":8},"own":{"type":"integer"}}),
            &["action"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        self.approval
    }
    fn read_only_hint(&self) -> bool {
        self.read_only
    }
    fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
        ToolOutput::ok(format!("{} {}", self.name, args))
    }
}

fn fake(name: &'static str, actions: &'static [&'static str], approval: bool) -> Arc<dyn Tool> {
    Arc::new(Fake {
        name,
        actions,
        approval,
        read_only: !approval,
    })
}

fn selector_facade() -> Facade {
    Facade::new(
        "inspect",
        "inspect files",
        "which reader",
        None,
        vec![
            Member::select("kind", "data", fake("data", &["profile"], false)),
            Member::select("kind", "audio", fake("audio", &["analyze"], true)),
        ],
    )
}

#[test]
fn select_routes_strip_the_selector_and_keep_member_arguments() {
    let facade = selector_facade();
    let (owner, own) = facade
        .resolve(&json!({"kind":"audio","action":"analyze","path":"a"}))
        .unwrap();
    assert_eq!(owner.name(), "audio");
    assert_eq!(own, json!({"action":"analyze","path":"a"}));
    assert!(facade.needs_approval(&json!({"kind":"audio","action":"analyze"})));
    assert!(!facade.needs_approval(&json!({"kind":"data","action":"profile"})));
    assert!(facade.resolve(&json!({"kind":"video"})).is_none());
    assert!(facade.needs_approval(&json!({"kind":"video"})));
    assert!(facade.resolve(&json!({"action":"profile"})).is_none());
    assert!(!facade.read_only_hint());
}

#[test]
fn action_routes_translate_to_the_recorded_action_and_fall_back_to_native() {
    let facade = Facade::new(
        "bg",
        "jobs",
        "",
        Some(fake("bg", &["start", "list"], false)),
        vec![
            Member::action(
                "watch",
                Some("start"),
                fake("monitor", &["start", "stop"], true),
            ),
            Member::action(
                "unwatch",
                Some("stop"),
                fake("monitor", &["start", "stop"], true),
            ),
        ],
    );
    let (owner, own) = facade.resolve(&json!({"action":"watch","own":1})).unwrap();
    assert_eq!(
        (owner.name(), own),
        ("monitor", json!({"action":"start","own":1}))
    );
    let (owner, own) = facade.resolve(&json!({"action":"list"})).unwrap();
    assert_eq!((owner.name(), own), ("bg", json!({"action":"list"})));
    let schema = facade.schema();
    assert_eq!(
        schema["function"]["parameters"]["properties"]["action"]["enum"],
        json!(["start", "list", "watch", "unwatch"])
    );
    assert_eq!(
        schema["function"]["parameters"]["required"],
        json!(["action"])
    );
    assert_eq!(schema["function"]["description"], "jobs");
}

#[test]
fn schema_requires_the_selector_and_unites_member_properties() {
    let schema = selector_facade().schema();
    let parameters = &schema["function"]["parameters"];
    assert_eq!(parameters["required"], json!(["kind"]));
    assert_eq!(
        parameters["properties"]["kind"]["enum"],
        json!(["data", "audio"])
    );
    assert_eq!(
        parameters["properties"]["action"]["enum"],
        json!(["profile", "analyze"])
    );
    assert_eq!(parameters["properties"]["path"]["maxLength"], 8);
    crate::tools::surface::catalog::validate_definition(&schema).unwrap();
}

#[test]
fn calls_are_validated_against_the_member_schema() {
    let facade = selector_facade();
    let (_dir, workspace) = crate::tools::testkit::workspace();
    let config = crate::config::types::Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let refused = facade.call(&ctx, &json!({"kind":"data","action":"analyze"}));
    assert!(refused.is_error, "{}", refused.text);
    assert!(
        refused
            .text
            .contains("action=analyze belongs to kind=audio; resend with that selector"),
        "{}",
        refused.text
    );
    let invalid = facade.call(&ctx, &json!({"kind":"data","action":"profile","own":"x"}));
    assert!(
        invalid.is_error && !invalid.text.contains("belongs to"),
        "{}",
        invalid.text
    );
    let ok = facade.call(&ctx, &json!({"kind":"data","action":"profile","path":"x"}));
    assert!(!ok.is_error, "{}", ok.text);
    assert_eq!(ok.text, r#"data {"action":"profile","path":"x"}"#);
    let unknown = facade.call(&ctx, &json!({"kind":"nope"}));
    assert!(
        unknown.text.contains("kind=data, kind=audio"),
        "{}",
        unknown.text
    );
}

#[test]
fn offered_schema_accepts_member_actions_that_share_a_native_property() {
    let facade = Facade::new(
        "memory",
        "facts",
        "store",
        Some(fake("memory", &["recall", "retain"], false)),
        vec![Member::select(
            "store",
            "codebase",
            fake("codebase_memory", &["recall", "architecture"], false),
        )],
    );
    let schema = facade.schema();
    crate::tools::surface::catalog::validate_arguments(
        &schema,
        &json!({"store":"codebase","action":"architecture"}),
    )
    .unwrap();
    assert_eq!(
        schema["function"]["parameters"]["properties"]["action"]["enum"],
        json!(["recall", "retain", "architecture"])
    );
    assert!(facade.collisions().is_empty(), "{:?}", facade.collisions());
    let (owner, own) = facade
        .resolve(&json!({"store":"codebase","action":"architecture"}))
        .unwrap();
    assert_eq!(
        (owner.name(), own),
        ("codebase_memory", json!({"action":"architecture"}))
    );
}

#[test]
fn collisions_report_shadowed_routes() {
    let facade = Facade::new(
        "bg",
        "jobs",
        "",
        Some(fake("bg", &["start", "list"], false)),
        vec![
            Member::action("start", Some("start"), fake("monitor", &["start"], true)),
            Member::select("path", "x", fake("data", &["profile"], false)),
        ],
    );
    let found = facade.collisions();
    assert!(
        found.iter().any(|f| f.contains("action start")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.contains("selector path")),
        "{found:?}"
    );
}

#[test]
fn recorded_member_calls_project_onto_the_successor() {
    assert_eq!(
        successor_call("monitor", &json!({"action":"start","command":"x"})),
        Some(("bg", json!({"action":"watch","command":"x"})))
    );
    assert_eq!(
        successor_call("runtime_inspect", &json!({"action":"summary","path":"t"})),
        Some((
            "inspect",
            json!({"kind":"trace","action":"summary","path":"t"})
        ))
    );
    assert_eq!(
        successor_call("monitor", &json!({"action":"invented"})),
        None
    );
    assert_eq!(successor_call("bash", &json!({})), None);
    assert_eq!(successor_of("glab_mr"), Some("git"));
}

#[test]
fn shared_action_owners_are_forked_and_polled_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counted(Arc<AtomicUsize>);
    impl Tool for Counted {
        fn name(&self) -> &str {
            "monitor"
        }
        fn schema(&self) -> Value {
            tool_schema(
                "monitor",
                "fixture",
                json!({"action":{"type":"string"}}),
                &["action"],
            )
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            ToolOutput::ok("ok".into())
        }
        fn fork_session(&self) -> Option<Arc<dyn Tool>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some(Arc::new(Counted(Arc::clone(&self.0))))
        }
        fn poll_monitors(&self) -> Vec<String> {
            vec!["event".into()]
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let owner: Arc<dyn Tool> = Arc::new(Counted(Arc::clone(&count)));
    let facade = Facade::new(
        "bg",
        "fixture",
        "",
        None,
        vec![
            Member::action("watch", Some("start"), Arc::clone(&owner)),
            Member::action("unwatch", Some("stop"), owner),
        ],
    );
    assert_eq!(facade.poll_monitors(), ["event"]);
    let forked = facade.fork_session().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(forked.poll_monitors(), ["event"]);
}

#[test]
fn selected_member_does_not_inherit_native_required_fields() {
    let facade = Facade::new(
        "git",
        "fixture",
        "forge",
        Some(fake("git", &["status"], false)),
        vec![Member::select("forge", "github", Arc::new(ForgeFixture))],
    );
    struct ForgeFixture;
    impl Tool for ForgeFixture {
        fn name(&self) -> &str {
            "github"
        }
        fn schema(&self) -> Value {
            tool_schema(
                "github",
                "forge",
                json!({"op":{"type":"string","enum":["pr_view"]}}),
                &["op"],
            )
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            ToolOutput::ok("ok".into())
        }
    }
    crate::tools::surface::catalog::validate_arguments(
        &facade.schema(),
        &json!({"forge":"github","op":"pr_view"}),
    )
    .unwrap();
}

#[test]
fn builtins_and_controller_routes_round_trip_without_losing_arguments() {
    for (member, own) in [
        (
            "web_fetch",
            json!({"url":"https://example.com","raw":true,"max_chars":123}),
        ),
        (
            "ast_search",
            json!({"pattern":"foo($A)","lang":"rust","path":"src","limit":5}),
        ),
        ("tool_load", json!({"names":["read"],"unload":["bg"]})),
        (
            "skill_manage",
            json!({"action":"create","name":"x","body":"b"}),
        ),
        ("rewind", json!({"id":"c1"})),
        (
            "side_request",
            json!({"action":"wait","id":"side-1","timeout":1}),
        ),
        (
            "request_directory",
            json!({"path":"/outside","reason":"inspect"}),
        ),
    ] {
        let (successor, args) = successor_call(member, &own).unwrap();
        assert_eq!(
            member_call(successor, &args),
            Some((member, own)),
            "{member}: {args}"
        );
    }
}

#[test]
fn grep_ast_preserves_captures_and_source_observations() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    std::fs::write(ws.root().join("sample.rs"), "fn main() { foo(42); }\n").unwrap();
    let args = json!({"ast":true,"pattern":"foo($A)","lang":"rust","path":"sample.rs"});
    let output = crate::tools::dispatch(&ws, "grep", &args, 10).unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("42"), "{}", output.text);
    assert!(output.data.as_ref().unwrap()["source_observations"].is_array());
    assert!(
        crate::tools::dispatch(
            &ws,
            "grep",
            &json!({"ast":true,"pattern":"foo($A)","literal":true}),
            10
        )
        .is_err()
    );
}

#[test]
fn fetch_read_keeps_the_network_gate_and_refuses_incompatible_modes() {
    let (_dir, workspace) = crate::tools::testkit::workspace();
    let mut config = crate::config::types::Config::default();
    config.sandbox.mode = crate::config::types::SandboxMode::Strict;
    let context = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let output = crate::tools::read_with_context(
        &context,
        &json!({"path":"https://example.com","fetch":true}),
        &AtomicBool::new(false),
        2000,
    );
    assert!(
        output.is_error && output.text.contains("network access is disabled"),
        "{}",
        output.text
    );
    let output = crate::tools::read_with_context(
        &context,
        &json!({"path":"https://example.com","fetch":true,"format":"text"}),
        &AtomicBool::new(false),
        2000,
    );
    assert!(
        output.is_error && output.text.contains("fetch requires"),
        "{}",
        output.text
    );
}
