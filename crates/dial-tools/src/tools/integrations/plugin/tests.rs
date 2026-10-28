use super::*;
#[cfg(unix)]
#[test]
fn stdio_plugin_handshake_and_event_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("plugin.sh");
    std::fs::write(
        &program,
        r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"events":["pre_tool_use"]}}\n' ;;
  *) printf '{"jsonrpc":"2.0","id":1,"result":{"rewrite":{"input":{"command":"safe"} }}}\n' ;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = ProcessPlugin::start("fixture", &program, &[], &Default::default(), 2).unwrap();
    assert_eq!(plugin.events(), ["pre_tool_use"]);
    let rewrite = plugin
        .on_event("pre_tool_use", &json!({}), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(rewrite, Some(json!({"input":{"command":"safe"}})));
}

#[cfg(unix)]
#[test]
fn declared_command_owner_answers_panel_requests_and_unload_stops_them() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("plugin.sh");
    std::fs::write(
        &program,
        r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"events":["pre_tool_use"],"commands":["demo"]}}\n' ;;
  *'"method":"command.panel"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"text":"closed"}}\n' ;;
  *) printf '{"jsonrpc":"2.0","id":1,"result":{"panel":{"title":"Demo"}}}\n' ;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = ProcessPlugin::start("fixture", &program, &[], &Default::default(), 2).unwrap();
    let host = Host::default();
    host.register(Arc::new(plugin)).unwrap();
    let idle = AtomicBool::new(false);
    assert_eq!(host.command_owner("demo").as_deref(), Some("fixture"));
    assert_eq!(host.command_owner("other"), None);
    assert_eq!(
        host.request("fixture", "command", json!({"name":"demo"}), &idle)
            .unwrap(),
        json!({"panel":{"title":"Demo"}})
    );
    assert_eq!(
        host.request("fixture", "command.panel", json!({}), &idle)
            .unwrap(),
        json!({"text":"closed"})
    );
    assert!(host.unregister("fixture"));
    let err = host
        .request("fixture", "command.panel", json!({}), &idle)
        .unwrap_err();
    assert!(err.to_string().contains("is not loaded"), "{err}");
}

struct Declares {
    commands: Vec<String>,
}
impl Plugin for Declares {
    fn name(&self) -> &str {
        "declares"
    }
    fn events(&self) -> &[String] {
        &[]
    }
    fn commands(&self) -> &[String] {
        &self.commands
    }
    fn on_event(&self, _: &str, _: &Value, _: &AtomicBool) -> anyhow::Result<Option<Value>> {
        Ok(None)
    }
}

#[test]
fn command_names_drop_malformed_protocol_text_and_keep_the_owner() {
    let host = Host::default();
    let overlong = "x".repeat(MAX_COMMAND_NAME_CHARS + 1);
    host.register(Arc::new(Declares {
        commands: vec![
            "demo".into(),
            String::new(),
            "two words".into(),
            "/slash".into(),
            overlong,
            "ok-name".into(),
        ],
    }))
    .unwrap();
    assert_eq!(
        host.command_names(),
        vec![
            ("demo".to_owned(), "declares".to_owned()),
            ("ok-name".to_owned(), "declares".to_owned()),
        ]
    );
}

#[cfg(unix)]
fn host_with_fixture(script: &str) -> (tempfile::TempDir, Host) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("plugin.sh");
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = ProcessPlugin::start("fixture", &program, &[], &Default::default(), 2).unwrap();
    let host = Host::default();
    host.register(Arc::new(plugin)).unwrap();
    (dir, host)
}

#[cfg(unix)]
const V2_STATUS_AND_MESSAGE: &str = r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2","capabilities":["ui.status","actions.send_user_message"],"events":["turn_end"]}}\n' ;;
  *'"method":"event"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"actions":[{"type":"ui","slot":"status","lines":["ready"]},{"type":"send_user_message","text":"hello"}]}}\n' ;;
esac
"#;

#[cfg(unix)]
const LEGACY_WITH_ACTIONS: &str = r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"events":["turn_end"]}}\n' ;;
  *'"method":"event"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"actions":[{"type":"ui","slot":"status","lines":["ready"]},{"type":"send_user_message","text":"hello"}]}}\n' ;;
esac
"#;

#[cfg(unix)]
const UNDECLARED_ACTION: &str = r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2","capabilities":["ui.status"],"events":["turn_end"]}}\n' ;;
  *'"method":"event"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"actions":[{"type":"send_user_message","text":"hello"},{"type":"ui","slot":"status","lines":["ready"]}]}}\n' ;;
esac
"#;

#[cfg(unix)]
const AUTOCOMPLETE: &str = r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2","capabilities":["autocomplete"],"events":["turn_end"]}}\n' ;;
  *'"method":"autocomplete"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"items":[{"value":"alpha","label":"Alpha"},{"value":"  "}]}}\n' ;;
esac
"#;

#[cfg(unix)]
const STATUS_TICK: &str = r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2","capabilities":["ui.status"],"events":["turn_end"]}}\n' ;;
  *'"method":"event"'*) printf '{"jsonrpc":"2.0","id":1,"result":{"actions":[{"type":"ui","slot":"status","lines":["tick"]}]}}\n' ;;
esac
"#;

#[cfg(unix)]
#[test]
fn protocol_two_actions_apply_on_dispatch_and_unregister_clears_them() {
    let (_dir, host) = host_with_fixture(V2_STATUS_AND_MESSAGE);
    let idle = AtomicBool::new(false);
    let answers = host.dispatch("turn_end", &json!({}), &idle);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].result.as_ref().ok(), Some(&Value::Null));
    let slots = host.ui_slots();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].0, "fixture");
    assert_eq!(slots[0].1.lines, ["ready"]);
    let effects = host.take_effects();
    assert_eq!(effects.len(), 1);
    assert!(matches!(&effects[0].action, Action::SendUserMessage { text } if text == "hello"));
    assert!(host.take_effects().is_empty());
    assert!(host.unregister("fixture"));
    assert!(host.ui_slots().is_empty());
}

#[cfg(unix)]
#[test]
fn legacy_protocol_plugins_cannot_emit_actions() {
    let (_dir, host) = host_with_fixture(LEGACY_WITH_ACTIONS);
    let idle = AtomicBool::new(false);
    host.dispatch("turn_end", &json!({}), &idle);
    assert!(host.ui_slots().is_empty());
    assert!(host.take_effects().is_empty());
    assert!(host.take_notices().is_empty());
}

#[cfg(unix)]
#[test]
fn undeclared_action_capability_is_refused_without_dropping_other_actions() {
    let (_dir, host) = host_with_fixture(UNDECLARED_ACTION);
    let idle = AtomicBool::new(false);
    host.dispatch("turn_end", &json!({}), &idle);
    assert_eq!(host.ui_slots().len(), 1);
    assert!(host.take_effects().is_empty());
    assert!(!host.take_notices().is_empty());
}

#[cfg(unix)]
#[test]
fn autocomplete_queries_only_plugins_that_declared_it() {
    let (_dir, host) = host_with_fixture(AUTOCOMPLETE);
    let idle = AtomicBool::new(false);
    let suggestions = host.autocomplete("al", &idle);
    assert_eq!(
        suggestions
            .iter()
            .map(|s| s.value.as_str())
            .collect::<Vec<_>>(),
        ["alpha"]
    );
    let (_other_dir, without) = host_with_fixture(V2_STATUS_AND_MESSAGE);
    assert!(without.autocomplete("al", &idle).is_empty());
}

#[cfg(unix)]
#[test]
fn emitted_events_reach_the_observer_worker_and_update_slots() {
    let (_dir, host) = host_with_fixture(STATUS_TICK);
    host.emit("turn_end", &json!({}));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while host.ui_slots().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "observer never delivered the event"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(host.ui_slots()[0].1.lines, ["tick"]);
}
#[cfg(unix)]
const ORDER_LOGGER: &str = r#"#!/bin/sh
while read request; do
  case "$request" in
    *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2","capabilities":[],"events":["turn_start","message_start","turn_end"]}}\n' ;;
    *'"method":"event"'*) printf '%s\n' "$request" >> "LOG_PATH"; printf '{"jsonrpc":"2.0","id":1,"result":{}}\n' ;;
  esac
done
"#;

#[cfg(unix)]
#[test]
fn observer_events_arrive_in_emit_order_with_session_payloads() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("events.log");
    let script = ORDER_LOGGER.replace("LOG_PATH", &log.display().to_string());
    let (_dir, host) = host_with_fixture(&script);
    let order = ["turn_start", "message_start", "turn_end"];
    for name in order {
        host.emit(name, &json!({"session_id": "s1"}));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let delivered = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if lines.len() == order.len() {
            break lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "observer delivered {} of {} events",
            lines.len(),
            order.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let names: Vec<&str> = delivered
        .iter()
        .filter_map(|line| line["params"]["event"].as_str())
        .collect();
    assert_eq!(names, order);
    assert!(
        delivered
            .iter()
            .all(|line| line["params"]["payload"]["session_id"] == "s1")
    );
}
