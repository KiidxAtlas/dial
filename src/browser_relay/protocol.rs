//! Wire contract shared by the Dial browser relay, its Chrome extension, and CDP clients.
//!
//! The extension speaks JSON text frames on `/ext`: it sends [`ExtMessage`] values and receives
//! [`RpcFrame`] values (plus `{"t":"pong"}`). CDP clients speak plain CDP JSON on `/cdp`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the extension-to-relay protocol. Both sides must report the same number.
pub const PROTOCOL_VERSION: u32 = 1;
/// Default loopback port of the browser relay.
pub const DEFAULT_PORT: u16 = 9224;
/// Default relay URL. Only loopback hosts are accepted.
pub const DEFAULT_RELAY_URL: &str = "http://127.0.0.1:9224";
/// HTTP header carrying the relay token. WebSocket URLs carry it as the `token` query parameter.
pub const TOKEN_HEADER: &str = "x-dial-relay-token";
/// The one browser context every relayed page target belongs to.
pub const DEFAULT_CONTEXT_ID: &str = "dial-default";
/// Target id of the browser itself.
pub const BROWSER_TARGET_ID: &str = "dial-browser";

/// A Chrome tab as the extension reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabSnapshot {
    pub tab_id: i64,
    /// Chrome's id for the tab's page target. Playwright requires it to equal the page's frame id.
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub discarded: bool,
}

/// Messages the extension sends to the relay on `/ext`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ExtMessage {
    /// First message on a connection: identifies the extension and lists its tabs.
    Hello {
        dial_protocol: u32,
        #[serde(default)]
        extension_version: String,
        #[serde(default)]
        product: String,
        #[serde(default)]
        user_agent: String,
        #[serde(default)]
        tabs: Vec<TabSnapshot>,
    },
    /// A `chrome.debugger` event. `session_id` is set for events from child sessions.
    CdpEvent {
        tab_id: i64,
        #[serde(default)]
        session_id: Option<String>,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// The debugger detached from a tab (user dismissed the infobar, DevTools opened, tab closed).
    Detached {
        tab_id: i64,
        #[serde(default)]
        reason: String,
    },
    TabCreated {
        tab: TabSnapshot,
    },
    TabUpdated {
        tab: TabSnapshot,
    },
    TabRemoved {
        tab_id: i64,
    },
    /// Answer to an [`RpcFrame`] with the same `id`.
    RpcResult {
        id: u64,
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        error: String,
    },
    Ping,
}

/// Operations the relay asks the extension to perform.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RelayOp {
    /// Attach `chrome.debugger` to a tab. Succeeds when the tab is already attached.
    Attach {
        tab_id: i64,
    },
    Detach {
        tab_id: i64,
    },
    /// Send one CDP command. `session_id` addresses a child session of the tab.
    Send {
        tab_id: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        method: String,
        params: Value,
    },
    CreateTab {
        url: String,
    },
    RemoveTab {
        tab_id: i64,
    },
    ActivateTab {
        tab_id: i64,
    },
}

/// A numbered request frame on `/ext`: `{"t":"rpc","id":N,"op":…,…}`.
#[derive(Debug, Serialize)]
pub struct RpcFrame<'a> {
    pub t: &'static str,
    pub id: u64,
    #[serde(flatten)]
    pub op: &'a RelayOp,
}

impl<'a> RpcFrame<'a> {
    pub fn new(id: u64, op: &'a RelayOp) -> Self {
        Self { t: "rpc", id, op }
    }
}

/// Keepalive answer on `/ext`.
pub fn pong_frame() -> String {
    r#"{"t":"pong"}"#.to_owned()
}

#[cfg(test)]
mod tests {
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
            serde_json::from_str(r#"{"t":"rpcResult","id":9,"ok":true,"result":{"tabId":4}}"#)
                .unwrap();
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
}
