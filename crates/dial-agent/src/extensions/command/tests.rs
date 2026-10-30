#![cfg(unix)]

use super::*;
use crate::agent::Ui;
use crate::extensions::panel::{Panel, Value as PanelValue};
use std::collections::BTreeMap;

/// A real extension process. `command` opens a panel; `command.panel` answers with the kind of
/// outcome it received, so the returned text proves what was delivered back.
const FIXTURE: &str = r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"commands":["demo"]}}' ;;
  *'"method":"command.panel"'*)
    case "$request" in
      *'"status":"submitted"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"text":"got submitted"}}' ;;
      *'"status":"cancelled"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"text":"got cancelled"}}' ;;
      *) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"text":"got unsupported"}}' ;;
    esac ;;
  *) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"panel":{"title":"Pick","generation":7,"elements":[{"type":"input","id":"name","label":"Name"},{"type":"button","id":"ok","label":"OK","primary":true}]}}}' ;;
esac
"#;

/// Answers the panel with a fixed outcome, optionally unloading the extension first to prove that
/// the close request is bound to the extension still being loaded.
struct FakeUi<'a> {
    host: Option<&'a Host>,
    outcome: Outcome,
    shown: Option<String>,
}

impl Ui for FakeUi<'_> {
    fn run_panel(&mut self, panel: &Panel, _cancel: &AtomicBool) -> Outcome {
        self.shown = Some(panel.title.clone());
        if let Some(host) = self.host {
            assert!(host.unregister("fixture"));
        }
        self.outcome.clone()
    }
}

fn host_with_fixture() -> (tempfile::TempDir, Host) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("plugin.sh");
    std::fs::write(&program, FIXTURE).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let host = Host::default();
    host.start_one("fixture", &program, &[], &BTreeMap::new(), 2);
    (dir, host)
}

fn submitted() -> Outcome {
    Outcome::Submitted {
        button: "ok".to_owned(),
        values: BTreeMap::from([("name".to_owned(), PanelValue::Text("kit".to_owned()))]),
    }
}

#[cfg(unix)]
#[test]
fn submitted_panel_reports_the_submission_back_to_its_extension() {
    let (_dir, host) = host_with_fixture();
    let idle = AtomicBool::new(false);
    assert_eq!(host.command_owner("demo").as_deref(), Some("fixture"));
    let mut ui = FakeUi {
        host: None,
        outcome: submitted(),
        shown: None,
    };
    let text = run(&mut ui, &host, "fixture", "demo", "", &idle).unwrap();
    assert_eq!(ui.shown.as_deref(), Some("Pick"));
    assert_eq!(text, "got submitted");
}

#[cfg(unix)]
#[test]
fn cancelled_panel_reports_the_cancellation_back() {
    let (_dir, host) = host_with_fixture();
    let idle = AtomicBool::new(false);
    let mut ui = FakeUi {
        host: None,
        outcome: Outcome::Cancelled,
        shown: None,
    };
    let text = run(&mut ui, &host, "fixture", "demo", "", &idle).unwrap();
    assert_eq!(text, "got cancelled");
}

#[cfg(unix)]
#[test]
fn unsupported_panel_is_an_explicit_error_after_the_extension_hears_about_it() {
    let (_dir, host) = host_with_fixture();
    let idle = AtomicBool::new(false);
    let mut ui = FakeUi {
        host: None,
        outcome: Outcome::Unsupported("no terminal".to_owned()),
        shown: None,
    };
    let err = run(&mut ui, &host, "fixture", "demo", "", &idle).unwrap_err();
    assert_eq!(err.to_string(), "demo: no terminal");
}

#[cfg(unix)]
#[test]
fn unloading_the_extension_while_the_panel_is_open_fails_the_close() {
    let (_dir, host) = host_with_fixture();
    let idle = AtomicBool::new(false);
    let mut ui = FakeUi {
        host: Some(&host),
        outcome: submitted(),
        shown: None,
    };
    let err = run(&mut ui, &host, "fixture", "demo", "", &idle).unwrap_err();
    assert!(err.to_string().contains("is not loaded"), "{err}");
}

#[test]
fn registered_command_schema_is_admitted_before_the_extension_runs() {
    use crate::runtime::extensions::Contributions;
    use crate::tools::integrations::plugin::Plugin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Structured(Arc<AtomicUsize>);
    impl Plugin for Structured {
        fn name(&self) -> &str {
            "fixture"
        }
        fn events(&self) -> &[String] {
            &[]
        }
        fn contributions(&self) -> Contributions {
            serde_json::from_value(json!({"commands":[{"name":"typed","description":"Typed input","parameters":{"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false},"keybinding":null}]})).unwrap()
        }
        fn on_event(&self, _: &str, params: &Value, _: &AtomicBool) -> Result<Option<Value>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert_eq!(params["args"], "{\"count\":2}");
            assert_eq!(params["arguments"], json!({"count":2}));
            Ok(Some(json!({"text":"accepted"})))
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let host = Host::default();
    host.register(Arc::new(Structured(calls.clone()))).unwrap();
    let mut ui = FakeUi {
        host: None,
        outcome: Outcome::Cancelled,
        shown: None,
    };
    let cancel = AtomicBool::new(false);
    for args in [
        "free text",
        "{}",
        "{\"count\":0}",
        "{\"count\":2,\"extra\":true}",
    ] {
        assert!(run(&mut ui, &host, "fixture", "typed", args, &cancel).is_err());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        run(&mut ui, &host, "fixture", "typed", "{\"count\":2}", &cancel).unwrap(),
        "accepted"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
