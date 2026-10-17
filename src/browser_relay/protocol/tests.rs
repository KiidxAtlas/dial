use serde_json::json;

use super::*;

#[test]
fn tab_snapshot_reads_the_chrome_target_id_and_defaults_it_empty() {
    let tab: TabSnapshot = serde_json::from_str(r#"{"tabId":4,"targetId":"A1B2"}"#).unwrap();
    assert_eq!(tab.target_id, "A1B2");
    let bare: TabSnapshot = serde_json::from_str(r#"{"tabId":5}"#).unwrap();
    assert_eq!(bare.target_id, "");
}

#[test]
fn extension_hello_parses_tabs_with_defaults() {
    let message: ExtMessage = serde_json::from_str(
        r#"{"t":"hello","dialProtocol":1,"extensionVersion":"1.0.0",
                "tabs":[{"tabId":7,"url":"https://a.test/","discarded":true}]}"#,
    )
    .unwrap();
    let ExtMessage::Hello {
        dial_protocol,
        extension_version,
        product,
        tabs,
        ..
    } = message
    else {
        panic!("expected hello");
    };
    assert_eq!(dial_protocol, PROTOCOL_VERSION);
    assert_eq!(extension_version, "1.0.0");
    assert_eq!(product, "");
    assert_eq!(
        tabs,
        vec![TabSnapshot {
            tab_id: 7,
            url: "https://a.test/".into(),
            title: String::new(),
            target_id: String::new(),
            discarded: true,
        }]
    );
}

#[test]
fn extension_child_event_keeps_session_id() {
    let message: ExtMessage = serde_json::from_str(
        r#"{"t":"cdpEvent","tabId":3,"sessionId":"S1","method":"Runtime.consoleAPICalled","params":{"a":1}}"#,
    )
    .unwrap();
    assert_eq!(
        message,
        ExtMessage::CdpEvent {
            tab_id: 3,
            session_id: Some("S1".into()),
            method: "Runtime.consoleAPICalled".into(),
            params: json!({"a": 1}),
        }
    );
}

#[test]
fn rpc_result_without_error_defaults_to_empty() {
    let message: ExtMessage =
        serde_json::from_str(r#"{"t":"rpcResult","id":9,"ok":true,"result":{"tabId":4}}"#).unwrap();
    assert_eq!(
        message,
        ExtMessage::RpcResult {
            id: 9,
            ok: true,
            result: json!({"tabId": 4}),
            error: String::new(),
        }
    );
}

#[test]
fn unknown_or_malformed_extension_frames_are_rejected() {
    assert!(serde_json::from_str::<ExtMessage>(r#"{"t":"bogus"}"#).is_err());
    assert!(serde_json::from_str::<ExtMessage>(r#"{"t":"hello"}"#).is_err());
    assert!(serde_json::from_str::<ExtMessage>(r#"{"t":"cdpEvent","tabId":1}"#).is_err());
}

#[test]
fn rpc_frame_flattens_the_operation_next_to_its_id() {
    let op = RelayOp::Send {
        tab_id: 5,
        session_id: Some("CHILD".into()),
        method: "Runtime.evaluate".into(),
        params: json!({"expression": "1+1"}),
    };
    let value = serde_json::to_value(RpcFrame::new(11, &op)).unwrap();
    assert_eq!(
        value,
        json!({
            "t": "rpc",
            "id": 11,
            "op": "send",
            "tabId": 5,
            "sessionId": "CHILD",
            "method": "Runtime.evaluate",
            "params": {"expression": "1+1"},
        })
    );
}

#[test]
fn top_level_send_omits_session_id_and_attach_has_only_tab() {
    let send = RelayOp::Send {
        tab_id: 1,
        session_id: None,
        method: "Page.enable".into(),
        params: json!({}),
    };
    let value = serde_json::to_value(&send).unwrap();
    assert!(value.get("sessionId").is_none());

    let attach = serde_json::to_value(RelayOp::Attach { tab_id: 2 }).unwrap();
    assert_eq!(attach, json!({"op": "attach", "tabId": 2}));
}

#[test]
fn pong_frame_is_the_documented_wire_shape() {
    let value: Value = serde_json::from_str(&pong_frame()).unwrap();
    assert_eq!(value, json!({"t": "pong"}));
}
