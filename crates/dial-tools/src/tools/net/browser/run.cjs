'use strict';
// Page-realm `run` and `call` helpers for the Dial browser driver (CommonJS, Node 20+).
// Code executes only inside the page via Playwright page.evaluate; Node never evaluates page code.
// Rust counterpart: run.rs (CALL_METHODS and the caps below must stay in sync).

const RUN_VALUE_PREVIEW_CHARS = 256 * 1024;
const SCREENSHOT_MAX_BYTES = 5 * 1024 * 1024;
const FULL_PAGE_MAX_PX = 12000;
const MAX_CHAIN_STEPS = 8;
const MAX_ARGS = 32;
const MAX_ARGS_JSON_CHARS = 64 * 1024;
const MIN_TIMEOUT_MS = 1000;
const MAX_TIMEOUT_MS = 3600000;
const SELECTOR_STATES = ['visible', 'hidden', 'attached', 'detached'];
const SELECT_VALUES_MAX = 32;
const quarantinedPages = new WeakSet();

// Runs in the page realm. Self-contained: Playwright serializes it with Function.prototype.toString.
// kind 'run'  -> `source` is a function body evaluated as an async function with assert/sleep bindings.
// kind 'eval' -> `source` is an expression evaluated with indirect eval (Dial's evaluate semantics).
const PAGE_EXEC = async ({ kind, source, limit }) => {
  const encode = value => {
    let json;
    try {
      json = JSON.stringify(value === undefined ? null : value);
    } catch (error) {
      throw new TypeError('run result is not JSON-serializable: ' + error.message);
    }
    if (json === undefined) return { value: null };
    if (json.length <= limit) return { value: JSON.parse(json) };
    // Never cut between UTF-16 surrogates: Rust rejects the lone half when it parses the JSON reply.
    let preview = json.slice(0, limit);
    if (/[\uD800-\uDBFF]$/.test(preview)) preview = preview.slice(0, -1);
    return { value_preview: preview, truncated: true, total_chars: json.length };
  };
  if (kind === 'run') {
    const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
    const assert = (condition, message) => {
      if (!condition) throw new Error(message === undefined ? 'Assertion failed' : String(message));
    };
    const sleep = ms => new Promise(resolve => setTimeout(resolve, Math.max(0, Math.min(Number(ms) || 0, 60000))));
    const isElVisible = el => {
      if (!el || !el.isConnected) return false;
      const style = window.getComputedStyle(el);
      if (!style || style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) return false;
      const rect = el.getBoundingClientRect();
      return rect.width > 0 || rect.height > 0;
    };
    const fireInputEvents = el => {
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    };
    const setElementValue = (el, value) => {
      const text = String(value ?? '');
      if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {
        const proto = el instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
        const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
        if (setter) setter.call(el, text);
        else el.value = text;
        fireInputEvents(el);
      } else if (el.isContentEditable) {
        el.textContent = text;
        fireInputEvents(el);
      } else {
        el.value = text;
        fireInputEvents(el);
      }
    };
    const queryAll = (finder) => (typeof finder === 'function' ? finder() : Array.from(document.querySelectorAll(finder)));
    const makeLocator = (finder, index = null) => {
      const resolveAll = () => queryAll(finder);
      const resolveOne = () => {
        const all = resolveAll();
        return index === null ? (all[0] || null) : (all[index] || null);
      };
      const requireOne = () => {
        const el = resolveOne();
        if (!el) throw new Error(`Locator matched no elements: ${typeof finder === 'string' ? finder : 'custom locator'}`);
        return el;
      };
      return {
        first: () => makeLocator(finder, 0),
        last: () => makeLocator(() => { const a = resolveAll(); return a.length ? [a[a.length - 1]] : []; }, 0),
        nth: i => makeLocator(finder, i),
        count: async () => resolveAll().length,
        all: async () => resolveAll().map((_, i) => makeLocator(finder, i)),
        elementHandle: async () => resolveOne(),
        textContent: async () => resolveOne()?.textContent ?? null,
        innerText: async () => resolveOne()?.innerText ?? '',
        innerHTML: async () => resolveOne()?.innerHTML ?? '',
        inputValue: async () => String(requireOne().value ?? ''),
        getAttribute: async name => resolveOne()?.getAttribute(name) ?? null,
        isVisible: async () => isElVisible(resolveOne()),
        isHidden: async () => !isElVisible(resolveOne()),
        isChecked: async () => Boolean(requireOne().checked),
        isEnabled: async () => !requireOne().disabled,
        isDisabled: async () => Boolean(requireOne().disabled),
        click: async () => { const el = requireOne(); el.scrollIntoView?.({ block: 'center', inline: 'center' }); el.click(); },
        focus: async () => { requireOne().focus(); },
        fill: async value => { const el = requireOne(); el.focus?.(); setElementValue(el, value); },
        type: async value => { const el = requireOne(); el.focus?.(); setElementValue(el, (el.value ?? '') + String(value ?? '')); },
        check: async () => { const el = requireOne(); if (!el.checked) el.click(); },
        uncheck: async () => { const el = requireOne(); if (el.checked) el.click(); },
        selectOption: async value => {
          const el = requireOne();
          const wanted = Array.isArray(value) ? value.map(String) : [String(value)];
          for (const opt of Array.from(el.options || [])) {
            opt.selected = wanted.includes(opt.value) || wanted.includes(opt.textContent?.trim());
          }
          fireInputEvents(el);
        },
        boundingBox: async () => {
          const el = resolveOne();
          if (!el) return null;
          const r = el.getBoundingClientRect();
          return { x: r.x, y: r.y, width: r.width, height: r.height };
        },
        waitFor: async (opts = {}) => {
          const state = opts?.state ?? 'visible';
          const deadline = Date.now() + Math.min(Math.max(Number(opts?.timeout ?? 5000), 50), 30000);
          for (;;) {
            const el = resolveOne();
            const ok = state === 'attached' ? Boolean(el)
              : state === 'detached' ? !el
              : state === 'hidden' ? !isElVisible(el)
              : isElVisible(el);
            if (ok) return;
            if (Date.now() >= deadline) throw new Error(`Timed out waiting for locator state "${state}"`);
            await sleep(50);
          }
        },
        evaluate: async (fn, arg) => fn(requireOne(), arg),
        evaluateAll: async (fn, arg) => fn(resolveAll(), arg),
        allTextContents: async () => resolveAll().map(el => el.textContent ?? ''),
        allInnerTexts: async () => resolveAll().map(el => el.innerText ?? ''),
      };
    };
    const hostOnly = method => () => {
      throw new Error(`page.${method}() is a host-side browser action and cannot run inside the page realm; use browser action="call" or action="${method}" instead`);
    };
    const dispatchKey = (key, type) => {
      const target = document.activeElement || document.body || document.documentElement;
      target.dispatchEvent(new KeyboardEvent(type, { key, bubbles: true, cancelable: true }));
    };
    const page = {
      url: () => window.location.href,
      title: async () => document.title,
      content: async () => document.documentElement?.outerHTML ?? '',
      evaluate: async (fn, ...args) => (typeof fn === 'function' ? fn(...args) : (0, eval)(fn)),
      waitForTimeout: sleep,
      waitForSelector: async (selector, opts) => {
        const loc = makeLocator(selector);
        await loc.waitFor(opts);
        return document.querySelector(selector);
      },
      waitForFunction: async (fn, arg, opts = {}) => {
        const deadline = Date.now() + Math.min(Math.max(Number(opts?.timeout ?? 5000), 50), 30000);
        for (;;) {
          const val = typeof fn === 'function' ? await fn(arg) : await (0, eval)(fn);
          if (val) return val;
          if (Date.now() >= deadline) throw new Error('Timed out waiting for function condition');
          await sleep(50);
        }
      },
      $: async selector => document.querySelector(selector),
      $$: async selector => Array.from(document.querySelectorAll(selector)),
      $eval: async (selector, fn, arg) => makeLocator(selector).evaluate(fn, arg),
      $$eval: async (selector, fn, arg) => makeLocator(selector).evaluateAll(fn, arg),
      locator: selector => makeLocator(selector),
      getByTestId: id => makeLocator(`[data-testid="${CSS.escape(String(id))}"]`),
      getByPlaceholder: text => makeLocator(() => Array.from(document.querySelectorAll('[placeholder]')).filter(el => (el.getAttribute('placeholder') || '').includes(String(text)))),
      getByText: (text, opts = {}) => makeLocator(() => {
        const exact = Boolean(opts?.exact);
        const wanted = String(text);
        return Array.from(document.querySelectorAll('*')).filter(el => {
          if (el.children.length > 0 && Array.from(el.children).some(c => (c.textContent || '').includes(wanted))) return false;
          const content = (el.textContent || '').trim();
          return exact ? content === wanted : content.includes(wanted);
        });
      }),
      getByRole: (role, opts = {}) => makeLocator(() => {
        const r = String(role).toLowerCase();
        const tagMap = { button: 'button,[role="button"],input[type="button"],input[type="submit"]', link: 'a[href],[role="link"]', textbox: 'input:not([type]),input[type="text"],input[type="search"],input[type="email"],textarea,[role="textbox"]', heading: 'h1,h2,h3,h4,h5,h6,[role="heading"]', checkbox: 'input[type="checkbox"],[role="checkbox"]' };
        const selector = tagMap[r] || `[role="${CSS.escape(r)}"]`;
        let els = Array.from(document.querySelectorAll(selector));
        if (opts?.name !== undefined) {
          const matcher = opts.name instanceof RegExp ? s => opts.name.test(s) : s => s.toLowerCase().includes(String(opts.name).toLowerCase());
          els = els.filter(el => matcher((el.getAttribute('aria-label') || el.textContent || el.value || '').trim()));
        }
        return els;
      }),
      getByLabel: text => makeLocator(() => {
        const wanted = String(text).toLowerCase();
        const results = [];
        for (const label of Array.from(document.querySelectorAll('label'))) {
          if (!(label.textContent || '').toLowerCase().includes(wanted)) continue;
          const forId = label.getAttribute('for');
          const control = forId ? document.getElementById(forId) : label.querySelector('input,textarea,select');
          if (control) results.push(control);
        }
        return results;
      }),
      textContent: selector => makeLocator(selector).textContent(),
      innerText: selector => makeLocator(selector).innerText(),
      innerHTML: selector => makeLocator(selector).innerHTML(),
      getAttribute: (selector, name) => makeLocator(selector).getAttribute(name),
      isVisible: selector => makeLocator(selector).isVisible(),
      click: selector => makeLocator(selector).click(),
      fill: (selector, value) => makeLocator(selector).fill(value),
      type: (selector, value) => makeLocator(selector).type(value),
      focus: selector => makeLocator(selector).focus(),
      check: selector => makeLocator(selector).check(),
      uncheck: selector => makeLocator(selector).uncheck(),
      selectOption: (selector, value) => makeLocator(selector).selectOption(value),
      keyboard: {
        press: async key => { dispatchKey(key, 'keydown'); dispatchKey(key, 'keypress'); dispatchKey(key, 'keyup'); },
        down: async key => { dispatchKey(key, 'keydown'); },
        up: async key => { dispatchKey(key, 'keyup'); },
        type: async text => { for (const ch of String(text ?? '')) { dispatchKey(ch, 'keydown'); dispatchKey(ch, 'keypress'); dispatchKey(ch, 'keyup'); } },
      },
      goto: hostOnly('goto'),
      reload: hostOnly('reload'),
      goBack: hostOnly('back'),
      goForward: hostOnly('forward'),
      screenshot: hostOnly('screenshot'),
      route: hostOnly('route'),
    };
    return encode(await new AsyncFunction('assert', 'sleep', 'page', source)(assert, sleep, page));
  }
  return encode(await (0, eval)(source));
};

function requireTimeout(timeoutMs) {
  if (!Number.isFinite(timeoutMs) || timeoutMs < MIN_TIMEOUT_MS || timeoutMs > MAX_TIMEOUT_MS) {
    throw new RangeError(`timeoutMs must be between ${MIN_TIMEOUT_MS} and ${MAX_TIMEOUT_MS}`);
  }
  return Math.floor(timeoutMs);
}

function requirePage(page) {
  if (!page || typeof page.evaluate !== 'function') throw new TypeError('a Playwright page is required');
  if (quarantinedPages.has(page)) throw new Error('Browser page is quarantined after an execution timeout; restart the browser session');
}

function withTimeout(work, timeoutMs, label, page, onTimeout) {
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => {
      // Poison synchronously, before exposing the timeout to the next queued request. Closing a
      // renderer may hang too, so retirement is best-effort and never delays the deadline.
      quarantinedPages.add(page);
      try { Promise.resolve(onTimeout?.()).catch(() => {}); } catch (_) {}
      reject(new Error(`${label} timed out after ${timeoutMs}ms; browser context quarantined, restart required`));
    }, timeoutMs);
  });
  return Promise.race([work, deadline]).finally(() => clearTimeout(timer));
}

// page.evaluate cannot be cancelled: quarantine before returning, and let the owner retire its
// context without closing an operator-owned attached browser.
async function runCode({ page, code, timeoutMs, onTimeout }) {
  requirePage(page);
  if (typeof code !== 'string' || code.trim().length === 0) throw new TypeError('run requires non-empty code');
  const ms = requireTimeout(timeoutMs);
  return withTimeout(page.evaluate(PAGE_EXEC, { kind: 'run', source: code, limit: RUN_VALUE_PREVIEW_CHARS }), ms, 'Browser code execution', page, onTimeout);
}

function expectArgs(name, args, min, max) {
  if (args.length < min || args.length > max) {
    const expected = min === max ? String(min) : `${min}-${max}`;
    throw new TypeError(`${name} expects ${expected} argument(s), got ${args.length}`);
  }
}

function requireString(label, value) {
  if (typeof value !== 'string' || value.length === 0) throw new TypeError(`${label} must be a non-empty string`);
  return value;
}

function requireText(label, value) {
  if (typeof value !== 'string') throw new TypeError(`${label} must be a string`);
  return value;
}

function requireValues(value) {
  if (typeof value === 'string') return value;
  if (Array.isArray(value) && value.length <= SELECT_VALUES_MAX && value.every(item => typeof item === 'string')) {
    return value;
  }
  throw new TypeError(`select values must be a string or at most ${SELECT_VALUES_MAX} strings`);
}

// Locator-based helper: args[0] is the selector, the rest are passed to `op` with the timeout.
function selectorAction(name, extraArity, op) {
  return async (page, args, timeoutMs) => {
    expectArgs(name, args, 1 + extraArity, 1 + extraArity);
    const locator = page.locator(requireString(`${name} selector`, args[0]));
    const out = await op(locator, args.slice(1), timeoutMs);
    return out === undefined ? true : out;
  };
}

// Allowlisted `call` helpers. Each maps to the same Playwright operation Dial's driver uses for that action.
// Keys are the public method names; a step naming anything else is rejected before any step runs.
const HELPERS = Object.freeze({
  url: (page, args) => {
    expectArgs('url', args, 0, 0);
    return page.url();
  },
  title: (page, args) => {
    expectArgs('title', args, 0, 0);
    return page.title();
  },
  goto: async (page, args, timeoutMs) => {
    expectArgs('goto', args, 1, 1);
    const url = requireString('goto url', args[0]);
    if (!/^(https?:\/\/|about:blank$)/i.test(url)) throw new TypeError('goto url must be an HTTP(S) URL or about:blank');
    const response = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: timeoutMs });
    return { url: page.url(), title: await page.title(), status: response ? response.status() : null };
  },
  click: selectorAction('click', 0, (locator, _rest, timeoutMs) => locator.click({ timeout: timeoutMs })),
  fill: selectorAction('fill', 1, (locator, [text], timeoutMs) =>
    locator.fill(requireText('fill text', text), { timeout: timeoutMs }),
  ),
  type: selectorAction('type', 1, (locator, [text], timeoutMs) =>
    locator.pressSequentially(requireText('type text', text), { timeout: timeoutMs }),
  ),
  press: selectorAction('press', 1, (locator, [key], timeoutMs) =>
    locator.press(requireString('press key', key), { timeout: timeoutMs }),
  ),
  check: selectorAction('check', 0, (locator, _rest, timeoutMs) => locator.check({ timeout: timeoutMs })),
  uncheck: selectorAction('uncheck', 0, (locator, _rest, timeoutMs) => locator.uncheck({ timeout: timeoutMs })),
  hover: selectorAction('hover', 0, (locator, _rest, timeoutMs) => locator.hover({ timeout: timeoutMs })),
  focus: selectorAction('focus', 0, (locator, _rest, timeoutMs) => locator.focus({ timeout: timeoutMs })),
  select: selectorAction('select', 1, (locator, [values], timeoutMs) =>
    locator.selectOption(requireValues(values), { timeout: timeoutMs }),
  ),
  waitForSelector: async (page, args, timeoutMs) => {
    expectArgs('waitForSelector', args, 1, 2);
    const selector = requireString('waitForSelector selector', args[0]);
    const state = args[1] === undefined ? 'visible' : args[1];
    if (!SELECTOR_STATES.includes(state)) {
      throw new TypeError(`waitForSelector state must be one of ${SELECTOR_STATES.join(', ')}`);
    }
    await page.locator(selector).waitFor({ state, timeout: timeoutMs });
    return true;
  },
  evaluate: async (page, args) => {
    expectArgs('evaluate', args, 1, 1);
    const expression = requireString('evaluate expression', args[0]);
    return page.evaluate(PAGE_EXEC, { kind: 'eval', source: expression, limit: RUN_VALUE_PREVIEW_CHARS });
  },
  screenshot: async (page, args, timeoutMs) => {
    expectArgs('screenshot', args, 0, 1);
    const fullPage = args[0] === undefined ? false : args[0];
    if (typeof fullPage !== 'boolean') throw new TypeError('screenshot fullPage must be a boolean');
    if (fullPage) {
      const height = await page.evaluate(() => Math.max(document.body?.scrollHeight || 0, document.documentElement.scrollHeight));
      if (height > FULL_PAGE_MAX_PX) throw new Error(`full-page screenshot exceeds ${FULL_PAGE_MAX_PX}px height; use viewport screenshot`);
    }
    const bytes = await page.screenshot({ type: 'png', fullPage, timeout: timeoutMs });
    if (bytes.length > SCREENSHOT_MAX_BYTES) {
      throw new Error('screenshot exceeds the provider attachment limit of 5 MiB; use a viewport screenshot');
    }
    return { artifact: { media_type: 'image/png', encoding: 'base64', data: bytes.toString('base64') } };
  },
});

// Validate every step before running any of them, so a bad later step never leaves a half-run chain.
function validateChain(chain) {
  if (!Array.isArray(chain) || chain.length === 0) throw new TypeError("call requires a non-empty 'chain' array");
  if (chain.length > MAX_CHAIN_STEPS) throw new RangeError(`call chains allow at most ${MAX_CHAIN_STEPS} steps`);
  return chain.map((step, index) => {
    if (!step || typeof step !== 'object' || typeof step.method !== 'string') {
      throw new TypeError(`chain[${index}].method must be a string`);
    }
    if (!Object.prototype.hasOwnProperty.call(HELPERS, step.method)) {
      throw new Error(`Unknown call method "${step.method}". Allowed: ${Object.keys(HELPERS).join(', ')}.`);
    }
    const args = step.args === undefined ? [] : step.args;
    if (!Array.isArray(args)) throw new TypeError(`chain[${index}].args must be an array`);
    if (args.length > MAX_ARGS) throw new RangeError(`chain[${index}] allows at most ${MAX_ARGS} arguments`);
    if (JSON.stringify(args).length > MAX_ARGS_JSON_CHARS) {
      throw new RangeError(`chain[${index}] arguments exceed ${MAX_ARGS_JSON_CHARS} characters`);
    }
    return { method: step.method, args };
  });
}

// Runs steps sequentially. Resolves to { value: [result of each step, in order] }.
async function runCall({ page, chain, timeoutMs, onTimeout }) {
  requirePage(page);
  const steps = validateChain(chain);
  const ms = requireTimeout(timeoutMs);
  // On timeout the caller gets the error at once; `aborted` keeps later steps from touching the page afterwards.
  let aborted = false;
  const work = (async () => {
    const results = [];
    for (const step of steps) {
      if (aborted || quarantinedPages.has(page)) throw new Error('Browser call was aborted by its timeout');
      results.push(await HELPERS[step.method](page, step.args, ms));
    }
    return { value: results };
  })();
  try {
    return await withTimeout(work, ms, 'Browser call', page, onTimeout);
  } catch (error) {
    aborted = true;
    throw error;
  }
}

module.exports = {
  runCode,
  runCall,
  CALL_METHODS: Object.freeze(Object.keys(HELPERS)),
  RUN_VALUE_PREVIEW_CHARS,
};
