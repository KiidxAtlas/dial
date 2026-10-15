// Contains third-party MIT-licensed code; see THIRD_PARTY_NOTICES.
// Dial Browser Relay: connects this Chrome to the Dial relay on 127.0.0.1 and runs the
// debugger operations Dial asks for. The port and token come from relay-config.js, which
// `dial browser-relay install` writes next to this file.
importScripts("relay-config.js");

const RELAY = self.dialRelayConfig;
const DIAL_PROTOCOL = 1;
const PING_INTERVAL_MS = 20000;
const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 10000;
const KEEPALIVE_ALARM = "dial-relay-keepalive";

let socket = null;
let reconnectDelay = RECONNECT_MIN_MS;
let pingTimer = null;
let lastError = null;

function snapshot(tab, targetId) {
  if (tab.id === undefined || !targetId) return null;
  return {
    tabId: tab.id,
    targetId,
    url: tab.url ?? tab.pendingUrl ?? "",
    title: tab.title ?? "",
    discarded: tab.discarded === true,
  };
}

// Chrome's id for each tab's page target, keyed by tab id. A page's main frame reports this same
// id, and Playwright refuses a page whose target id does not match it, so Dial reports the real id.
async function pageTargetIds() {
  const targets = await chrome.debugger.getTargets();
  const ids = new Map();
  for (const target of targets) {
    if (target.type === "page" && target.tabId !== undefined) ids.set(target.tabId, target.id);
  }
  return ids;
}

// A new tab can take a moment to appear in the debugger's target list.
async function waitForPageTargetId(tabId) {
  const deadline = Date.now() + 2000;
  for (;;) {
    const id = (await pageTargetIds()).get(tabId);
    if (id) return id;
    if (Date.now() > deadline) throw new Error("Chrome did not report a debuggable target for the new tab");
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

function post(message) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message));
  }
}

function productName() {
  const match = /Chrome\/[\d.]+/.exec(navigator.userAgent);
  return match ? match[0] : "Chrome/unknown";
}

async function buildHello() {
  const [tabs, targetIds] = await Promise.all([chrome.tabs.query({}), pageTargetIds()]);
  const snapshots = [];
  for (const tab of tabs) {
    const entry = snapshot(tab, targetIds.get(tab.id));
    if (entry) snapshots.push(entry);
  }
  return {
    t: "hello",
    dialProtocol: DIAL_PROTOCOL,
    extensionVersion: chrome.runtime.getManifest().version,
    product: productName(),
    userAgent: navigator.userAgent,
    tabs: snapshots,
  };
}

async function runOperation(message) {
  switch (message.op) {
    case "attach":
      await chrome.debugger.attach({ tabId: message.tabId }, "1.3");
      return {};
    case "detach":
      await chrome.debugger.detach({ tabId: message.tabId });
      return {};
    case "send":
      return await chrome.debugger.sendCommand(
        message.sessionId
          ? { tabId: message.tabId, sessionId: message.sessionId }
          : { tabId: message.tabId },
        message.method,
        message.params ?? {},
      );
    case "createTab": {
      const tab = await chrome.tabs.create({ url: message.url, active: true });
      if (tab.id === undefined) throw new Error("Chrome did not report an id for the new tab");
      return { tabId: tab.id, targetId: await waitForPageTargetId(tab.id) };
    }
    case "removeTab":
      await chrome.tabs.remove(message.tabId);
      return {};
    case "activateTab": {
      const tab = await chrome.tabs.get(message.tabId);
      await chrome.windows.update(tab.windowId, { focused: true });
      await chrome.tabs.update(message.tabId, { active: true });
      return {};
    }
    default:
      throw new Error(`unknown relay operation: ${message.op}`);
  }
}

function handleRelayFrame(raw) {
  let message;
  try {
    message = JSON.parse(raw);
  } catch {
    return;
  }
  if (message.t === "pong") return;
  if (message.t !== "rpc") return;
  runOperation(message)
    .then((result) => post({ t: "rpcResult", id: message.id, ok: true, result }))
    .catch((error) =>
      post({
        t: "rpcResult",
        id: message.id,
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      }),
    );
}

function setBadge(connected) {
  chrome.action.setBadgeText({ text: connected ? "on" : "off" }).catch(() => {});
  chrome.action.setBadgeBackgroundColor({ color: connected ? "#1a7f37" : "#8b8b8b" }).catch(() => {});
}

function scheduleReconnect() {
  const delay = reconnectDelay;
  reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
  setTimeout(() => connect(), delay);
}

function connect() {
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return;
  }
  const url = `ws://127.0.0.1:${RELAY.port}/ext?token=${encodeURIComponent(RELAY.token)}`;
  const current = new WebSocket(url);
  socket = current;
  current.onopen = () => {
    lastError = null;
    reconnectDelay = RECONNECT_MIN_MS;
    setBadge(true);
    buildHello().then((hello) => post(hello));
    clearInterval(pingTimer);
    pingTimer = setInterval(() => post({ t: "ping" }), PING_INTERVAL_MS);
  };
  current.onmessage = (event) => {
    if (typeof event.data === "string") handleRelayFrame(event.data);
  };
  current.onclose = () => {
    if (socket !== current) return;
    socket = null;
    clearInterval(pingTimer);
    pingTimer = null;
    setBadge(false);
    lastError ??= `the relay on 127.0.0.1:${RELAY.port} is not reachable or refused the connection`;
    scheduleReconnect();
  };
  current.onerror = () => current.close();
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (source.tabId === undefined) return;
  post({ t: "cdpEvent", tabId: source.tabId, sessionId: source.sessionId ?? null, method, params });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  if (source.tabId === undefined) return;
  post({ t: "detached", tabId: source.tabId, reason });
});

chrome.tabs.onCreated.addListener(async (tab) => {
  const entry = snapshot(tab, (await pageTargetIds()).get(tab.id));
  if (entry) post({ t: "tabCreated", tab: entry });
});

chrome.tabs.onUpdated.addListener(async (_tabId, _changeInfo, tab) => {
  const entry = snapshot(tab, (await pageTargetIds()).get(tab.id));
  if (entry) post({ t: "tabUpdated", tab: entry });
});

chrome.tabs.onRemoved.addListener((tabId) => {
  post({ t: "tabRemoved", tabId });
});

chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === KEEPALIVE_ALARM) connect();
});

// The popup asks for the status and can force a reconnect.
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message && message.t === "status") {
    sendResponse({
      connected: !!socket && socket.readyState === WebSocket.OPEN,
      port: RELAY.port,
      lastError,
    });
    return false;
  }
  if (message && message.t === "reconnect") {
    connect();
    sendResponse({ ok: true });
    return false;
  }
  return false;
});

chrome.runtime.onInstalled.addListener(() => connect());
chrome.runtime.onStartup.addListener(() => connect());
connect();
