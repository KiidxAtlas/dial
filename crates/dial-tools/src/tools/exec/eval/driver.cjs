'use strict';
// Trusted transport driver. vm/repl is namespace management, never a security boundary.
// The containing process has the workspace OS sandbox and mandatory process-fork fence.
const fs = require('node:fs');
const vm = require('node:vm');
// Node 22 REPL only routes truthy awaited rejections to its error domain; falsy reasons (null,
// undefined, 0, "") reach the evaluation callback as a successful cell. Box those reasons on the
// REPL's own await path only, before the REPL captures vm.Script.prototype.runInContext. Unboxed in finish.
class AwaitRejection { constructor(reason) { this.reason = reason; } }
const unwrapRejection = error => (error instanceof AwaitRejection ? error.reason : error);
let awaitingCell = false;
const replRunInContext = vm.Script.prototype.runInContext;
vm.Script.prototype.runInContext = function runInContext(contextified, options) {
  const result = Reflect.apply(replRunInContext, this, arguments);
  if (!awaitingCell || contextified !== context || typeof result?.then !== 'function') return result;
  return result.then(undefined, reason => { throw reason ? reason : new AwaitRejection(reason); });
};
const repl = require('node:repl');
const util = require('node:util');
const { PassThrough, Writable } = require('node:stream');
const { AsyncLocalStorage } = require('node:async_hooks');
if (Number(process.versions.node.split('.')[0]) < 22) {
  throw new Error('eval.node requires operator-installed Node 22 or newer');
}

const LIMIT = 20000;
const MAX_FRAME = 12 * 1024 * 1024;
const MAX_CALLBACKS = 128;
const operationStorage = new AsyncLocalStorage();
const protocol = 3; // The trusted launcher preserves stdout here and routes native stdout to stderr.
let callbackSequence = 0;
let pending = Buffer.allocUnsafe(64 * 1024);
let pendingStart = 0;
let pendingEnd = 0;
let pendingScan = 0;

class Capture {
  constructor() { this.parts = []; this.size = 0; this.dropped = 0; }
  write(text) {
    text = String(text);
    const take = Math.min(text.length, Math.max(0, LIMIT - this.size));
    if (take) this.parts.push(text.slice(0, take));
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
  // One geometrically grown, bounded buffer is reused; input never queues unbounded lines.
  for (;;) {
    const newline = pending.subarray(pendingScan, pendingEnd).indexOf(10);
    if (newline !== -1) {
      const end = pendingScan + newline;
      const text = pending.toString('utf8', pendingStart, end);
      pendingStart = end + 1;
      if (pendingStart === pendingEnd) pendingStart = pendingEnd = 0;
      pendingScan = pendingStart;
      let value;
      try {
        value = JSON.parse(text);
      } catch {
        throw new ToolError('controller frame is not valid JSON');
      }
      if (!value || typeof value !== 'object' || Array.isArray(value)) throw new ToolError('controller frame must be an object');
      return value;
    }
    pendingScan = pendingEnd;
    if (pendingEnd - pendingStart > MAX_FRAME) throw new ToolError('controller frame exceeds the eval limit');
    if (pendingEnd === pending.length) {
      if (pendingStart) {
        pending.copyWithin(0, pendingStart, pendingEnd);
        pendingEnd -= pendingStart;
        pendingScan -= pendingStart;
        pendingStart = 0;
      } else {
        const grown = Buffer.allocUnsafe(Math.min(MAX_FRAME + 1, pending.length * 2));
        pending.copy(grown, 0, 0, pendingEnd);
        pending = grown;
      }
    }
    const size = fs.readSync(0, pending, pendingEnd, Math.min(16384, pending.length - pendingEnd), null);
    if (size === 0) {
      if (pendingEnd !== pendingStart) throw new Error('incomplete controller frame');
      return null;
    }
    pendingEnd += size;
  }
}

function sendFrame(value) {
  // JSON.stringify escapes lone UTF-16 surrogates, but Rust's JSON receiver rejects them.
  // Normalize at the wire boundary, including truncated strings, nested tool values and keys.
  // Consume escaped backslashes and valid surrogate pairs intact ("\\ud800" is literal text).
  const json = JSON.stringify(value).replace(
    /\\(?:u[dD][89aAbB][0-9a-fA-F]{2}\\u[dD][c-fC-F][0-9a-fA-F]{2}|u[dD][89a-fA-F][0-9a-fA-F]{2}|.)/g,
    escape => /^\\u[dD][89a-fA-F][0-9a-fA-F]{2}$/.test(escape) ? '\\ufffd' : escape,
  );
  const bytes = Buffer.from(json + '\n', 'utf8');
  if (bytes.length > MAX_FRAME) throw new ToolError('eval frame exceeds the protocol limit');
  let offset = 0;
  while (offset < bytes.length) offset += fs.writeSync(protocol, bytes, offset, bytes.length - offset);
}

function callback(kind, fields = {}) {
  const operation = operationStorage.getStore();
  if (!operation || !operation.active) throw new ToolError('tool callback belongs to an expired cell');
  if (++operation.callbacks > MAX_CALLBACKS) throw new ToolError('cell callback limit exceeded');
  const callbackId = ++callbackSequence;
  sendFrame({ callback: kind, callback_id: callbackId, operation_id: operation.id, ...fields });
  // Deliberately synchronous. There is no fake Promise-based parallel tool execution.
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
const tool = new Proxy(Object.create(null), {
  get(_target, name) {
    if (name === 'call') return call;
    if (name === 'schemas') {
      const result = callback('schemas');
      if (!Array.isArray(result.data)) throw new ToolError('controller did not return current tool schemas', result);
      return result.data;
    }
    if (name === 'then' || typeof name !== 'string' || name.startsWith('_')) return undefined;
    return (args = {}) => call(name, args);
  },
});

const sink = new Writable({ write(_chunk, _encoding, done) { done(); } });
const input = new PassThrough();
// Node's own REPL evaluator supplies real top-level await and persistent lexical declarations.
// Its context is a real vm.Context, and remains the same for every cell.
const kernel = repl.start({ input, output: sink, terminal: false, prompt: '', useGlobal: false, ignoreUndefined: true, historySize: 0, handleError: handleEvaluationError });
const context = kernel.context;
if (!vm.isContext(context)) throw new Error('Node REPL did not create a vm context');
for (const name of ['require', 'module', 'exports', 'process', '__filename', '__dirname']) delete context[name];
context.tool = tool;
context.__dial_prelude__ = async (name, parameters) => callback('prelude', { name, parameters }).data;
context.__dial_display__ = value => { callback('display', { text: typeof value === 'string' ? value : util.inspect(value) }); };
context.console = Object.fromEntries(['log', 'info', 'debug', 'warn', 'error', 'dir'].map(name => [name, (...args) => {
  const operation = operationStorage.getStore();
  if (!operation || !operation.active) return;
  const output = name === 'error' || name === 'warn' ? operation.stderr : operation.stdout;
  output.write(util.formatWithOptions({ depth: 4, maxArrayLength: 100, maxStringLength: LIMIT, customInspect: false, getters: false }, ...args).slice(0, LIMIT) + '\n');
}]));
context.setTimeout = setTimeout;
context.clearTimeout = clearTimeout;
context.setInterval = setInterval;
context.clearInterval = clearInterval;

process.on('unhandledRejection', error => {
  const operation = operationStorage.getStore();
  if (operation?.active) operation.stderr.write(String(error?.stack || error).slice(-LIMIT) + '\n');
});

// Runtime failures bypass eval's callback. Node 26+ supplies the public handleError option;
// Node 22–25 use the REPL domain instead. Keep the real evaluator in both cases.
let pendingEvaluation = null;
if (kernel._domain) {
  kernel._domain.removeAllListeners('error');
  kernel._domain.on('error', handleEvaluationError);
}
function handleEvaluationError(rawError) {
  const error = unwrapRejection(rawError);
  const operation = operationStorage.getStore();
  if (pendingEvaluation && operation === pendingEvaluation.operation) {
    pendingEvaluation.finish(error, undefined, true);
  } else if (operation?.active) {
    operation.stderr.write(String(error?.stack || error).slice(-LIMIT) + '\n');
  } else {
    // No active cell can own this failure; do not silently report success.
    process.stderr.write(String(error?.stack || error).slice(-LIMIT).toWellFormed() + '\n');
  }
  return 'ignore'; // The error was framed above, not swallowed by the REPL's display sink.
}

function evaluate(code) {
  return new Promise(resolve => {
    // The REPL's default evaluator handles declarations, await and completion values without
    // wrapping cells in disposable async-function scopes or pretending page JS is Node code.
    const operation = operationStorage.getStore();
    const finish = (error, value, failed = error !== null && error !== undefined) => {
      if (!pendingEvaluation || pendingEvaluation.finish !== finish) return;
      pendingEvaluation = null;
      awaitingCell = false;
      resolve({ error: unwrapRejection(error), value, failed });
    };
    pendingEvaluation = { operation, finish };
    awaitingCell = code.includes('await');
    try {
      kernel.eval(code + '\n', context, '<dial-eval>', finish);
    } catch (error) {
      finish(error, undefined, true);
    }
  });
}

(async () => {
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
    const operation = { id: request.id, active: true, callbacks: 0, stdout: new Capture(), stderr: new Capture() };
    const result = { stdout: '', stderr: '', value: null, error: null };
    context.tool = tool;
    try {
      await operationStorage.run(operation, async () => {
        if (typeof request.code !== 'string') throw new TypeError('code must be a string');
        const outcome = await evaluate(request.code);
        if (outcome.failed) throw outcome.error;
        if (outcome.value !== undefined) {
          context._ = outcome.value;
          result.value = util.inspect(outcome.value, { depth: 4, maxArrayLength: 100, maxStringLength: LIMIT, customInspect: false, getters: false }).slice(0, LIMIT);
        }
      });
    } catch (error) {
      result.error = String(error?.stack || error).slice(-LIMIT);
      if (error?.result !== undefined) result.tool_error = error.result;
    } finally {
      // Timers/promises from a previous cell retain this owner, never the next cell's permission.
      operation.active = false;
    }
    result.stdout = operation.stdout.value();
    result.stderr = operation.stderr.value();
    try {
      sendFrame({ id: request.id, result });
    } catch (error) {
      if (!(error instanceof ToolError)) throw error;
      sendFrame({ id: request.id, result: { stdout: '', stderr: '', value: null, error: error.message.slice(-LIMIT) } });
    }
  }
  kernel.close();
})().catch(error => { process.stderr.write(String(error?.stack || error).slice(-LIMIT).toWellFormed()); process.exitCode = 1; });
