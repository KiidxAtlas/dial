//! Relay state: one extension link, any number of CDP clients, and the sessions between them.
//!
//! The bridge does no socket I/O. Server threads call into it; it answers CDP requests, forwards
//! extension events to clients, and sends extension operations through channels. Every method
//! that needs an extension answer releases the lock while it waits, so the extension reader thread
//! can deliver that answer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::outbox::Outbox;
use super::protocol::{
    BROWSER_TARGET_ID, DEFAULT_CONTEXT_ID, ExtMessage, PROTOCOL_VERSION, RelayOp, RpcFrame,
    TabSnapshot, pong_frame,
};

/// How long one relay-to-extension operation may take before the CDP caller gets an error.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const NOT_CONNECTED: &str = "the Dial browser extension is not connected; run `dial browser-relay install`, load the printed folder in chrome://extensions, and keep Chrome open";
/// Extension requests that may await answers at once. Further CDP calls are refused, not queued.
pub const MAX_PENDING_RPCS: usize = 256;
const EXTENSION_BUSY: &str = "the Dial browser extension is busy or not keeping up; retry shortly";
/// URL prefixes that `chrome.debugger` refuses or that are not user content.
const INTERNAL_PREFIXES: [&str; 6] = [
    "chrome://",
    "chrome-extension://",
    "chrome-untrusted://",
    "devtools://",
    "edge://",
    "view-source:",
];

/// A CDP error response body: `{code, message}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpError {
    pub code: i64,
    pub message: String,
}

impl CdpError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }

    fn with_code(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub type CdpResult = Result<Value, CdpError>;

/// Identity the extension reported in its hello.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionInfo {
    pub version: String,
    pub product: String,
    pub user_agent: String,
}

/// Point-in-time view for `/json/*` answers and `dial browser-relay status`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub connected: bool,
    pub extension_seen: bool,
    pub uptime: Duration,
    /// Time since the extension last went away; `None` while connected or before any connection.
    pub disconnected_for: Option<Duration>,
    pub info: ExtensionInfo,
    pub targets: Vec<Value>,
    /// Most recent background auto-attach failure, cleared when a tab attaches.
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionKind {
    /// Session to the browser target itself.
    Browser,
    /// Session to a tab. `child` is the Chrome session id for a child session, `None` for the
    /// top-level session the relay minted.
    Page { tab: i64, child: Option<String> },
}

#[derive(Debug, Clone)]
struct Session {
    owner: u64,
    kind: SessionKind,
}

struct Link {
    id: u64,
    tx: Outbox,
}

struct Client {
    tx: Outbox,
    discover: bool,
    auto_attach: bool,
}

struct State {
    started: Instant,
    next_id: u64,
    link: Option<Link>,
    extension_seen: bool,
    disconnected_at: Option<Instant>,
    info: ExtensionInfo,
    tabs: BTreeMap<i64, TabSnapshot>,
    pending: HashMap<u64, Sender<Result<Value, String>>>,
    clients: HashMap<u64, Client>,
    sessions: HashMap<String, Session>,
    /// Number of top-level page sessions per tab. The debugger stays attached while it is nonzero.
    attached: HashMap<i64, usize>,
    /// (client, tab) pairs with a background auto-attach in flight.
    attaching: HashSet<(u64, i64)>,
    /// Tabs whose first attach is in flight. Concurrent callers wait for it instead of attaching again.
    attach_inflight: HashSet<i64>,
    last_error: Option<String>,
}

/// The relay's shared state. Wrap in `Arc` to share across server threads.
pub struct Relay {
    state: Mutex<State>,
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

impl Relay {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                started: Instant::now(),
                next_id: 0,
                link: None,
                extension_seen: false,
                disconnected_at: None,
                info: ExtensionInfo::default(),
                tabs: BTreeMap::new(),
                pending: HashMap::new(),
                clients: HashMap::new(),
                sessions: HashMap::new(),
                attached: HashMap::new(),
                attaching: HashSet::new(),
                attach_inflight: HashSet::new(),
                last_error: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.lock();
        let now = Instant::now();
        let connected = st.link.is_some();
        let targets = if connected {
            st.tabs
                .values()
                .filter(|tab| eligible(tab))
                .map(|tab| target_info(tab, is_attached(&st, tab.tab_id)))
                .collect()
        } else {
            Vec::new()
        };
        Snapshot {
            connected,
            extension_seen: st.extension_seen,
            uptime: now.duration_since(st.started),
            disconnected_for: st.disconnected_at.map(|at| now.duration_since(at)),
            info: st.info.clone(),
            targets,
            last_error: st.last_error.clone(),
        }
    }

    /// Registers an extension connection from its hello. Replaces any earlier connection and
    /// returns the link id the server uses for the rest of that connection.
    pub fn extension_hello(&self, tx: Outbox, hello: ExtMessage) -> Result<u64, String> {
        let ExtMessage::Hello {
            dial_protocol,
            extension_version,
            product,
            user_agent,
            tabs,
        } = hello
        else {
            return Err("the first extension message must be hello".to_owned());
        };
        if dial_protocol != PROTOCOL_VERSION {
            return Err(format!(
                "extension speaks protocol {dial_protocol}, this Dial speaks {PROTOCOL_VERSION}; run `dial browser-relay install` and reload the extension"
            ));
        }
        let mut st = self.lock();
        clear_extension(&mut st);
        st.next_id += 1;
        let id = st.next_id;
        st.link = Some(Link { id, tx });
        st.extension_seen = true;
        st.disconnected_at = None;
        st.info = ExtensionInfo {
            version: extension_version,
            product,
            user_agent,
        };
        st.tabs = tabs.into_iter().map(|tab| (tab.tab_id, tab)).collect();
        let created: Vec<Value> = st
            .tabs
            .values()
            .filter(|tab| eligible(tab))
            .map(|tab| target_info(tab, false))
            .collect();
        for info in created {
            broadcast_discover(&st, "Target.targetCreated", json!({"targetInfo": info}));
        }
        Ok(id)
    }

    /// Forgets the extension when its connection `link` ends. A stale link is ignored.
    pub fn extension_disconnected(&self, link: u64) {
        let mut st = self.lock();
        if st.link.as_ref().map(|current| current.id) != Some(link) {
            return;
        }
        clear_extension(&mut st);
        st.disconnected_at = Some(Instant::now());
    }

    /// Handles one message from the extension connection `link`. Returns the tabs that now need
    /// a background auto-attach; the caller runs [`Relay::auto_attach_tabs`] for them off the
    /// extension reader thread.
    pub fn extension_message(&self, link: u64, message: ExtMessage) -> Vec<i64> {
        let mut st = self.lock();
        if st.link.as_ref().map(|current| current.id) != Some(link) {
            return Vec::new();
        }
        match message {
            ExtMessage::Hello { .. } => {}
            ExtMessage::Ping => {
                if let Some(current) = st.link.as_ref() {
                    let _ = current.tx.send(pong_frame());
                }
            }
            ExtMessage::RpcResult {
                id,
                ok,
                result,
                error,
            } => {
                if let Some(reply) = st.pending.remove(&id) {
                    let _ = reply.send(if ok { Ok(result) } else { Err(error) });
                }
            }
            ExtMessage::CdpEvent {
                tab_id,
                session_id,
                method,
                params,
            } => route_event(&mut st, tab_id, session_id.as_deref(), &method, params),
            ExtMessage::Detached { tab_id, .. } => close_tab(&mut st, tab_id),
            ExtMessage::TabRemoved { tab_id } => {
                close_tab(&mut st, tab_id);
                if let Some(tab) = st.tabs.remove(&tab_id).filter(eligible) {
                    broadcast_discover(
                        &st,
                        "Target.targetDestroyed",
                        json!({"targetId": tab.target_id}),
                    );
                }
            }
            ExtMessage::TabCreated { tab } | ExtMessage::TabUpdated { tab } => {
                return upsert_tab(&mut st, tab);
            }
        }
        Vec::new()
    }

    /// Registers a CDP client connection and returns its id. `tx` receives every frame for it.
    pub fn connect_client(&self, tx: Outbox) -> u64 {
        let mut st = self.lock();
        st.next_id += 1;
        let id = st.next_id;
        st.clients.insert(
            id,
            Client {
                tx,
                discover: false,
                auto_attach: false,
            },
        );
        id
    }

    /// Drops a client. Its sessions end, and tabs nobody else uses are detached from Chrome.
    pub fn disconnect_client(&self, client: u64) {
        let mut st = self.lock();
        st.clients.remove(&client);
        st.attaching.retain(|(owner, _)| *owner != client);
        let keys: Vec<String> = st
            .sessions
            .iter()
            .filter(|(_, session)| session.owner == client)
            .map(|(key, _)| key.clone())
            .collect();
        let mut owned: Vec<Session> = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(session) = st.sessions.remove(&key) {
                owned.push(session);
            }
        }
        // Child sessions first: their top-level parent may be released below.
        for session in &owned {
            if let SessionKind::Page {
                tab,
                child: Some(child),
            } = &session.kind
            {
                fire(
                    &mut st,
                    RelayOp::Send {
                        tab_id: *tab,
                        session_id: None,
                        method: "Target.detachFromTarget".to_owned(),
                        params: json!({"sessionId": child}),
                    },
                );
            }
        }
        for session in &owned {
            if let SessionKind::Page { tab, child: None } = session.kind {
                release(&mut st, tab);
            }
        }
    }

    /// Answers one CDP request from `client`. Returns the response text, or `None` for a message
    /// without an `id` (CDP notifications need no answer).
    pub fn handle_cdp(&self, client: u64, request: &Value) -> Option<String> {
        let id = request.get("id")?.clone();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let session = request.get("sessionId").and_then(Value::as_str);
        let result = match session {
            Some(session) => self.session_command(client, session, method, &params),
            None => self.browser_command(client, method, &params),
        };
        let mut response = match result {
            Ok(value) => json!({"id": id, "result": value}),
            Err(error) => json!({
                "id": id,
                "error": {"code": error.code, "message": error.message},
            }),
        };
        if let Some(session) = session {
            response["sessionId"] = json!(session);
        }
        Some(response.to_string())
    }

    /// Attaches every eligible tab (or only `tabs`) for every client that asked for auto-attach.
    /// Runs the extension operations, so call it off the extension reader thread.
    pub fn auto_attach_tabs(&self, tabs: Option<Vec<i64>>) {
        let candidates = self.attach_candidates(None, tabs);
        self.run_auto_attach(candidates);
    }

    fn browser_command(&self, client: u64, method: &str, params: &Value) -> CdpResult {
        match method {
            "Browser.getVersion" => Ok(self.version()),
            // Downloads stay under the user's own Chrome settings; Playwright only sets them here.
            "Browser.setDownloadBehavior" | "Browser.close" => Ok(json!({})),
            "Target.getTargetInfo" => self.target_info_for(params.get("targetId")),
            "Target.getTargets" => Ok(json!({"targetInfos": self.page_targets()})),
            "Target.setDiscoverTargets" => {
                let discover = params["discover"].as_bool().unwrap_or(false);
                self.set_discover(client, discover);
                Ok(json!({}))
            }
            "Target.setAutoAttach" => {
                let enabled = params["autoAttach"].as_bool().unwrap_or(false);
                self.set_auto_attach(client, enabled);
                if enabled {
                    let candidates = self.attach_candidates(Some(client), None);
                    self.run_auto_attach(candidates);
                }
                Ok(json!({}))
            }
            "Target.attachToTarget" => {
                let tab = target_tab(&self.lock(), params)?;
                let session = self.open_page_session(client, tab)?;
                Ok(json!({"sessionId": session}))
            }
            "Target.attachToBrowserTarget" => {
                let session = self.open_browser_session(client);
                Ok(json!({"sessionId": session}))
            }
            "Target.detachFromTarget" => {
                let session = params["sessionId"]
                    .as_str()
                    .ok_or_else(|| CdpError::with_code(-32602, "sessionId is required"))?;
                self.detach_session(client, session)
            }
            "Target.createTarget" => {
                self.create_target(client, params["url"].as_str().unwrap_or("about:blank"))
            }
            "Target.closeTarget" => {
                let tab = target_tab(&self.lock(), params)?;
                self.rpc(RelayOp::RemoveTab { tab_id: tab })?;
                Ok(json!({"success": true}))
            }
            "Target.activateTarget" => {
                let tab = target_tab(&self.lock(), params)?;
                self.rpc(RelayOp::ActivateTab { tab_id: tab })?;
                Ok(json!({}))
            }
            "Target.getBrowserContexts" => Ok(json!({
                "browserContextIds": [DEFAULT_CONTEXT_ID],
                "defaultBrowserContextId": DEFAULT_CONTEXT_ID,
            })),
            "Target.createBrowserContext" | "Target.disposeBrowserContext" => Err(CdpError::new(
                "the Dial browser relay drives your existing Chrome profile and cannot create or dispose browser contexts; use the default context",
            )),
            _ => Err(CdpError::with_code(
                -32601,
                format!("the Dial browser relay does not implement {method}"),
            )),
        }
    }

    fn session_command(
        &self,
        client: u64,
        session: &str,
        method: &str,
        params: &Value,
    ) -> CdpResult {
        let kind = {
            let st = self.lock();
            match st.sessions.get(session) {
                Some(found) if found.owner == client => found.kind.clone(),
                _ => return Err(session_not_found()),
            }
        };
        match kind {
            SessionKind::Browser => self.browser_command(client, method, params),
            SessionKind::Page { tab, child } => self.rpc(RelayOp::Send {
                tab_id: tab,
                session_id: child,
                method: method.to_owned(),
                params: params.clone(),
            }),
        }
    }

    fn version(&self) -> Value {
        let st = self.lock();
        let product = if st.info.product.is_empty() {
            "Chrome/0.0.0".to_owned()
        } else {
            st.info.product.clone()
        };
        json!({
            "protocolVersion": "1.3",
            "product": product,
            "revision": "dial-relay",
            "userAgent": st.info.user_agent,
            "jsVersion": "",
        })
    }

    fn target_info_for(&self, target_id: Option<&Value>) -> CdpResult {
        let st = self.lock();
        let Some(target_id) = target_id.and_then(Value::as_str) else {
            return Ok(json!({"targetInfo": browser_target_info()}));
        };
        if target_id == BROWSER_TARGET_ID {
            return Ok(json!({"targetInfo": browser_target_info()}));
        }
        let tab = st
            .tabs
            .values()
            .find(|tab| tab.target_id == target_id)
            .ok_or_else(no_such_target)?;
        Ok(json!({"targetInfo": target_info(tab, is_attached(&st, tab.tab_id))}))
    }

    fn page_targets(&self) -> Vec<Value> {
        let st = self.lock();
        st.tabs
            .values()
            .filter(|tab| eligible(tab))
            .map(|tab| target_info(tab, is_attached(&st, tab.tab_id)))
            .collect()
    }

    fn set_discover(&self, client: u64, discover: bool) {
        let mut st = self.lock();
        let Some(entry) = st.clients.get_mut(&client) else {
            return;
        };
        entry.discover = discover;
        if !discover {
            return;
        }
        let created: Vec<Value> = st
            .tabs
            .values()
            .filter(|tab| eligible(tab))
            .map(|tab| target_info(tab, is_attached(&st, tab.tab_id)))
            .collect();
        for info in created {
            notify(
                &st,
                client,
                None,
                "Target.targetCreated",
                json!({"targetInfo": info}),
            );
        }
    }

    fn set_auto_attach(&self, client: u64, enabled: bool) {
        if let Some(entry) = self.lock().clients.get_mut(&client) {
            entry.auto_attach = enabled;
        }
    }

    fn open_browser_session(&self, client: u64) -> String {
        let mut st = self.lock();
        let key = next_session_key(&mut st, "dial-browser-session-");
        st.sessions.insert(
            key.clone(),
            Session {
                owner: client,
                kind: SessionKind::Browser,
            },
        );
        key
    }

    /// Opens a top-level page session for `client`, attaching the debugger if nothing has it yet.
    ///
    /// Concurrent first attachments to one tab share a single extension attach: the first caller
    /// reserves the tab and the others wait for its answer. The session commits only while this
    /// client and the extension connection that answered are still live. A successful attach that
    /// cannot be committed is released, so no debugger stays attached without an owner.
    fn open_page_session(&self, client: u64, tab: i64) -> Result<String, CdpError> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        let (link, reserved) = loop {
            let mut st = self.lock();
            if !st.clients.contains_key(&client) {
                return Err(client_gone());
            }
            let snapshot = st.tabs.get(&tab).ok_or_else(no_such_target)?;
            if !eligible(snapshot) {
                return Err(CdpError::new(format!(
                    "tab {tab} cannot be attached: it is discarded or an internal page"
                )));
            }
            let link = st.link.as_ref().map(|current| current.id);
            if is_attached(&st, tab) {
                break (link, false);
            }
            if st.attach_inflight.insert(tab) {
                break (link, true);
            }
            drop(st);
            if Instant::now() >= deadline {
                return Err(CdpError::new(format!(
                    "could not attach to tab {tab}: another attach is still in progress"
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let attach = if reserved {
            let answer = self.rpc(RelayOp::Attach { tab_id: tab });
            self.lock().attach_inflight.remove(&tab);
            Some(answer)
        } else {
            None
        };
        let mut st = self.lock();
        let attached_now = match attach {
            Some(Ok(_)) => true,
            Some(Err(error)) => {
                return Err(CdpError::new(format!(
                    "could not attach to tab {tab}: {}",
                    error.message
                )));
            }
            None => false,
        };
        let live = st.clients.contains_key(&client)
            && st.link.as_ref().map(|current| current.id) == link
            && st.tabs.contains_key(&tab);
        if !live {
            // Release an attach this request made when nothing else holds the debugger.
            if attached_now && st.link.is_some() && !is_attached(&st, tab) {
                fire(&mut st, RelayOp::Detach { tab_id: tab });
            }
            return Err(if st.clients.contains_key(&client) {
                no_such_target()
            } else {
                client_gone()
            });
        }
        *st.attached.entry(tab).or_insert(0) += 1;
        let key = next_session_key(&mut st, "dial-session-");
        st.sessions.insert(
            key.clone(),
            Session {
                owner: client,
                kind: SessionKind::Page { tab, child: None },
            },
        );
        Ok(key)
    }

    fn detach_session(&self, client: u64, session: &str) -> CdpResult {
        let mut st = self.lock();
        match st.sessions.get(session) {
            Some(found) if found.owner == client => {}
            _ => return Err(session_not_found()),
        }
        let Some(removed) = st.sessions.remove(session) else {
            return Err(session_not_found());
        };
        match removed.kind {
            SessionKind::Browser => {}
            SessionKind::Page { tab, child: None } => release(&mut st, tab),
            SessionKind::Page {
                tab,
                child: Some(child),
            } => fire(
                &mut st,
                RelayOp::Send {
                    tab_id: tab,
                    session_id: None,
                    method: "Target.detachFromTarget".to_owned(),
                    params: json!({"sessionId": child}),
                },
            ),
        }
        Ok(json!({}))
    }

    fn create_target(&self, client: u64, url: &str) -> CdpResult {
        let result = self.rpc(RelayOp::CreateTab {
            url: url.to_owned(),
        })?;
        let tab = result["tabId"]
            .as_i64()
            .ok_or_else(|| CdpError::new("the extension did not report the new tab id"))?;
        let target_id = result["targetId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CdpError::new("the extension did not report the new tab's target id"))?
            .to_owned();
        {
            let mut st = self.lock();
            st.tabs.entry(tab).or_insert_with(|| TabSnapshot {
                tab_id: tab,
                target_id: target_id.clone(),
                url: url.to_owned(),
                title: String::new(),
                discarded: false,
            });
        }
        let candidates = self.attach_candidates(Some(client), Some(vec![tab]));
        self.run_auto_attach(candidates);
        self.wait_for_attach(client, tab);
        Ok(json!({"targetId": target_id}))
    }

    /// `(client, tab)` pairs that need an auto-attach now. Marks each one as in flight.
    fn attach_candidates(
        &self,
        only_client: Option<u64>,
        only_tabs: Option<Vec<i64>>,
    ) -> Vec<(u64, i64)> {
        let mut st = self.lock();
        let clients: Vec<u64> = st
            .clients
            .iter()
            .filter(|(id, client)| {
                client.auto_attach && only_client.is_none_or(|only| only == **id)
            })
            .map(|(id, _)| *id)
            .collect();
        let tabs: Vec<i64> = st
            .tabs
            .values()
            .filter(|tab| eligible(tab))
            .filter(|tab| {
                only_tabs
                    .as_ref()
                    .is_none_or(|only| only.contains(&tab.tab_id))
            })
            .map(|tab| tab.tab_id)
            .collect();
        let mut out = Vec::new();
        for client in clients {
            for &tab in &tabs {
                let key = (client, tab);
                if has_top_session(&st, client, tab) || st.attaching.contains(&key) {
                    continue;
                }
                st.attaching.insert(key);
                out.push(key);
            }
        }
        out
    }

    fn run_auto_attach(&self, candidates: Vec<(u64, i64)>) {
        for (client, tab) in candidates {
            let result = self.open_page_session(client, tab);
            let mut st = self.lock();
            st.attaching.remove(&(client, tab));
            match result {
                Ok(session) => {
                    st.last_error = None;
                    if let Some(info) = st
                        .tabs
                        .get(&tab)
                        .map(|snapshot| target_info(snapshot, true))
                    {
                        notify(
                            &st,
                            client,
                            None,
                            "Target.attachedToTarget",
                            json!({
                                "sessionId": session,
                                "targetInfo": info,
                                "waitingForDebugger": false,
                            }),
                        );
                    }
                }
                Err(error) => {
                    // A client that left mid-attach has no status to report to.
                    if st.clients.contains_key(&client) {
                        st.last_error = Some(error.message);
                    }
                }
            }
        }
    }

    /// Waits until an attach for `tab` that another thread started has queued its event, so a
    /// `createTarget` reply never overtakes the `attachedToTarget` event it causes.
    fn wait_for_attach(&self, client: u64, tab: i64) {
        let deadline = Instant::now() + RPC_TIMEOUT;
        while self.lock().attaching.contains(&(client, tab)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Sends one request to the extension and waits for its answer.
    fn rpc(&self, op: RelayOp) -> CdpResult {
        let (id, rx) = {
            let mut st = self.lock();
            queue_rpc(&mut st, &op)?
        };
        match rx.recv_timeout(RPC_TIMEOUT) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(CdpError::new(message)),
            Err(RecvTimeoutError::Disconnected) => {
                Err(CdpError::new("the Dial browser extension disconnected"))
            }
            Err(RecvTimeoutError::Timeout) => {
                self.lock().pending.remove(&id);
                Err(CdpError::new(format!(
                    "the Dial browser extension did not answer within {} seconds",
                    RPC_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

fn is_attached(st: &State, tab: i64) -> bool {
    st.attached.get(&tab).copied().unwrap_or(0) > 0
}

fn has_top_session(st: &State, client: u64, tab: i64) -> bool {
    st.sessions.values().any(|session| {
        session.owner == client
            && matches!(session.kind, SessionKind::Page { tab: t, child: None } if t == tab)
    })
}

/// Whether Playwright-visible page targets may include this tab. Discarded tabs are excluded
/// because attaching would reload them; internal pages cannot be debugged.
pub fn eligible(tab: &TabSnapshot) -> bool {
    !tab.discarded
        && !tab.target_id.is_empty()
        && !INTERNAL_PREFIXES
            .iter()
            .any(|prefix| tab.url.starts_with(prefix))
}

fn target_info(tab: &TabSnapshot, attached: bool) -> Value {
    let url = if tab.url.is_empty() {
        "about:blank"
    } else {
        tab.url.as_str()
    };
    let title = if tab.title.is_empty() {
        url
    } else {
        tab.title.as_str()
    };
    json!({
        "targetId": tab.target_id,
        "type": "page",
        "title": title,
        "url": url,
        "attached": attached,
        "canAccessOpener": false,
        "browserContextId": DEFAULT_CONTEXT_ID,
    })
}

fn browser_target_info() -> Value {
    json!({
        "targetId": BROWSER_TARGET_ID,
        "type": "browser",
        "title": "Dial browser relay",
        "url": "",
        "attached": true,
        "canAccessOpener": false,
    })
}

fn target_tab(st: &State, params: &Value) -> Result<i64, CdpError> {
    let target_id = params["targetId"].as_str().ok_or_else(no_such_target)?;
    st.tabs
        .values()
        .find(|tab| tab.target_id == target_id)
        .map(|tab| tab.tab_id)
        .ok_or_else(no_such_target)
}

/// Chrome's target id for `tab`, or an empty string when the tab is unknown.
fn target_id_of(st: &State, tab: i64) -> String {
    st.tabs
        .get(&tab)
        .map(|tab| tab.target_id.clone())
        .unwrap_or_default()
}

fn no_such_target() -> CdpError {
    CdpError::with_code(-32602, "No target with given id found")
}

fn session_not_found() -> CdpError {
    CdpError::with_code(-32001, "Session with given id not found.")
}

fn client_gone() -> CdpError {
    CdpError::new("the CDP client disconnected before its request completed")
}

fn next_session_key(st: &mut State, prefix: &str) -> String {
    st.next_id += 1;
    format!("{prefix}{}", st.next_id)
}

/// Writes one event to `client`, optionally tagged with a session id.
fn notify(st: &State, client: u64, session: Option<&str>, method: &str, params: Value) {
    let Some(entry) = st.clients.get(&client) else {
        return;
    };
    let mut frame = json!({"method": method, "params": params});
    if let Some(session) = session {
        frame["sessionId"] = json!(session);
    }
    let _ = entry.tx.send(frame.to_string());
}

fn broadcast_discover(st: &State, method: &str, params: Value) {
    for (id, client) in &st.clients {
        if client.discover {
            notify(st, *id, None, method, params.clone());
        }
    }
}

/// Top-level page sessions for `tab`, sorted by key so routing is deterministic.
fn top_sessions(st: &State, tab: i64) -> Vec<(String, u64)> {
    let mut found: Vec<(String, u64)> = st
        .sessions
        .iter()
        .filter(|(_, session)| {
            matches!(session.kind, SessionKind::Page { tab: t, child: None } if t == tab)
        })
        .map(|(key, session)| (key.clone(), session.owner))
        .collect();
    found.sort();
    found
}

fn route_event(st: &mut State, tab: i64, session: Option<&str>, method: &str, params: Value) {
    match session {
        Some(session) => {
            if let Some(owner) = st.sessions.get(session).map(|found| found.owner) {
                notify(st, owner, Some(session), method, params);
            }
        }
        None => {
            if method == "Target.detachedFromTarget"
                && let Some(child) = params["sessionId"].as_str()
            {
                st.sessions.remove(child);
            }
            let owners = top_sessions(st, tab);
            if method == "Target.attachedToTarget"
                && let (Some(child), Some((_, owner))) =
                    (params["sessionId"].as_str(), owners.first())
            {
                st.sessions.insert(
                    child.to_owned(),
                    Session {
                        owner: *owner,
                        kind: SessionKind::Page {
                            tab,
                            child: Some(child.to_owned()),
                        },
                    },
                );
            }
            for (key, owner) in owners {
                notify(st, owner, Some(&key), method, params.clone());
            }
        }
    }
}

/// Ends every session on `tab` and forgets its attachment count.
fn close_tab(st: &mut State, tab: i64) {
    let keys: Vec<String> = st
        .sessions
        .iter()
        .filter(|(_, session)| matches!(session.kind, SessionKind::Page { tab: t, .. } if t == tab))
        .map(|(key, _)| key.clone())
        .collect();
    let target_id = target_id_of(st, tab);
    for key in keys {
        if let Some(session) = st.sessions.remove(&key) {
            notify(
                st,
                session.owner,
                None,
                "Target.detachedFromTarget",
                json!({"sessionId": key, "targetId": target_id}),
            );
        }
    }
    st.attached.remove(&tab);
    st.attaching.retain(|(_, owned)| *owned != tab);
}

/// Records a created or updated tab and reports the tabs that need an auto-attach.
fn upsert_tab(st: &mut State, tab: TabSnapshot) -> Vec<i64> {
    let id = tab.tab_id;
    let was = st.tabs.get(&id).is_some_and(eligible);
    let now = eligible(&tab);
    st.tabs.insert(id, tab);
    let snapshot = st.tabs[&id].clone();
    match (was, now) {
        (false, true) => broadcast_discover(
            st,
            "Target.targetCreated",
            json!({"targetInfo": target_info(&snapshot, is_attached(st, id))}),
        ),
        (true, true) => broadcast_discover(
            st,
            "Target.targetInfoChanged",
            json!({"targetInfo": target_info(&snapshot, is_attached(st, id))}),
        ),
        (true, false) => {
            close_tab(st, id);
            broadcast_discover(
                st,
                "Target.targetDestroyed",
                json!({"targetId": snapshot.target_id}),
            );
        }
        (false, false) => {}
    }
    let auto = st.clients.values().any(|client| client.auto_attach);
    if now && auto { vec![id] } else { Vec::new() }
}

/// Drops the extension connection and everything that depended on it.
fn clear_extension(st: &mut State) {
    st.link = None;
    for (_, reply) in st.pending.drain() {
        let _ = reply.send(Err("the Dial browser extension disconnected".to_owned()));
    }
    let sessions: Vec<(String, Session)> = st.sessions.drain().collect();
    for (key, session) in sessions {
        let target = match session.kind {
            SessionKind::Browser => BROWSER_TARGET_ID.to_owned(),
            SessionKind::Page { tab, .. } => target_id_of(st, tab),
        };
        notify(
            st,
            session.owner,
            None,
            "Target.detachedFromTarget",
            json!({"sessionId": key, "targetId": target}),
        );
    }
    st.attached.clear();
    st.attaching.clear();
    let old: Vec<TabSnapshot> = std::mem::take(&mut st.tabs).into_values().collect();
    for tab in old.iter().filter(|tab| eligible(tab)) {
        broadcast_discover(
            st,
            "Target.targetDestroyed",
            json!({"targetId": tab.target_id}),
        );
    }
}

/// Queues one request to the extension and returns the channel its answer arrives on.
fn queue_rpc(
    st: &mut State,
    op: &RelayOp,
) -> Result<(u64, Receiver<Result<Value, String>>), CdpError> {
    let link_tx = match st.link.as_ref() {
        Some(link) => link.tx.clone(),
        None => return Err(CdpError::new(NOT_CONNECTED)),
    };
    if st.pending.len() >= MAX_PENDING_RPCS {
        return Err(CdpError::new(EXTENSION_BUSY));
    }
    st.next_id += 1;
    let id = st.next_id;
    let frame = serde_json::to_string(&RpcFrame::new(id, op))
        .map_err(|error| CdpError::new(format!("could not encode extension request: {error}")))?;
    let (reply, rx) = mpsc::channel();
    st.pending.insert(id, reply);
    if link_tx.send(frame).is_err() {
        st.pending.remove(&id);
        return Err(CdpError::new(if link_tx.is_overloaded() {
            EXTENSION_BUSY
        } else {
            NOT_CONNECTED
        }));
    }
    Ok((id, rx))
}

/// Sends an operation without waiting for its answer.
fn fire(st: &mut State, op: RelayOp) {
    let _ = queue_rpc(st, &op);
}

/// Drops one top-level session on `tab`. The last one detaches the debugger and ends any child
/// sessions on that tab.
fn release(st: &mut State, tab: i64) {
    let remaining = match st.attached.get_mut(&tab) {
        Some(count) => {
            *count = count.saturating_sub(1);
            *count
        }
        None => return,
    };
    if remaining > 0 {
        return;
    }
    st.attached.remove(&tab);
    let children: Vec<String> = st
        .sessions
        .iter()
        .filter(|(_, session)| {
            matches!(session.kind, SessionKind::Page { tab: t, child: Some(_) } if t == tab)
        })
        .map(|(key, _)| key.clone())
        .collect();
    let target_id = target_id_of(st, tab);
    for key in children {
        if let Some(session) = st.sessions.remove(&key) {
            notify(
                st,
                session.owner,
                None,
                "Target.detachedFromTarget",
                json!({"sessionId": key, "targetId": target_id}),
            );
        }
    }
    fire(st, RelayOp::Detach { tab_id: tab });
}

#[cfg(test)]
mod tests;
