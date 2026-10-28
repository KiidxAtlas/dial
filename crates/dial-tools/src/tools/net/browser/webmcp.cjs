'use strict';
// WebMCP page bridge. Playwright serializes these functions with toString(), so each must be self-contained and
// must not reference module-level bindings.
const WEBMCP_KEY = '__dialWebMcpBridge_v1';

// Runs as a document-start init script in every frame. Mirrors navigator.modelContext registrations into a
// bounded catalog. The native getter is never called; it can create a context as a side effect.
function installHook(key) {
  if (globalThis[key]) return;
  const NAME = /^[A-Za-z0-9_.-]{1,128}$/;
  const EVENTS_MAX = 256;
  const tools = new Map();
  const events = [];
  let sequence = 0;
  let dropped = 0;
  const record = (type, name) => {
    sequence += 1;
    events.push({ sequence, type, name });
    if (events.length > EVENTS_MAX) dropped += events.splice(0, events.length - EVENTS_MAX).length;
  };
  const remember = (tool, signal) => {
    if (!tool || typeof tool.name !== 'string' || !NAME.test(tool.name)) {
      throw new TypeError("WebMCP tool name must contain 1-128 ASCII letters, digits, '_', '-', or '.'");
    }
    const execute = typeof tool.execute === 'function' ? tool.execute : tool.handler;
    if (typeof execute !== 'function') throw new TypeError('WebMCP tool requires an execute function');
    const kind = tools.has(tool.name) ? 'updated' : 'registered';
    tools.set(tool.name, { name: tool.name, description: String(tool.description ?? ''), schema: tool.inputSchema ?? null, execute });
    record(kind, tool.name);
    if (signal && typeof signal.addEventListener === 'function') {
      signal.addEventListener('abort', () => {
        if (tools.delete(tool.name)) record('unregistered', tool.name);
      }, { once: true });
    }
  };
  const forget = name => {
    if (tools.delete(String(name))) record('unregistered', String(name));
  };
  const nativeProto = globalThis.ModelContext && globalThis.ModelContext.prototype;
  let nativeDescriptor = null;
  for (let proto = globalThis.Navigator && globalThis.Navigator.prototype; proto && !nativeDescriptor; proto = Object.getPrototypeOf(proto)) {
    nativeDescriptor = Object.getOwnPropertyDescriptor(proto, 'modelContext') || null;
  }
  const nativeAvailable = Boolean(nativeProto || nativeDescriptor);
  if (nativeProto) {
    const register = nativeProto.registerTool;
    const unregister = nativeProto.unregisterTool;
    if (typeof register === 'function') {
      Object.defineProperty(nativeProto, 'registerTool', { configurable: true, writable: true, value: async function registerTool(tool, options) {
        const out = await register.call(this, tool, options);
        remember(tool, options && options.signal);
        return out;
      } });
    }
    if (typeof unregister === 'function') {
      Object.defineProperty(nativeProto, 'unregisterTool', { configurable: true, writable: true, value: async function unregisterTool(name) {
        const out = await unregister.call(this, name);
        forget(name);
        return out;
      } });
    }
  } else if (!nativeDescriptor) {
    const polyfill = {
      async registerTool(tool, options) { remember(tool, options && options.signal); },
      async unregisterTool(name) { forget(name); },
    };
    Object.defineProperty(globalThis.navigator, 'modelContext', { configurable: true, enumerable: false, value: polyfill });
  }
  const bridge = {
    nativeAvailable,
    snapshot(name) {
      return [...tools.values()]
        .filter(tool => name === null || tool.name === name)
        .map(tool => {
          let schema = '';
          try { schema = tool.schema === null ? '' : JSON.stringify(tool.schema); } catch (_) { schema = ''; }
          return {
            name: tool.name,
            description: tool.description.slice(0, 1024),
            input_schema_json: schema.slice(0, 64 * 1024),
          };
        });
    },
    async invoke(name, params) {
      const tool = tools.get(name);
      if (!tool) throw new Error(`No page-side WebMCP tool named "${name}"`);
      return tool.execute(params, { signal: new AbortController().signal });
    },
    events(since, clear) {
      const selected = events.filter(event => event.sequence > since);
      const cursor = sequence;
      if (clear) events.length = 0;
      return { events: selected, cursor, truncated: dropped > 0 && since < dropped };
    },
  };
  Object.defineProperty(globalThis, key, { configurable: true, enumerable: false, value: bridge });
}

// The four functions below run inside page.evaluate. They read the bridge installed by installHook.
function listInPage({ key, name }) {
  const bridge = globalThis[key];
  if (!bridge) {
    return { status: 'unavailable', detail: 'WebMCP is unavailable on this page: the hook was not installed before it loaded.', tools: [] };
  }
  return { status: 'ready', name, tools: bridge.snapshot(name) };
}

async function invokeInPage({ key, name, params }) {
  const bridge = globalThis[key];
  if (!bridge) return { ok: false, error: 'WebMCP is unavailable on this page: the hook was not installed before it loaded.' };
  try {
    const value = await bridge.invoke(name, params);
    const encoded = JSON.stringify(value === undefined ? null : value);
    const text = encoded === undefined ? 'null' : encoded;
    const max = 64 * 1024;
    return { ok: true, result_json: text.slice(0, max), truncated: text.length > max };
  } catch (error) {
    return { ok: false, error: String((error && error.message) || error).slice(0, 2048) };
  }
}

function eventsInPage({ key, since, clear }) {
  const bridge = globalThis[key];
  if (!bridge) return { events: [], cursor: 0, truncated: false };
  return bridge.events(since, clear);
}

module.exports = { KEY: WEBMCP_KEY, installHook, listInPage, invokeInPage, eventsInPage };
