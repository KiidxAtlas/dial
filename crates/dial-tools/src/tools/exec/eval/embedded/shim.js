// Embedded `eval` runtime: the QuickJS counterpart of driver.cjs. It speaks the same newline-delimited
// JSON protocol on fd 3 and applies the same cell semantics. Rust (embedded.rs) installs the natives
// below before this script runs, and this script removes them from the global object at once.
(function () {
  'use strict';
  const natives = {
    readLine: __dial_read_line,
    writeLine: __dial_write_line,
    completionSource: __dial_completion_source,
    evaluate: __dial_eval,
    runJobs: __dial_run_jobs,
    sleep: __dial_sleep,
  };
  for (const name of ['__dial_read_line', '__dial_write_line', '__dial_completion_source', '__dial_eval', '__dial_run_jobs', '__dial_sleep']) {
    delete globalThis[name];
  }

  const LIMIT = 20000;
  const MAX_FRAME = 12 * 1024 * 1024;
  const MAX_CALLBACKS = 128;
  const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;
  const ESCAPED_LONE_SURROGATE = /\\(?:u[dD][89aAbB][0-9a-fA-F]{2}\\u[dD][c-fC-F][0-9a-fA-F]{2}|u[dD][89a-fA-F][0-9a-fA-F]{2}|.)/g;
  const toWellFormed = text => text.replace(LONE_SURROGATE, '\uFFFD');
  const utf8Length = text => {
    let bytes = 0;
    for (const character of text) {
      const code = character.codePointAt(0);
      bytes += code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
    }
    return bytes;
  };

  let current = null;
  let callbackSequence = 0;

  class Capture {
    constructor() { this.parts = []; this.size = 0; this.dropped = 0; this.originalBytes = 0; this.retainedBytes = 0; }
    write(text) {
      text = String(text);
      this.originalBytes += utf8Length(toWellFormed(text));
      const take = Math.min(text.length, Math.max(0, LIMIT - this.size));
      if (take) this.parts.push(text.slice(0, take));
      this.retainedBytes += utf8Length(toWellFormed(text.slice(0, take)));
      this.size += take;
      this.dropped += text.length - take;
    }
    value() {
      return this.parts.join('') + (this.dropped ? `\n[truncated ${this.dropped} characters]` : '');
    }
  }

  class ToolError extends Error {
    constructor(message, result) { super(message); this.name = 'ToolError'; this.result = result; }
  }

  function readFrame() {
    const got = natives.readLine();
    if (got.eof) return null;
    if (got.overflow) throw new ToolError('controller frame exceeds the eval limit');
    let value;
    try {
      value = JSON.parse(got.line);
    } catch {
      throw new ToolError('controller frame is not valid JSON');
    }
    if (!value || typeof value !== 'object' || Array.isArray(value)) throw new ToolError('controller frame must be an object');
    return value;
  }

  function sendFrame(value) {
    // Rust's JSON receiver rejects lone surrogates, so normalize them at the wire boundary.
    const json = toWellFormed(JSON.stringify(value)).replace(ESCAPED_LONE_SURROGATE, escape =>
      /^\\u[dD][89a-fA-F][0-9a-fA-F]{2}$/.test(escape) ? '\\ufffd' : escape);
    if (utf8Length(json) + 1 > MAX_FRAME) throw new ToolError('eval frame exceeds the protocol limit');
    natives.writeLine(json);
  }

  function callback(kind, fields = {}) {
    const operation = current;
    if (!operation || !operation.active) throw new ToolError('tool callback belongs to an expired cell');
    if (++operation.callbacks > MAX_CALLBACKS) throw new ToolError('cell callback limit exceeded');
    const callbackId = ++callbackSequence;
    sendFrame({ callback: kind, callback_id: callbackId, operation_id: operation.id, ...fields });
    // Deliberately synchronous, as in the Node driver.
    const reply = readFrame();
    if (!reply || reply.callback_id !== callbackId || reply.operation_id !== operation.id) throw new ToolError('controller callback response identity mismatch');
    if (Object.hasOwn(reply, 'error')) throw new ToolError(String(reply.error));
    const result = reply.result;
    if (!result || typeof result !== 'object' || typeof result.is_error !== 'boolean') throw new ToolError('malformed controller tool result');
    if (result.is_error) throw new ToolError(result.text || 'tool call denied or failed', result);
    return result;
  }

  const call = (name, args = {}) => {
    if (typeof name !== 'string' || !name || name.length > 256) throw new ToolError('tool name must be a nonempty string of at most 256 characters');
    if (!args || typeof args !== 'object' || Array.isArray(args)) throw new ToolError('tool arguments must be an object');
    return callback('tool', { name, arguments: args });
  };

  // Reserved controller name (BATCH_CALLBACK in eval.rs); no provider tool can carry it.
  const BATCH = 'dial.batch';
  const batch = entries => {
    if (!Array.isArray(entries) || entries.length > MAX_CALLBACKS) throw new ToolError(`tool.batch expects an array of at most ${MAX_CALLBACKS} {name, args} entries`);
    const calls = entries.map((entry, index) => {
      const { name, args = {} } = entry ?? {};
      if (typeof name !== 'string' || !name || name.length > 256) throw new ToolError(`tool.batch entry ${index}: tool name must be a nonempty string of at most 256 characters`);
      if (!args || typeof args !== 'object' || Array.isArray(args)) throw new ToolError(`tool.batch entry ${index}: tool arguments must be an object`);
      return { name, arguments: args };
    });
    if (!calls.length) return [];
    const operation = current;
    // Each entry is one callback against the cell's limit, whether or not it runs concurrently.
    if (operation) operation.callbacks += calls.length - 1;
    let result;
    try {
      result = callback('tool', { name: BATCH, arguments: { calls } });
    } catch (error) {
      // A controller without batch support refuses the reserved name: run the entries serially.
      if (!(error instanceof ToolError) || !(error.result?.data?.code === 'unsupported_batch' && error.result?.data?.executed === false)) throw error;
      if (operation) operation.callbacks -= calls.length;
      return calls.map(({ name, arguments: args }) => {
        try {
          return call(name, args);
        } catch (failure) {
          if (!(failure instanceof ToolError)) throw failure;
          return failure.result ?? { is_error: true, text: failure.message, data: null };
        }
      });
    }
    if (!Array.isArray(result.data) || result.data.length !== calls.length || result.data.some(item => !item || typeof item !== 'object' || typeof item.is_error !== 'boolean')) throw new ToolError('controller returned a malformed tool.batch result', result);
    return result.data;
  };

  const tool = new Proxy(Object.create(null), {
    get(_target, name) {
      if (name === 'call') return call;
      if (name === 'batch') return batch;
      if (name === 'schemas') {
        const result = callback('schemas');
        if (!Array.isArray(result.data)) throw new ToolError('controller did not return current tool schemas', result);
        return result.data;
      }
      if (name === 'then' || typeof name !== 'string' || name.startsWith('_')) return undefined;
      return (args = {}) => call(name, args);
    },
  });

  // ---- util.inspect / util.format approximation (Node defaults, breakLength 80) ----
  const IDENTIFIER = /^[A-Za-z_$][A-Za-z0-9_$]*$/;
  const ESCAPES = { '\b': '\\b', '\t': '\\t', '\n': '\\n', '\v': '\\v', '\f': '\\f', '\r': '\\r' };
  function quote(text) {
    const quoteChar = !text.includes("'") ? "'" : !text.includes('"') ? '"' : !text.includes('`') ? '`' : "'";
    let out = '';
    for (const character of text) {
      if (character === quoteChar || character === '\\') out += '\\' + character;
      else if (ESCAPES[character]) out += ESCAPES[character];
      else if (character < ' ' || character === '\x7f') out += '\\x' + character.charCodeAt(0).toString(16).toUpperCase().padStart(2, '0');
      else out += character;
    }
    return quoteChar + out + quoteChar;
  }
  function constructorName(value) {
    const prototype = Object.getPrototypeOf(value);
    if (prototype === null) return null;
    const constructor = prototype.constructor;
    return typeof constructor === 'function' && constructor.name ? constructor.name : '';
  }
  function functionLabel(fn) {
    if (/^class\b/.test(Function.prototype.toString.call(fn))) return fn.name ? `[class ${fn.name}]` : '[class (anonymous)]';
    const kind = fn[Symbol.toStringTag] || 'Function';
    return fn.name ? `[${kind}: ${fn.name}]` : `[${kind} (anonymous)]`;
  }
  function inspectValue(value, depth, seen) {
    if (typeof value === 'string') return quote(value);
    if (typeof value === 'number') return Object.is(value, -0) ? '-0' : String(value);
    if (typeof value === 'bigint') return `${value}n`;
    if (typeof value === 'function') return functionLabel(value);
    if (value === null || typeof value !== 'object') return String(value);
    if (seen.includes(value)) return '[Circular *1]';
    if (value instanceof Error) return String(value.stack || value);
    if (value instanceof Date) return Number.isNaN(value.getTime()) ? 'Invalid Date' : value.toISOString();
    if (value instanceof RegExp) return String(value);
    const name = constructorName(value);
    if (depth < 0) return Array.isArray(value) ? '[Array]' : `[${name || 'Object'}]`;
    seen.push(value);
    try {
      return formatObject(value, depth, seen, name);
    } finally {
      seen.pop();
    }
  }
  function formatObject(value, depth, seen, name) {
    const entries = [];
    let open = '{';
    let close = '}';
    let prefix = '';
    if (Array.isArray(value)) {
      open = '[';
      close = ']';
      if (name && name !== 'Array') prefix = `${name}(${value.length}) `;
      const shown = Math.min(value.length, 100);
      for (let index = 0; index < shown; index++) {
        entries.push(index in value ? inspectValue(value[index], depth - 1, seen) : '<1 empty item>');
      }
      if (value.length > shown) entries.push(`... ${value.length - shown} more item${value.length - shown > 1 ? 's' : ''}`);
    } else if (value instanceof Map) {
      prefix = `Map(${value.size}) `;
      for (const [key, item] of value) entries.push(`${inspectValue(key, depth - 1, seen)} => ${inspectValue(item, depth - 1, seen)}`);
    } else if (value instanceof Set) {
      prefix = `Set(${value.size}) `;
      for (const item of value) entries.push(inspectValue(item, depth - 1, seen));
    } else if (value instanceof Promise) {
      // The engine exposes no synchronous promise state, so settled promises also show as pending.
      prefix = 'Promise ';
      entries.push('<pending>');
    } else {
      if (name === null) prefix = '[Object: null prototype] ';
      else if (name && name !== 'Object') prefix = `${name} `;
      for (const key of Object.keys(value)) {
        entries.push(`${IDENTIFIER.test(key) ? key : quote(key)}: ${inspectValue(value[key], depth - 1, seen)}`);
      }
    }
    if (entries.length === 0) return prefix + open + close;
    const start = entries.length + open.length + prefix.length + 10;
    const total = entries.reduce((sum, entry) => sum + entry.length, 0) + entries.length + start;
    if (total <= 80 && entries.every(entry => !entry.includes('\n'))) return `${prefix}${open} ${entries.join(', ')} ${close}`;
    const pad = '  ';
    return `${prefix}${open}\n${entries.map(entry => pad + entry.replace(/\n/g, '\n' + pad)).join(',\n')}\n${close}`;
  }
  function inspect(value, depth) {
    return inspectValue(value, depth, []);
  }
  function formatSpecifier(spec, arg) {
    switch (spec) {
      case 's':
        if (typeof arg === 'bigint') return `${arg}n`;
        if (typeof arg === 'symbol') return String(arg);
        if (typeof arg === 'object' && arg !== null) return inspect(arg, 1);
        return Object.is(arg, -0) ? '-0' : String(arg);
      case 'd':
        if (typeof arg === 'bigint') return `${arg}n`;
        if (typeof arg === 'symbol' || (typeof arg === 'object' && arg !== null)) return 'NaN';
        return String(Number(arg));
      case 'i':
        if (typeof arg === 'bigint') return `${arg}n`;
        if (typeof arg === 'symbol') return 'NaN';
        return String(parseInt(arg, 10));
      case 'f':
        if (typeof arg === 'symbol') return 'NaN';
        return String(parseFloat(arg));
      case 'j':
        try {
          return String(JSON.stringify(arg));
        } catch {
          return '[Circular]';
        }
      case 'o':
      case 'O':
        return inspect(arg, 4);
      case 'c':
        return '';
      default:
        return '%' + spec;
    }
  }
  function formatArgs(args) {
    if (args.length === 0) return '';
    const parts = [];
    let start = 0;
    if (typeof args[0] === 'string') {
      if (args.length === 1) return args[0];
      let next = 1;
      parts.push(args[0].replace(/%([sdifjoOc%])/g, (match, spec) => {
        if (spec === '%') return '%';
        if (next >= args.length) return match;
        return formatSpecifier(spec, args[next++]);
      }));
      start = next;
    }
    for (let index = start; index < args.length; index++) {
      parts.push(typeof args[index] === 'string' ? args[index] : inspect(args[index], 4));
    }
    return parts.join(' ');
  }

  // ---- cell state ----
  const WITHHELD_GLOBALS = {
    process: "await import('node:process')", require: "await import('node:<module>')",
    module: 'top-level declarations', exports: 'top-level declarations',
    __filename: 'a workspace-relative path', __dirname: 'a workspace-relative path',
  };
  // QuickJS stacks omit the `Name: message` header that Node prints first; restore it.
  function errorText(error) {
    if (typeof error !== 'object' || error === null || typeof error.message !== 'string') return String(error);
    const header = `${error.name}: ${error.message}`;
    const stack = typeof error.stack === 'string' ? error.stack : '';
    if (stack.startsWith(header)) return stack;
    return stack ? `${header}\n${stack}` : header;
  }
  function evaluationHint(error) {
    const message = String(error?.message ?? '');
    const missing = error?.name === 'ReferenceError' && /^(\w+) is not defined$/.exec(message);
    if (missing && Object.hasOwn(WITHHELD_GLOBALS, missing[1])) {
      return `\n[dial] \`${missing[1]}\` is not a global in eval cells; use ${WITHHELD_GLOBALS[missing[1]]} instead.`;
    }
    if (error?.name === 'SyntaxError' && /^(Identifier '[^']+' has already been declared|redeclaration of '[^']+')$/.test(message)) {
      return '\n[dial] Top-level declarations persist across cells in this namespace; reuse the existing binding, assign to it, or pick a new name.';
    }
    return '';
  }

  // ---- timers: they fire only inside a cell's own event loop, as in the Node driver ----
  const timers = new Map();
  let timerSequence = 0;
  function addTimer(fn, delay, args, repeat) {
    if (typeof fn !== 'function') throw new TypeError('The "callback" argument must be of type function');
    const ms = Math.max(0, Number(delay) || 0);
    const id = ++timerSequence;
    timers.set(id, { id, fn, args, interval: repeat ? Math.max(1, ms) : null, due: Date.now() + ms, owner: current });
    return id;
  }
  function nextTimer() {
    let next;
    for (const timer of timers.values()) if (!next || timer.due < next.due) next = timer;
    return next;
  }
  function fireTimer(timer) {
    if (timer.interval === null) timers.delete(timer.id);
    else timer.due = Date.now() + timer.interval;
    const previous = current;
    current = timer.owner;
    try {
      timer.fn(...timer.args);
    } catch (error) {
      if (timer.owner?.active) timer.owner.stderr.write(errorText(error).slice(-LIMIT) + '\n');
    } finally {
      current = previous;
    }
  }
  // Runs queued jobs and timers until `isDone` holds. Returns false when nothing can make progress.
  // Timers already due when the cell completes run before it does; later timers wait for later cells.
  function runEventLoop(isDone) {
    for (;;) {
      natives.runJobs();
      if (isDone()) break;
      const next = nextTimer();
      if (next === undefined) return false;
      const wait = next.due - Date.now();
      if (wait > 0) natives.sleep(wait);
      fireTimer(next);
    }
    for (;;) {
      natives.runJobs();
      const next = nextTimer();
      if (next === undefined || next.due > Date.now()) return true;
      fireTimer(next);
    }
  }

  function evaluate(code) {
    // Async cells need the completion value captured explicitly: QuickJS drops it for promise-mode eval.
    const isAsync = code.includes('await');
    globalThis.__dial_completion__ = undefined;
    let completion;
    try {
      completion = natives.evaluate(isAsync ? natives.completionSource(code) + '\n' : code + '\n', isAsync);
    } catch (error) {
      return { failed: true, error, value: undefined };
    }
    if (!isAsync) {
      runEventLoop(() => true);
      return { failed: false, value: completion };
    }
    let settled = false;
    let rejected = false;
    let reason;
    completion.then(() => { settled = true; }, error => { settled = true; rejected = true; reason = error; });
    if (!runEventLoop(() => settled)) {
      return { failed: true, error: new Error('top-level await never settled: no pending timer or job can resolve it') };
    }
    if (rejected) return { failed: true, error: reason };
    const value = globalThis.__dial_completion__;
    globalThis.__dial_completion__ = undefined;
    return { failed: false, value };
  }

  function runCell(request) {
    const operation = { id: request.id, active: true, callbacks: 0, stdout: new Capture(), stderr: new Capture() };
    const result = { stdout: '', stderr: '', value: null, error: null };
    current = operation;
    try {
      if (typeof request.code !== 'string') throw new TypeError('code must be a string');
      const outcome = evaluate(request.code);
      if (outcome.failed) throw outcome.error;
      if (outcome.value !== undefined) {
        globalThis._ = outcome.value;
        result.value = inspect(outcome.value, 4).slice(0, LIMIT);
      }
    } catch (error) {
      result.error = (errorText(error) + evaluationHint(error)).slice(-LIMIT);
      if (error?.result !== undefined) result.tool_error = error.result;
    } finally {
      operation.active = false;
      current = null;
    }
    result.stdout = operation.stdout.value();
    result.stderr = operation.stderr.value();
    // These count captured console streams only; native writes and value formatting are not a lossless capture.
    result.output_integrity = {
      original_bytes: operation.stdout.originalBytes + operation.stderr.originalBytes,
      retained_bytes: operation.stdout.retainedBytes + operation.stderr.retainedBytes,
    };
    return result;
  }

  globalThis.tool = tool;
  globalThis.__dial_prelude__ = async (name, parameters) => callback('prelude', { name, parameters }).data;
  globalThis.__dial_check__ = (name, passed) => callback('check', { name, passed });
  globalThis.__dial_display__ = value => {
    callback('display', { text: typeof value === 'string' ? value : inspect(value, 2) });
  };
  globalThis.console = Object.fromEntries(['log', 'info', 'debug', 'warn', 'error', 'dir'].map(name => [name, (...args) => {
    const operation = current;
    if (!operation || !operation.active) return;
    const output = name === 'error' || name === 'warn' ? operation.stderr : operation.stdout;
    output.write(formatArgs(args).slice(0, LIMIT) + '\n');
  }]));
  globalThis.setTimeout = (fn, delay, ...args) => addTimer(fn, delay, args, false);
  globalThis.setInterval = (fn, delay, ...args) => addTimer(fn, delay, args, true);
  globalThis.clearTimeout = id => { timers.delete(id); };
  globalThis.clearInterval = id => { timers.delete(id); };

  for (;;) {
    let request;
    try {
      request = readFrame();
    } catch (error) {
      if (!(error instanceof ToolError)) throw error;
      sendFrame({ id: null, error: error.message });
      continue;
    }
    if (request === null) break;
    const result = runCell(request);
    try {
      sendFrame({ id: request.id, result });
    } catch (error) {
      if (!(error instanceof ToolError)) throw error;
      sendFrame({ id: request.id, result: { stdout: '', stderr: '', value: null, error: error.message.slice(-LIMIT) } });
    }
  }
})();
