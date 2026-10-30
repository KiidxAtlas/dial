use super::*;
use crate::{
    config::types::Config,
    tools::{
        Workspace,
        integrations::{ext::Registry, plugin::Plugin},
    },
};

struct Fixture {
    owner: String,
    definitions: Vec<Definition>,
}
impl Plugin for Fixture {
    fn name(&self) -> &str {
        &self.owner
    }
    fn events(&self) -> &[String] {
        &[]
    }
    fn tools(&self) -> &[Definition] {
        &self.definitions
    }
    fn on_event(&self, _: &str, _: &Value, _: &AtomicBool) -> Result<Option<Value>> {
        Ok(None)
    }
    fn call(&self, method: &str, params: Value, _: &AtomicBool) -> Result<Value> {
        assert_eq!(method, "tool.call");
        assert_eq!(params["name"], "echo");
        Ok(json!({"text":params["arguments"]["text"], "is_error":false}))
    }
}
fn fixture(owner: &str) -> Arc<dyn Plugin> {
    Arc::new(Fixture { owner: owner.into(), definitions: definitions(&json!({"tools":[{
        "name":"echo", "description":"Echo text", "revision":2,
        "parameters":{"type":"object", "properties":{"text":{"type":"string"}}, "required":["text"], "additionalProperties":false}
    }]})).unwrap() })
}

#[test]
fn tools_validate_dispatch_and_follow_fork_lifecycle() {
    let mut registry = Registry::new();
    registry.register_plugin(fixture("demo")).unwrap();
    let name = "ext__demo__echo";
    let tool = registry.get(name).unwrap();
    assert!(tool.needs_approval(&json!({})));
    assert!(tool.may_modify_workspace(&json!({})));
    assert!(!tool.read_only_hint());
    assert_eq!(tool.contract_revision(), 2);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    assert_eq!(tool.call(&ctx, &json!({"text":"hello"})).text, "hello");
    assert!(tool.call(&ctx, &json!({"text":42})).is_error);
    let cancelled =
        tool.call_streaming(&ctx, &json!({"text":"hi"}), &|_| {}, &AtomicBool::new(true));
    assert!(cancelled.is_error);
    let fork = registry.fork_session();
    registry.plugins().unwrap().close();
    assert!(tool.call(&ctx, &json!({"text":"closed"})).is_error);
    assert_eq!(
        fork.get(name)
            .unwrap()
            .call(&ctx, &json!({"text":"child"}))
            .text,
        "child"
    );
    fork.plugins().unwrap().unregister("demo");
    assert!(
        fork.get(name)
            .unwrap()
            .call(&ctx, &json!({"text":"unloaded"}))
            .is_error
    );
    fork.plugins().unwrap().register(fixture("demo")).unwrap();
    // A new registration under the same name cannot inherit an old tool's admitted contract.
    assert!(
        fork.get(name)
            .unwrap()
            .call(&ctx, &json!({"text":"replacement"}))
            .is_error
    );
}

#[test]
fn plugin_tool_contract_rejects_invalid_definitions_and_collisions() {
    for value in [
        json!({"tools":[{"name":"bad name","description":"bad","parameters":{}}]}),
        json!({"tools":[{"name":"ok","description":"bad","parameters":{},"revision":0}]}),
        json!({"tools":[{"name":"ok","description":"bad","parameters":42}]}),
        json!({"tools":{}}),
    ] {
        assert!(definitions(&value).is_err(), "{value}");
    }
    let mut registry = Registry::new();
    registry.register_plugin(fixture("a-b")).unwrap();
    registry.register_plugin(fixture("a_b")).unwrap();
    assert!(registry.get("ext__a-b__echo").is_some());
    assert!(registry.get("ext__a_ub__echo").is_some());
    assert!(registry.register_plugin(fixture("a_b")).is_err());
    assert_eq!(registry.plugins().unwrap().tools().len(), 2);
}

#[cfg(unix)]
#[test]
fn process_handshake_accepts_tool_only_plugins_and_dispatches() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("plugin.sh");
    std::fs::write(&program, r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2","tools":[{"name":"echo","description":"Echo","parameters":{"type":"object"}}]}}\n' ;;
  *) printf '{"jsonrpc":"2.0","id":1,"result":{"text":"from process","is_error":false}}\n' ;;
esac
"#).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let host = Arc::new(Host::default());
    host.start_one("process", &program, &[], &Default::default(), 3);
    assert!(host.take_notices().is_empty());
    assert_eq!(host.tools().len(), 1);
    assert_eq!(
        host.request(
            "process",
            "tool.call",
            json!({"name":"echo","arguments":{}}),
            &AtomicBool::new(false)
        )
        .unwrap()["text"],
        "from process"
    );
}

struct CallbackFixture;
impl Plugin for CallbackFixture {
    fn name(&self) -> &str {
        "callbacks"
    }
    fn events(&self) -> &[String] {
        &[]
    }
    fn on_event(&self, _: &str, _: &Value, _: &AtomicBool) -> Result<Option<Value>> {
        Ok(None)
    }
    fn call_with_host(
        &self,
        _: &str,
        params: Value,
        _: &AtomicBool,
        host: &mut super::super::HostCall<'_>,
    ) -> Result<Value> {
        let arguments = &params["arguments"];
        let response = host(arguments["method"].as_str().unwrap(), &arguments["params"])?;
        Ok(json!({"text":"callback complete", "is_error":false, "data":response}))
    }
}

#[test]
fn callback_dispatch_uses_controller_and_preserves_failed_tool_facts() {
    let host = Arc::new(Host::default());
    let target: Arc<dyn Plugin> = Arc::new(CallbackFixture);
    host.register(target.clone()).unwrap();
    let tool = PluginTool::new(
        host,
        target,
        Definition {
            name: "callback".into(),
            description: "Callback".into(),
            parameters: json!({"type":"object"}),
            revision: 1,
        },
    );
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(directory.path()).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let events = parking_lot::Mutex::new(Vec::new());
    let mut calls = Vec::new();
    let mut dispatcher = |request| {
        calls.push(request);
        let mut output = ToolOutput::err("approval denied");
        output.data = Some(json!({"reason":"user denied"}));
        output
    };
    assert!(tool.supports_tool_dispatch());
    for (method, params, success) in [
        (
            "tools.call",
            json!({"name":"task","arguments":{"prompt":"test"}}),
            true,
        ),
        ("progress", json!({"text":"working"}), true),
        ("context.get", json!({}), false),
        ("tools.call", json!({"arguments":{}}), false),
    ] {
        let output = tool.call_with_dispatch(
            &ctx,
            &json!({"method":method,"params":params}),
            &|event| events.lock().push(event),
            &AtomicBool::new(false),
            &|_| Err("no adoption".into()),
            &mut dispatcher,
        );
        assert_eq!(!output.is_error, success, "{method}: {}", output.text);
        if method == "tools.call" && success {
            let value = output.data.unwrap();
            assert_eq!(value["data"]["is_error"], true);
            assert_eq!(value["data"]["data"]["reason"], "user denied");
        }
    }
    assert_eq!(events.lock().len(), 1);
    assert!(matches!(&calls[..], [ToolCallback::Call {name,..}] if name == "task"));
}

struct RendererFixture(crate::runtime::extensions::Contributions);
impl Plugin for RendererFixture {
    fn name(&self) -> &str {
        "rendered"
    }
    fn events(&self) -> &[String] {
        &[]
    }
    fn contributions(&self) -> crate::runtime::extensions::Contributions {
        self.0.clone()
    }
    fn on_event(&self, _: &str, _: &Value, _: &AtomicBool) -> Result<Option<Value>> {
        Ok(None)
    }
    fn call(&self, _: &str, params: Value, _: &AtomicBool) -> Result<Value> {
        Ok(params["arguments"].clone())
    }
}

#[test]
fn renderer_projects_only_declared_json_pointers_without_mutating_results() {
    let contributions = crate::runtime::extensions::Contributions::parse(json!({"renderers":[{
        "id":"summary", "title":"Inspection", "markdown":"Plain summary", "fields":[
            {"label":"Finding", "path":"/nested/a~1b"}, {"label":"Absent", "path":"/missing"}
        ]
    }]}))
    .unwrap();
    let target: Arc<dyn Plugin> = Arc::new(RendererFixture(contributions));
    let host = Arc::new(Host::default());
    host.register(target.clone()).unwrap();
    let tool = PluginTool::new(
        host,
        target,
        Definition {
            name: "report".into(),
            description: "Report".into(),
            parameters: json!({"type":"object"}),
            revision: 1,
        },
    );
    let mut output = ToolOutput::err("original evidence")
        .with_effects(crate::runtime::execution::Effects::Unknown);
    output.data =
        Some(json!({"renderer":"summary","data":{"nested":{"a/b":"hello\u{001b}[31m\nworld"}}}));
    let before = (
        output.text.clone(),
        output.data.clone(),
        serde_json::to_value(&output.execution).unwrap(),
    );
    let presentation = tool.render_result(&json!({}), &output);
    assert_eq!(presentation.label, "Inspection");
    assert!(
        presentation
            .facts
            .contains(&"Finding: hello[31m world".into())
    );
    assert_eq!(
        presentation.preview.as_deref(),
        Some("Plain summary\nFinding: hello[31m world")
    );
    assert_eq!(
        before,
        (
            output.text.clone(),
            output.data.clone(),
            serde_json::to_value(&output.execution).unwrap()
        )
    );
    output.data.as_mut().unwrap()["renderer"] = json!("unknown");
    assert_ne!(tool.render_result(&json!({}), &output).label, "Inspection");
}

#[test]
fn renderer_roundtrip_preserves_failure_data_and_bounds_untrusted_projection() {
    let target: Arc<dyn Plugin> = Arc::new(RendererFixture(
        crate::runtime::extensions::Contributions::parse(json!({"renderers":[{
            "id":"summary", "title":"Report\n title", "fields":[
                {"label":"Item", "path":"/items/0/~0key"},
                {"label":"Invalid", "path":"not/a/pointer"},
                {"label":"Too far", "path":"/items/99"}
            ]
        }]}))
        .unwrap(),
    ));
    let host = Arc::new(Host::default());
    host.register(target.clone()).unwrap();
    let tool = PluginTool::new(
        host,
        target,
        Definition {
            name: "report".into(),
            description: "Report".into(),
            parameters: json!({"type":"object"}),
            revision: 1,
        },
    );
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(directory.path()).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let envelope = json!({"text":"failure evidence", "is_error":true, "renderer":"summary", "data":{"items":[{"~key":"é".repeat(4096)}]}});
    let output = tool.call(&ctx, &envelope);
    assert!(output.is_error);
    assert_eq!(output.text, "failure evidence");
    assert_eq!(output.data.as_ref(), Some(&envelope));
    let baseline =
        crate::tools::surface::presentation::render_result(tool.name(), &json!({}), &output);
    let presentation = tool.render_result(&json!({}), &output);
    assert_eq!(presentation.label, "Report title");
    assert_eq!(presentation.tone, baseline.tone);
    assert!(presentation.preview.as_ref().unwrap().len() <= 8192);
    assert!(
        presentation
            .facts
            .iter()
            .any(|fact| fact.starts_with("Item: ") && fact.len() < 1100)
    );
    assert!(
        !presentation
            .facts
            .iter()
            .any(|fact| fact.starts_with("Invalid:") || fact.starts_with("Too far:"))
    );
    for renderer in [Value::Null, json!(42), json!("unregistered")] {
        let mut fallback = output.clone();
        fallback.data.as_mut().unwrap()["renderer"] = renderer;
        assert_eq!(
            tool.render_result(&json!({}), &fallback).label,
            baseline.label
        );
        assert!(
            tool.render_result(&json!({}), &fallback)
                .facts
                .iter()
                .any(|fact| {
                    fact.starts_with("Invalid extension renderer:")
                        || fact.starts_with("Unknown extension renderer:")
                })
        );
    }
    assert_eq!(output.data.as_ref(), Some(&envelope));
}
