'use strict';
// Cookies, web storage, emulation, clipboard and saved-state actions for the Dial browser driver (CommonJS, Node 20+).
// Inlined into the `node -e` driver as an IIFE, so `require` is available only through the driver's process.
// Rust counterparts: state.rs (validation, redaction, state files) and emulation.rs (argument parsing).
// Page code runs only inside the page via Playwright page.evaluate; storage helpers are self-contained.

const ACTIONS = new Set([
  'cookies', 'set_cookies', 'clear_cookies', 'storage', 'emulate', 'devices',
  'clipboard_read', 'clipboard_write', 'clipboard_copy', 'clipboard_paste',
  'save_state', 'load_state',
]);

// Runs in the page realm. `area` is local or session; `operation` is get, set or clear.
const STORAGE_PAGE = ({ area, operation, key, value }) => {
  const store = area === 'session' ? window.sessionStorage : window.localStorage;
  if (operation === 'get') {
    if (key !== undefined) {
      const found = store.getItem(key);
      return { entries: found === null ? {} : { [key]: found } };
    }
    const entries = {};
    for (let i = 0; i < store.length; i++) {
      const name = store.key(i);
      entries[name] = store.getItem(name);
    }
    return { entries };
  }
  if (operation === 'set') {
    store.setItem(key, value);
    return {};
  }
  store.clear();
  return {};
};

const HTTP_URL = /^https?:\/\//;

function playwrightDevices() {
  return require(process.env.DIAL_PLAYWRIGHT).devices;
}

function httpOrigin(page, what) {
  const url = page.url();
  if (!HTTP_URL.test(url)) {
    throw new Error(`${what} needs a page on an http(s) origin; the current page is ${url.slice(0, 200)}`);
  }
  return new URL(url).origin;
}

// Applies tab-scoped emulation: device preset, viewport, user agent, touch, timezone, locale, color scheme.
// Context-wide settings (offline, geolocation) are handled by `emulate` itself.
async function applyTabEmulation(tab, emulation) {
  await tab.ready;
  if (!tab.cdp) throw new Error('page has no CDP session; emulation is unavailable on this page');
  const page = tab.page;
  const applied = [];
  let base = {};
  if (emulation.device !== undefined) {
    const devices = playwrightDevices();
    if (!Object.hasOwn(devices, emulation.device)) {
      throw new Error(`device ${JSON.stringify(emulation.device)} is not available in this Playwright build`);
    }
    base = devices[emulation.device];
    applied.push('device');
  }
  const viewport = emulation.viewport;
  const width = viewport?.width ?? base.viewport?.width;
  const height = viewport?.height ?? base.viewport?.height;
  if (width !== undefined && height !== undefined) {
    if (viewport?.scale !== undefined || emulation.device !== undefined) {
      await tab.cdp.send('Emulation.setDeviceMetricsOverride', {
        width,
        height,
        deviceScaleFactor: viewport?.scale ?? base.deviceScaleFactor ?? 1,
        mobile: base.isMobile ?? false,
      });
    } else {
      await page.setViewportSize({ width, height });
    }
    applied.push('viewport');
  }
  if (emulation.device !== undefined) {
    await tab.cdp.send('Emulation.setTouchEmulationEnabled', { enabled: base.hasTouch === true });
  }
  const userAgent = emulation.user_agent ?? (emulation.device !== undefined ? base.userAgent : undefined);
  if (userAgent !== undefined) {
    await tab.cdp.send('Emulation.setUserAgentOverride', { userAgent });
    applied.push('user_agent');
  }
  if (emulation.timezone !== undefined) {
    await tab.cdp.send('Emulation.setTimezoneOverride', { timezoneId: emulation.timezone });
    applied.push('timezone');
  }
  if (emulation.locale !== undefined) {
    await tab.cdp.send('Emulation.setLocaleOverride', { locale: emulation.locale });
    applied.push('locale');
  }
  if (emulation.color_scheme !== undefined) {
    await page.emulateMedia({ colorScheme: emulation.color_scheme });
    applied.push('color_scheme');
  }
  return applied;
}

async function emulate(request, host) {
  const emulation = request.emulation;
  const tab = host.tabFor(request);
  const applied = await applyTabEmulation(tab, emulation);
  const contextWide = [];
  if (Object.hasOwn(emulation, 'offline')) {
    await host.context.setOffline(emulation.offline);
    contextWide.push('offline');
  }
  if (Object.hasOwn(emulation, 'geolocation')) {
    if (emulation.geolocation === null) {
      await host.context.setGeolocation();
    } else {
      await host.context.grantPermissions(['geolocation']);
      await host.context.setGeolocation(emulation.geolocation);
    }
    contextWide.push('geolocation');
  }
  return { applied, context_wide: contextWide };
}

async function clipboardGrant(host, page) {
  const origin = httpOrigin(page, 'clipboard access');
  await host.context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin });
  await page.bringToFront();
}

async function loadState(request, host) {
  const state = request.loaded_state;
  const tab = host.tabFor(request);
  await tab.ready;
  const page = tab.page;
  if (state.cookies.length > 0) await host.context.addCookies(state.cookies);
  const current = new URL(page.url()).origin;
  const restorable = state.origins.find(entry => new URL(entry.origin).origin === current);
  const skipped = state.origins
    .map(entry => new URL(entry.origin).origin)
    .filter(origin => origin !== current);
  let localStorageEntries = 0;
  if (restorable && restorable.localStorage.length > 0 && HTTP_URL.test(page.url())) {
    await page.evaluate(entries => {
      for (const entry of entries) localStorage.setItem(entry.name, entry.value);
    }, restorable.localStorage);
    localStorageEntries = restorable.localStorage.length;
  }
  return {
    cookies: state.cookies.length,
    local_storage_entries: localStorageEntries,
    restored_origin: restorable && localStorageEntries > 0 ? current : null,
    skipped_origins: skipped,
  };
}

async function run(request, host) {
  const context = host.context;
  switch (request.action) {
    case 'cookies':
      return { cookies: await context.cookies(request.urls?.length ? request.urls : undefined) };
    case 'set_cookies':
      await context.addCookies(request.cookies);
      return { set: request.cookies.length };
    case 'clear_cookies': {
      if (!request.names) {
        await context.clearCookies();
        return { cleared: 'all' };
      }
      for (const name of request.names) await context.clearCookies({ name });
      return { cleared: request.names };
    }
    case 'devices': {
      const available = playwrightDevices();
      return { devices: request.devices.filter(name => Object.hasOwn(available, name)) };
    }
    case 'emulate':
      return emulate(request, host);
    case 'save_state':
      return { storage_state: await context.storageState() };
    case 'load_state':
      return loadState(request, host);
    case 'storage': {
      const tab = host.tabFor(request);
      await tab.ready;
      httpOrigin(tab.page, 'storage');
      const result = await tab.page.evaluate(STORAGE_PAGE, request.storage);
      return { area: request.storage.area, operation: request.storage.operation, ...result };
    }
    case 'clipboard_read': {
      const tab = host.tabFor(request);
      await tab.ready;
      await clipboardGrant(host, tab.page);
      return { text: await tab.page.evaluate(() => navigator.clipboard.readText()) };
    }
    case 'clipboard_write': {
      const tab = host.tabFor(request);
      await tab.ready;
      await clipboardGrant(host, tab.page);
      await tab.page.evaluate(text => navigator.clipboard.writeText(text), request.text);
      return { chars: request.text.length };
    }
    case 'clipboard_copy':
    case 'clipboard_paste': {
      const tab = host.tabFor(request);
      await tab.ready;
      await tab.page.bringToFront();
      const shortcut = request.action === 'clipboard_copy' ? 'ControlOrMeta+c' : 'ControlOrMeta+v';
      await tab.page.keyboard.press(shortcut);
      return { pressed: shortcut };
    }
    default:
      throw new Error(`unsupported site action: ${request.action}`);
  }
}

module.exports = { ACTIONS, run, applyTabEmulation };
