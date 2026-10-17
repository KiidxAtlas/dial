use std::sync::Arc;
use std::thread;

use serde_json::json;

use super::*;
use crate::browser_relay::outbox::{self, OutboxReceiver};

const WAIT: Duration = Duration::from_secs(5);

fn tab(id: i64, url: &str) -> TabSnapshot {
    TabSnapshot {
        tab_id: id,
        url: url.to_owned(),
        target_id: format!("CDP-TARGET-{id}"),
        title: format!("tab {id}"),
        discarded: false,
    }
}

fn hello(tabs: Vec<TabSnapshot>) -> ExtMessage {
    ExtMessage::Hello {
        dial_protocol: PROTOCOL_VERSION,
        extension_version: "1.0.0".into(),
        product: "Chrome/137.0.1".into(),
        user_agent: "Mozilla/5.0 Chrome/137.0.1".into(),
        tabs,
    }
}

/// Connects a fake extension whose RPC answers come from `answer(op) -> Result<Value, String>`.
/// Returns the relay, the link id, and a thread that answers every frame until the link ends.
fn fake_extension(
    relay: &Arc<Relay>,
    tabs: Vec<TabSnapshot>,
    answer: fn(&Value) -> Result<Value, String>,
) -> u64 {
    let (tx, rx) = outbox::channel();
    let link = relay.extension_hello(tx, hello(tabs)).unwrap();
    let relay = Arc::clone(relay);
    thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            let frame: Value = serde_json::from_str(&frame).unwrap();
            let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let reply = match answer(&frame) {
                Ok(result) => ExtMessage::RpcResult {
                    id,
                    ok: true,
                    result,
                    error: String::new(),
                },
                Err(error) => ExtMessage::RpcResult {
                    id,
                    ok: false,
                    result: Value::Null,
                    error,
                },
            };
            relay.extension_message(link, reply);
        }
    });
    link
}

fn standard_answer(frame: &Value) -> Result<Value, String> {
    match frame["op"].as_str() {
        Some("attach" | "detach" | "activateTab" | "removeTab") => Ok(json!({})),
        Some("createTab") => Ok(json!({"tabId": 77, "targetId": "CDP-TARGET-77"})),
        Some("send") => Ok(json!({"echo": frame["method"]})),
        other => Err(format!("unexpected op {other:?}")),
    }
}

fn client(relay: &Relay) -> (u64, OutboxReceiver) {
    let (tx, rx) = outbox::channel();
    (relay.connect_client(tx), rx)
}

fn next_frame(rx: &OutboxReceiver) -> Value {
    serde_json::from_str(&rx.recv_timeout(WAIT).expect("frame")).unwrap()
}

#[test]
fn internal_and_discarded_tabs_are_not_eligible() {
    assert!(eligible(&tab(1, "https://example.test/")));
    assert!(eligible(&tab(2, "")));
    assert!(eligible(&tab(3, "about:blank")));
    assert!(!eligible(&tab(4, "chrome://newtab/")));
    assert!(!eligible(&tab(
        5,
        "devtools://devtools/bundled/inspector.html"
    )));
    assert!(!eligible(&tab(6, "chrome-extension://abc/popup.html")));
    assert!(!eligible(&tab(7, "view-source:https://example.test/")));
    let discarded = TabSnapshot {
        discarded: true,
        ..tab(8, "https://example.test/")
    };
    assert!(!eligible(&discarded));
}

#[test]
fn target_info_uses_chrome_target_ids_and_the_constant_context() {
    let info = target_info(&tab(9, "https://example.test/a"), true);
    assert_eq!(info["targetId"], "CDP-TARGET-9");
    assert_eq!(info["type"], "page");
    assert_eq!(info["title"], "tab 9");
    assert_eq!(info["attached"], true);
    assert_eq!(info["browserContextId"], DEFAULT_CONTEXT_ID);
}

#[test]
fn target_info_falls_back_to_url_for_empty_titles_and_about_blank_for_empty_urls() {
    let untitled = TabSnapshot {
        title: String::new(),
        ..tab(10, "https://example.test/")
    };
    assert_eq!(
        target_info(&untitled, false)["title"],
        "https://example.test/"
    );
    let blank = TabSnapshot {
        url: String::new(),
        title: String::new(),
        ..tab(11, "")
    };
    assert_eq!(target_info(&blank, false)["url"], "about:blank");
}

#[test]
fn snapshot_reports_disconnected_state_and_hides_targets_until_connected() {
    let relay = Relay::new();
    let before = relay.snapshot();
    assert!(!before.connected);
    assert!(!before.extension_seen);
    assert!(before.targets.is_empty());
    assert!(before.disconnected_for.is_none());

    let (tx, _rx) = outbox::channel();
    let link = relay
        .extension_hello(tx, hello(vec![tab(1, "https://a.test/")]))
        .unwrap();
    let connected = relay.snapshot();
    assert!(connected.connected);
    assert!(connected.extension_seen);
    assert_eq!(connected.info.product, "Chrome/137.0.1");
    assert_eq!(connected.targets.len(), 1);

    relay.extension_disconnected(link);
    let gone = relay.snapshot();
    assert!(!gone.connected);
    assert!(gone.extension_seen);
    assert!(gone.disconnected_for.is_some());
}

#[test]
fn hello_with_another_protocol_version_is_rejected_with_an_update_hint() {
    let relay = Relay::new();
    let (tx, _rx) = outbox::channel();
    let stale = ExtMessage::Hello {
        dial_protocol: PROTOCOL_VERSION + 1,
        extension_version: "9".into(),
        product: String::new(),
        user_agent: String::new(),
        tabs: Vec::new(),
    };
    let error = relay.extension_hello(tx, stale).unwrap_err();
    assert!(error.contains("dial browser-relay install"), "{error}");
    assert!(!relay.snapshot().connected);
}

#[test]
fn a_newer_extension_connection_replaces_the_old_one() {
    let relay = Relay::new();
    let (tx1, _rx1) = outbox::channel();
    let first = relay
        .extension_hello(tx1, hello(vec![tab(1, "https://a.test/")]))
        .unwrap();
    let (tx2, _rx2) = outbox::channel();
    let second = relay
        .extension_hello(tx2, hello(vec![tab(2, "https://b.test/")]))
        .unwrap();
    assert_ne!(first, second);
    relay.extension_disconnected(first);
    assert!(
        relay.snapshot().connected,
        "stale link must not drop the live one"
    );
    let targets = relay.snapshot().targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["targetId"], "CDP-TARGET-2");
}

#[test]
fn browser_level_commands_answer_without_touching_the_extension() {
    let relay = Relay::new();
    let (tx, _rx) = outbox::channel();
    relay.extension_hello(tx, hello(Vec::new())).unwrap();
    let (id, _client_rx) = client(&relay);

    let version: Value = serde_json::from_str(
        &relay
            .handle_cdp(id, &json!({"id": 1, "method": "Browser.getVersion"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(version["result"]["product"], "Chrome/137.0.1");
    assert_eq!(version["result"]["protocolVersion"], "1.3");

    let root: Value = serde_json::from_str(
        &relay
            .handle_cdp(id, &json!({"id": 2, "method": "Target.getTargetInfo"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(root["result"]["targetInfo"]["type"], "browser");
    assert_eq!(root["result"]["targetInfo"]["targetId"], BROWSER_TARGET_ID);

    let contexts: Value = serde_json::from_str(
        &relay
            .handle_cdp(id, &json!({"id": 3, "method": "Target.getBrowserContexts"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        contexts["result"]["defaultBrowserContextId"],
        DEFAULT_CONTEXT_ID
    );
}

#[test]
fn unknown_browser_command_and_explicit_context_creation_are_errors() {
    let relay = Relay::new();
    let (id, _rx) = client(&relay);
    let unknown: Value = serde_json::from_str(
        &relay
            .handle_cdp(id, &json!({"id": 1, "method": "Bogus.thing"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(unknown["error"]["code"], -32601);
    let created: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                id,
                &json!({"id": 2, "method": "Target.createBrowserContext"}),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(
        created["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot create")
    );
}

#[test]
fn notifications_without_an_id_get_no_response() {
    let relay = Relay::new();
    let (id, _rx) = client(&relay);
    assert!(
        relay
            .handle_cdp(id, &json!({"method": "Page.enable"}))
            .is_none()
    );
}

#[test]
fn page_command_without_extension_reports_not_connected() {
    let relay = Relay::new();
    let (id, _rx) = client(&relay);
    let response: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                id,
                &json!({"id": 1, "method": "Target.createTarget", "params": {}}),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not connected")
    );
}

#[test]
fn auto_attach_announces_each_tab_and_routes_page_commands_by_session() {
    let relay = Arc::new(Relay::new());
    fake_extension(
        &relay,
        vec![tab(1, "https://a.test/"), tab(2, "chrome://newtab/")],
        standard_answer,
    );
    let (id, rx) = client(&relay);

    let set: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                id,
                &json!({"id": 1, "method": "Target.setAutoAttach", "params": {
                    "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(set["result"], json!({}));

    let attached = next_frame(&rx);
    assert_eq!(attached["method"], "Target.attachedToTarget");
    assert_eq!(attached["params"]["targetInfo"]["targetId"], "CDP-TARGET-1");
    assert_eq!(attached["params"]["waitingForDebugger"], false);
    let session = attached["params"]["sessionId"].as_str().unwrap().to_owned();
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "internal tab must not attach"
    );

    let evaluated: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                id,
                &json!({"id": 2, "sessionId": session, "method": "Runtime.evaluate",
                        "params": {"expression": "1+1"}}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(evaluated["sessionId"], session.as_str());
    assert_eq!(evaluated["result"]["echo"], "Runtime.evaluate");
}

#[test]
fn child_session_events_route_to_the_owning_client_and_commands_go_back_to_the_child() {
    let relay = Arc::new(Relay::new());
    let link = fake_extension(&relay, vec![tab(3, "https://a.test/")], |frame| {
        if frame["method"] == "Runtime.evaluate" {
            Ok(json!({"child": frame["sessionId"]}))
        } else {
            standard_answer(frame)
        }
    });
    let (id, rx) = client(&relay);
    relay.handle_cdp(
        id,
        &json!({"id": 1, "method": "Target.setAutoAttach", "params": {"autoAttach": true}}),
    );
    let top = next_frame(&rx)["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    relay.extension_message(
        link,
        ExtMessage::CdpEvent {
            tab_id: 3,
            session_id: None,
            method: "Target.attachedToTarget".into(),
            params: json!({"sessionId": "CHILD-9", "targetInfo": {"type": "iframe"}}),
        },
    );
    let forwarded = next_frame(&rx);
    assert_eq!(forwarded["sessionId"], top.as_str());
    assert_eq!(forwarded["method"], "Target.attachedToTarget");

    relay.extension_message(
        link,
        ExtMessage::CdpEvent {
            tab_id: 3,
            session_id: Some("CHILD-9".into()),
            method: "Runtime.consoleAPICalled".into(),
            params: json!({"type": "log"}),
        },
    );
    let child_event = next_frame(&rx);
    assert_eq!(child_event["sessionId"], "CHILD-9");

    let response: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                id,
                &json!({"id": 5, "sessionId": "CHILD-9", "method": "Runtime.evaluate",
                        "params": {}}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response["result"]["child"], "CHILD-9");
}

#[test]
fn sessions_of_other_clients_are_invisible() {
    let relay = Arc::new(Relay::new());
    fake_extension(&relay, vec![tab(4, "https://a.test/")], standard_answer);
    let (owner, _owner_rx) = client(&relay);
    let (other, _other_rx) = client(&relay);
    let session = relay.open_page_session(owner, 4).unwrap();
    let response: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                other,
                &json!({"id": 1, "sessionId": session, "method": "Runtime.evaluate",
                        "params": {}}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response["error"]["code"], -32001);
}

#[test]
fn last_top_level_session_detaches_the_debugger_when_the_client_leaves() {
    let relay = Arc::new(Relay::new());
    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    let (tx, rx) = outbox::channel();
    let link = relay
        .extension_hello(tx, hello(vec![tab(5, "https://a.test/")]))
        .unwrap();
    let answering = Arc::clone(&relay);
    thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            let frame: Value = serde_json::from_str(&frame).unwrap();
            seen_tx
                .send(frame["op"].as_str().unwrap().to_owned())
                .unwrap();
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                answering.extension_message(
                    link,
                    ExtMessage::RpcResult {
                        id,
                        ok: true,
                        result: json!({}),
                        error: String::new(),
                    },
                );
            }
        }
    });
    let (owner, _owner_rx) = client(&relay);
    relay.open_page_session(owner, 5).unwrap();
    assert_eq!(seen_rx.recv_timeout(WAIT).unwrap(), "attach");
    relay.disconnect_client(owner);
    assert_eq!(seen_rx.recv_timeout(WAIT).unwrap(), "detach");
    assert!(!is_attached(&relay.lock(), 5));
}

#[test]
fn extension_disconnect_tells_clients_their_sessions_ended() {
    let relay = Arc::new(Relay::new());
    let link = fake_extension(&relay, vec![tab(6, "https://a.test/")], standard_answer);
    let (id, rx) = client(&relay);
    relay.open_page_session(id, 6).unwrap();
    relay.extension_disconnected(link);
    let ended = next_frame(&rx);
    assert_eq!(ended["method"], "Target.detachedFromTarget");
    assert_eq!(ended["params"]["targetId"], "CDP-TARGET-6");
    assert!(!relay.snapshot().connected);
}

#[test]
fn extension_detached_event_closes_only_that_tabs_sessions() {
    let relay = Arc::new(Relay::new());
    let link = fake_extension(
        &relay,
        vec![tab(7, "https://a.test/"), tab(8, "https://b.test/")],
        standard_answer,
    );
    let (id, rx) = client(&relay);
    let keep = relay.open_page_session(id, 8).unwrap();
    let drop = relay.open_page_session(id, 7).unwrap();
    relay.extension_message(
        link,
        ExtMessage::Detached {
            tab_id: 7,
            reason: "canceled_by_user".into(),
        },
    );
    let ended = next_frame(&rx);
    assert_eq!(ended["params"]["sessionId"], drop.as_str());
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    let still = relay.handle_cdp(
        id,
        &json!({"id": 1, "sessionId": keep, "method": "Runtime.evaluate", "params": {}}),
    );
    assert!(still.unwrap().contains("\"echo\""));
}

#[test]
fn tab_updates_emit_discover_events_and_request_auto_attach_once_eligible() {
    let relay = Arc::new(Relay::new());
    let link = fake_extension(&relay, vec![tab(9, "chrome://newtab/")], standard_answer);
    let (id, rx) = client(&relay);
    relay.handle_cdp(
        id,
        &json!({"id": 1, "method": "Target.setDiscoverTargets", "params": {"discover": true}}),
    );
    let wanted = relay.extension_message(
        link,
        ExtMessage::TabUpdated {
            tab: tab(9, "https://a.test/"),
        },
    );
    assert_eq!(wanted, Vec::<i64>::new(), "no auto-attach client yet");
    let created = next_frame(&rx);
    assert_eq!(created["method"], "Target.targetCreated");
    assert_eq!(created["params"]["targetInfo"]["targetId"], "CDP-TARGET-9");

    relay.handle_cdp(
        id,
        &json!({"id": 2, "method": "Target.setAutoAttach", "params": {"autoAttach": true}}),
    );
    let wanted = relay.extension_message(
        link,
        ExtMessage::TabUpdated {
            tab: tab(9, "https://a.test/x"),
        },
    );
    assert_eq!(wanted, vec![9]);
}

#[test]
fn tab_removed_announces_destruction_and_ends_sessions() {
    let relay = Arc::new(Relay::new());
    let link = fake_extension(&relay, vec![tab(10, "https://a.test/")], standard_answer);
    let (id, rx) = client(&relay);
    relay.handle_cdp(
        id,
        &json!({"id": 1, "method": "Target.setDiscoverTargets", "params": {"discover": true}}),
    );
    let created = next_frame(&rx);
    assert_eq!(created["method"], "Target.targetCreated");
    let session = relay.open_page_session(id, 10).unwrap();
    relay.extension_message(link, ExtMessage::TabRemoved { tab_id: 10 });
    let detached = next_frame(&rx);
    assert_eq!(detached["params"]["sessionId"], session.as_str());
    let destroyed = next_frame(&rx);
    assert_eq!(destroyed["method"], "Target.targetDestroyed");
    assert!(relay.snapshot().targets.is_empty());
}

#[test]
fn create_target_returns_the_new_tab_and_attaches_for_auto_attach_clients() {
    let relay = Arc::new(Relay::new());
    fake_extension(&relay, Vec::new(), standard_answer);
    let (id, rx) = client(&relay);
    relay.handle_cdp(
        id,
        &json!({"id": 1, "method": "Target.setAutoAttach", "params": {"autoAttach": true}}),
    );
    let response: Value = serde_json::from_str(
        &relay
            .handle_cdp(
                id,
                &json!({"id": 2, "method": "Target.createTarget",
                        "params": {"url": "https://new.test/"}}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response["result"]["targetId"], "CDP-TARGET-77");
    let attached = next_frame(&rx);
    assert_eq!(attached["method"], "Target.attachedToTarget");
    assert_eq!(
        attached["params"]["targetInfo"]["targetId"],
        "CDP-TARGET-77"
    );
}

#[test]
fn a_failed_background_attach_is_recorded_for_status() {
    let relay = Arc::new(Relay::new());
    fake_extension(&relay, vec![tab(12, "https://a.test/")], |frame| {
        if frame["op"] == "attach" {
            Err("Another debugger is already attached to the tab".into())
        } else {
            standard_answer(frame)
        }
    });
    let (id, _rx) = client(&relay);
    relay.handle_cdp(
        id,
        &json!({"id": 1, "method": "Target.setAutoAttach", "params": {"autoAttach": true}}),
    );
    let error = relay.snapshot().last_error.unwrap();
    assert!(error.contains("tab 12"), "{error}");
    assert!(error.contains("already attached"), "{error}");
}

#[test]
fn attach_answered_after_its_client_left_is_released_without_a_session() {
    let relay = Arc::new(Relay::new());
    let (tx, rx) = outbox::channel();
    let link = relay
        .extension_hello(tx, hello(vec![tab(15, "https://a.test/")]))
        .unwrap();
    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let answering = Arc::clone(&relay);
    thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            let frame: Value = serde_json::from_str(&frame).unwrap();
            let op = frame["op"].as_str().unwrap_or_default().to_owned();
            seen_tx.send(op.clone()).unwrap();
            if op == "attach" {
                release_rx.recv().unwrap();
            }
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                answering.extension_message(
                    link,
                    ExtMessage::RpcResult {
                        id,
                        ok: true,
                        result: json!({}),
                        error: String::new(),
                    },
                );
            }
        }
    });
    let (owner, _owner_rx) = client(&relay);
    let opener = Arc::clone(&relay);
    let open = thread::spawn(move || opener.open_page_session(owner, 15));
    assert_eq!(seen_rx.recv_timeout(WAIT).unwrap(), "attach");
    relay.disconnect_client(owner);
    release_tx.send(()).unwrap();
    assert!(open.join().unwrap().is_err());
    assert_eq!(seen_rx.recv_timeout(WAIT).unwrap(), "detach");
    let st = relay.lock();
    assert!(!is_attached(&st, 15));
    assert!(st.sessions.is_empty());
}

#[test]
fn simultaneous_first_attachments_share_one_extension_attach() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let relay = Arc::new(Relay::new());
    let (tx, rx) = outbox::channel();
    let link = relay
        .extension_hello(tx, hello(vec![tab(16, "https://a.test/")]))
        .unwrap();
    let attaches = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attaches);
    let answering = Arc::clone(&relay);
    thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            let frame: Value = serde_json::from_str(&frame).unwrap();
            if frame["op"] == "attach" {
                counter.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(100));
            }
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                answering.extension_message(
                    link,
                    ExtMessage::RpcResult {
                        id,
                        ok: true,
                        result: json!({}),
                        error: String::new(),
                    },
                );
            }
        }
    });
    let (first, _first_rx) = client(&relay);
    let (second, _second_rx) = client(&relay);
    let (left, right) = thread::scope(|scope| {
        let left = scope.spawn(|| relay.open_page_session(first, 16));
        let right = scope.spawn(|| relay.open_page_session(second, 16));
        (left.join().unwrap(), right.join().unwrap())
    });
    assert!(left.is_ok() && right.is_ok(), "{left:?} {right:?}");
    assert_eq!(attaches.load(Ordering::SeqCst), 1);
    assert_eq!(relay.lock().attached.get(&16), Some(&2));
}

#[test]
fn extension_requests_are_refused_past_the_pending_budget() {
    let relay = Relay::new();
    let (tx, _rx) = outbox::channel();
    relay
        .extension_hello(tx, hello(vec![tab(17, "https://a.test/")]))
        .unwrap();
    let mut st = relay.lock();
    for id in 0..MAX_PENDING_RPCS as u64 {
        let (reply, _answer) = mpsc::channel();
        st.pending.insert(id, reply);
    }
    let error = queue_rpc(&mut st, &RelayOp::Detach { tab_id: 17 }).unwrap_err();
    assert_eq!(error.message, EXTENSION_BUSY);
}
