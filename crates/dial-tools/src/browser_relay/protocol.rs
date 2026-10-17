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
mod tests;
