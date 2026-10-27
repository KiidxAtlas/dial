// Computer desktop worker. Runs desktop scripts and bridges native desktop
// operations to the host over line-delimited JSON on stdio. Self-contained:
// spawned as `node --input-type=module -e <source>` or as a file.

import { AsyncLocalStorage } from "node:async_hooks";
import { createHash, randomUUID } from "node:crypto";
import { Console } from "node:console";
import * as fsp from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import readline from "node:readline";
import { format } from "node:util";
import { Writable } from "node:stream";
import * as vm from "node:vm";
import * as nodeModule from "node:module";
import { createRequire, isBuiltin } from "node:module";
import { existsSync, mkdtempSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/** Tool-facing failure whose message is shown to the model verbatim. */
class ToolError extends Error {
	constructor(message, options) {
		super(message, options);
		this.name = "ToolError";
	}
}

/** Raised when a tool operation is aborted (signal, timeout, revocation, close). */
class ToolAbortError extends Error {
	constructor(message = "Operation aborted", options) {
		super(message, options);
		this.name = "ToolAbortError";
	}
}

function abortError(signal) {
	return signal.reason instanceof ToolAbortError ? signal.reason : new ToolAbortError(undefined, { cause: signal.reason });
}

function throwIfAborted(signal) {
	if (signal?.aborted) throw abortError(signal);
}

function errorPayload(error) {
	if (error instanceof ToolAbortError) {
		return { name: error.name, message: error.message, stack: error.stack, isToolError: false, isAbort: true };
	}
	if (error instanceof ToolError) {
		return { name: error.name, message: error.message, stack: error.stack, isToolError: true, isAbort: false };
	}
	if (error instanceof Error) {
		return { name: error.name, message: error.message, stack: error.stack, isToolError: false, isAbort: false };
	}
	return { name: "Error", message: String(error), isToolError: false, isAbort: false };
}

function replyError(payload) {
	if (!payload || typeof payload !== "object") return new Error(String(payload));
	if (payload.isAbort) {
		const error = new ToolAbortError(payload.message || "Tool call aborted");
		if (payload.stack) error.stack = payload.stack;
		return error;
	}
	const ErrorType = payload.isToolError ? ToolError : Error;
	const error = new ErrorType(payload.message);
	if (payload.name) error.name = payload.name;
	if (payload.stack) error.stack = payload.stack;
	return error;
}

function nativeError(error) {
	return new ToolError(error instanceof Error ? error.message : String(error));
}

// ---------------------------------------------------------------------------
// Protocol transport (stdout is reserved for protocol lines)
// ---------------------------------------------------------------------------

const protocolOut = process.stdout.write.bind(process.stdout);
// Stray writes from user code must not corrupt the protocol stream.
process.stdout.write = (chunk, ...rest) => process.stderr.write(chunk, ...rest);

function writeLine(message) {
	protocolOut(`${JSON.stringify(message)}\n`);
}

// Native ids carry a per-process nonce so a reply can never settle a request from another worker.
const nativeInstance = randomUUID();
let nativeSeq = 0;
const pendingNative = new Map();

/** Requests one native desktop operation from the host and awaits its result. */
function native(method, args = {}) {
	const id = `native-${nativeInstance}-${++nativeSeq}`;
	return new Promise((resolve, reject) => {
		pendingNative.set(id, { resolve, reject });
		writeLine({ type: "native", id, method, args });
	});
}


function settleNative(message) {
	const entry = pendingNative.get(message.id);
	if (!entry) return;
	pendingNative.delete(message.id);
	if (message.ok) entry.resolve(message.value);
	else entry.reject(replyError(message.error));
}

/** Native session facade: every method forwards to the host's DesktopSession. */
function createNativeSession() {
	return {
		capabilities: () => native("capabilities"),
		listDisplays: () => native("listDisplays"),
		listWindows: diagnostics => native("listWindows", { diagnostics }),
		getWindow: id => native("getWindow", { id }),
		capture: (target, caps) => native("capture", { target, caps }),
		captureRegion: (target, region, caps) => native("captureRegion", { target, region, caps }),
		cancel: reason => native("cancel", { reason }),
		retire: () => native("retire"),
		listApplications: options => native("listApplications", { options }),
		openApplication: (id, options) => native("openApplication", { id, options }),
		menuItems: (target, path) => native("menuItems", { target, path }),
		menuSelect: (target, path, options) => native("menuSelect", { target, path, options }),
		observe: (target, caps, axOptions) => native("observe", { target, caps, axOptions }),
		holdKeys: (target, keys, options) => native("holdKeys", { target, keys, options }),
		holdMouse: (target, x, y, options) => native("holdMouse", { target, x, y, options }),
		acquireControl: () => native("acquireControl"),
		releaseControl: () => native("releaseControl"),
		controlState: () => native("controlState"),
		bringToCurrentSpace: windowId => native("bringToCurrentSpace", { windowId }),
		click: (target, x, y, opts) => native("click", { target, x, y, opts }),
		moveMouse: (target, x, y, opts) => native("moveMouse", { target, x, y, opts }),
		drag: (target, points, opts) => native("drag", { target, path: points, opts }),
		scroll: (target, x, y, dx, dy, opts) => native("scroll", { target, x, y, dx, dy, opts }),
		typeText: (target, text, opts) => native("typeText", { target, text, opts }),
		keyChord: (target, keys, opts) => native("keyChord", { target, keys, opts }),
		raiseWindow: windowId => native("raiseWindow", { windowId }),
		axSnapshot: (target, opts) => native("axSnapshot", { target, opts }),
		axQuery: (target, query) => native("axQuery", { target, query }),
		axElementAt: (target, x, y) => native("axElementAt", { target, x, y }),
		axFocused: () => native("axFocused"),
		axNode: reference => native("axNode", { reference }),
		axAttributes: reference => native("axAttributes", { reference }),
		axChildren: reference => native("axChildren", { reference }),
		axParent: reference => native("axParent", { reference }),
		axPerform: (reference, action) => native("axPerform", { reference, action }),
		axSetValue: (reference, value) => native("axSetValue", { reference, value }),
		axFocus: reference => native("axFocus", { reference }),
		axClick: (reference, opts) => native("axClick", { reference, opts }),
	};
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async function nativeCall(signal, call) {
	throwIfAborted(signal);
	try {
		const value = await call();
		throwIfAborted(signal);
		return value;
	} catch (error) {
		throwIfAborted(signal);
		if (error instanceof ToolAbortError) throw error;
		throw nativeError(error);
	}
}

function pointerOptions(options) {
	const mapped = {};
	if (!options) return mapped;
	if ("button" in options && options.button !== undefined) mapped.button = options.button;
	if ("count" in options && options.count !== undefined) mapped.count = options.count;
	if ("modifiers" in options && options.modifiers !== undefined) mapped.modifiers = options.modifiers;
	if ("keys" in options && options.keys !== undefined) mapped.keys = options.keys;
	if (options.takeover !== undefined) mapped.takeover = options.takeover;
	return mapped;
}

function chordKeys(chord) {
	return typeof chord === "string"
		? chord
				.split("+")
				.map(key => key.trim())
				.filter(Boolean)
		: chord;
}

function validateKeys(value, label, options) {
	if (
		!Array.isArray(value) ||
		(!options?.allowEmpty && value.length === 0) ||
		value.some(key => typeof key !== "string" || !key.trim())
	) {
		throw new ToolError(`${label} requires a non-empty array of non-empty strings`);
	}
}

function validateHold(options) {
	if (
		!options ||
		typeof options.duration !== "number" ||
		!Number.isFinite(options.duration) ||
		options.duration < 0 ||
		options.duration > 100
	) {
		throw new ToolError("duration must be seconds in the range 0..100");
	}
}

function strictObject(value, allowed, label) {
	if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some(key => !allowed.includes(key))) {
		throw new ToolError(`${label} must be an object with only: ${allowed.join(", ")}`);
	}
}

function matchesFilter(window, filter) {
	if (!filter) return true;
	const app = filter.app?.toLocaleLowerCase();
	const title = filter.title?.toLocaleLowerCase();
	return (
		(filter.id === undefined || window.id === String(filter.id)) &&
		(!app || window.app.toLocaleLowerCase().includes(app)) &&
		(!title || window.title.toLocaleLowerCase().includes(title))
	);
}

let windowSnapshotSeq = 0;

/** Lists windows and stamps the snapshot so resolution errors can be compared across calls and workers. */
async function listWindowSnapshot(session, signal, runtimeIdentity) {
	const listing = await nativeCall(signal, () => session.listWindows(true));
	return {
		...listing,
		seq: ++windowSnapshotSeq,
		at: new Date().toISOString(),
		worker: `${process.pid}/${nativeInstance.slice(0, 8)}`,
		runtime: runtimeIdentity ?? "unreported build",
	};
}

/** Explains what a window listing can see, so an absent window is not mistaken for a closed one. */
function describeWindowSnapshot(snapshot) {
	const scope =
		process.platform === "darwin"
			? "on-screen shareable windows only; hidden, minimized or other-Space windows may be absent"
			: "top-level windows the native backend reports; hidden or minimized windows may be absent";
	const completeness = snapshot.truncated
		? `truncated at ${snapshot.limit} windows; explicit window(id) lookup bypasses this listing cap`
		: "not truncated";
	return `(window snapshot #${snapshot.seq} at ${snapshot.at} from worker ${snapshot.worker} listed ${snapshot.windows.length} window(s); ${completeness}; runtime: ${snapshot.runtime}, ${process.version}; scope: ${scope}. windows() and window(filter) share this source; window(id) uses targeted native lookup with the same safety filters; compare snapshot sequence, worker identity and runtime build when investigating changes.)`;
}

/** Points scripts that use the Eval-only `computer` global at the run-scope `desktop` facade. */
function hintWrongGlobal(error) {
	if (error?.name === "ReferenceError" && error.message === "computer is not defined") {
		error.message +=
			"; direct computer run code uses the `desktop` global (for example `await desktop.windows()`). `computer.*` exists only in Eval code.";
	}
	return error;
}

function guardRun(context, method) {
	if (context.readOnly) throw new ToolError(`read-only run: '${method}' requires read_only: false`);
	throwIfAborted(context.signal);
}

function frameBytes(data) {
	return typeof data === "string" ? Buffer.from(data, "base64") : Buffer.from(data);
}

function untilAborted(signal, fn) {
	return new Promise((resolve, reject) => {
		if (signal.aborted) {
			reject(abortError(signal));
			return;
		}
		const onAbort = () => reject(abortError(signal));
		signal.addEventListener("abort", onAbort, { once: true });
		Promise.resolve()
			.then(fn)
			.then(
				value => {
					signal.removeEventListener("abort", onAbort);
					resolve(value);
				},
				error => {
					signal.removeEventListener("abort", onAbort);
					reject(error);
				},
			);
	});
}

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

/** Clone-safe output buffering: stream text joins the entries at the next push or on finish. */
class RunOutput {
	#displays = [];
	#textBuffer = "";

	pushText(chunk) {
		this.#textBuffer += chunk;
	}

	pushDisplay(output) {
		if (output.type === "image") {
			this.push({ type: "image", data: output.data, mimeType: output.mimeType });
			return;
		}
		if (output.type === "json") {
			this.push({ type: "text", text: safeJsonStringify(output.data) });
			return;
		}
		this.push({ type: "text", text: safeJsonStringify(output.event) });
	}

	push(entry) {
		this.#flush();
		this.#displays.push(entry);
	}

	finish() {
		this.#flush();
		return this.#displays;
	}

	#flush() {
		if (!this.#textBuffer) return;
		this.#displays.push({ type: "text", text: this.#textBuffer.replace(/\n$/, "") });
		this.#textBuffer = "";
	}
}

function safeJsonStringify(value) {
	try {
		return JSON.stringify(value, null, 2);
	} catch {
		return String(value);
	}
}

/** Passes a return value across the run boundary: structured-cloneable as-is, else JSON round-trip, else String. */
function cloneSafe(value) {
	if (value === undefined) return undefined;
	try {
		structuredClone(value);
		return value;
	} catch {}
	try {
		return JSON.parse(JSON.stringify(value));
	} catch {}
	return String(value);
}

const CELL_BUDGET_SLACK_MS = 1_000;
const DEFAULT_PREDICATE_TIMEOUT_MS = 30_000;

/** Effective `wait(predicate)` deadline strictly below the cell budget. */
function resolvePredicateTimeout(cellTimeoutMs, explicit) {
	const budgetBound = Math.max(1, cellTimeoutMs - CELL_BUDGET_SLACK_MS);
	if (explicit === 0 || explicit === Number.POSITIVE_INFINITY) return budgetBound;
	if (explicit !== undefined && Number.isFinite(explicit) && explicit > 0) return Math.min(explicit, budgetBound);
	return Math.min(DEFAULT_PREDICATE_TIMEOUT_MS, budgetBound);
}

/** Marks a promise observed so run-teardown rejections never reach the process-level handler. */
function markHandled(promise) {
	void promise.catch(() => undefined);
	return promise;
}

/** Run-scoped `wait()`: sleeps for a number of ms, or polls a predicate until truthy. */
function waitForRun(msOrPredicate, signal, opts) {
	return markHandled(
		(async () => {
			throwIfAborted(signal);
			if (typeof msOrPredicate === "number") {
				await untilAborted(signal, () => sleep(msOrPredicate));
				throwIfAborted(signal);
				return undefined;
			}
			if (typeof msOrPredicate !== "function") {
				throw new ToolError("wait(...) expects milliseconds (number) or a predicate function to poll");
			}
			const timeout =
				opts?.timeout !== undefined && Number.isFinite(opts.timeout) && opts.timeout > 0
					? opts.timeout
					: DEFAULT_PREDICATE_TIMEOUT_MS;
			const interval = Math.max(opts?.interval ?? 100, 10);
			const deadline = Date.now() + timeout;
			for (;;) {
				const value = await untilAborted(signal, async () => await msOrPredicate());
				throwIfAborted(signal);
				if (value) return value;
				if (Date.now() + interval > deadline) {
					throw new ToolError(`wait(predicate) timed out after ${timeout}ms — predicate never returned truthy`);
				}
				await untilAborted(signal, () => sleep(interval));
			}
		})(),
	);
}

/** Binds a long-lived scope facade to one run's abort signal. */
function bindRunFacade(target, signal) {
	const cache = new Map();
	return new Proxy(target, {
		get(current, prop) {
			throwIfAborted(signal);
			const cached = cache.get(prop);
			if (cached) return cached;
			const value = Reflect.get(current, prop, current);
			if (typeof value === "function") {
				const wrapped = (...args) => {
					throwIfAborted(signal);
					const result = Reflect.apply(value, current, args);
					if (result && typeof result === "object" && typeof Reflect.get(result, "then") === "function") {
						return markHandled(
							Promise.resolve(result).then(resolved => {
								throwIfAborted(signal);
								return resolved;
							}),
						);
					}
					throwIfAborted(signal);
					return result;
				};
				cache.set(prop, wrapped);
				return wrapped;
			}
			if (value && typeof value === "object") {
				// AbortSignals are native-branded; a Proxy cannot forward their internal slots.
				if (value instanceof AbortSignal) return value;
				const wrapped = bindRunFacade(value, signal);
				cache.set(prop, wrapped);
				return wrapped;
			}
			return value;
		},
	});
}

// ---------------------------------------------------------------------------
// Script runtime: per-run globals and compiled run body
// ---------------------------------------------------------------------------

function displayValue(value, hooks) {
	if (value === undefined) return;
	if (value !== null && typeof value === "object" && value.type === "image" && typeof value.data === "string" && typeof value.mimeType === "string") {
		hooks.onDisplay({ type: "image", data: value.data, mimeType: value.mimeType });
		return;
	}
	if (value === null || (typeof value !== "object" && typeof value !== "function")) {
		hooks.onText(`${String(value)}\n`);
		return;
	}
	try {
		hooks.onDisplay({ type: "json", data: structuredClone(value) });
	} catch {
		hooks.onText(`${Object.prototype.toString.call(value)}\n`);
	}
}

/** Per-run hooks and final-value slot, reached from the globals installed below. */
const runStore = new AsyncLocalStorage();

function warnOutsideRun(name) {
	process.stderr.write(`computer: ${name} called outside an active run\n`);
}

function emitText(prefix, args) {
	const run = runStore.getStore();
	if (!run) return warnOutsideRun("console");
	const text = `${prefix}${format(...args)}`;
	run.hooks.onText(text.endsWith("\n") ? text : `${text}\n`);
}

function consoleTable(...args) {
	const run = runStore.getStore();
	if (!run) return warnOutsideRun("console.table");
	const sink = new Writable({
		write(chunk, _encoding, callback) {
			run.hooks.onText(chunk.toString());
			callback();
		},
	});
	new Console({ stdout: sink, stderr: sink }).table(...args);
}

const runConsole = {
	log: (...args) => emitText("", args),
	info: (...args) => emitText("", args),
	debug: (...args) => emitText("", args),
	warn: (...args) => emitText("[warn] ", args),
	error: (...args) => emitText("[error] ", args),
	table: (...args) => consoleTable(...args),
};

// Per-run globals route to the active run's hooks; installed once per worker, like the prelude they replace.
Object.assign(globalThis, {
	console: runConsole,
	print: (...args) => emitText("", args),
	display: value => {
		const run = runStore.getStore();
		if (!run) return warnOutsideRun("display");
		displayValue(value, run.hooks);
	},
	tool: (name, args) => {
		const run = runStore.getStore();
		if (!run) throw new ToolError("no active computer run");
		return run.hooks.callTool(name, args);
	},
	__dial_set_final_expr__: value => {
		const run = runStore.getStore();
		if (!run) throw new ToolError("no active computer run");
		run.final = { value };
	},
});

const STATEMENT_KEYWORDS = new Set([
	"var", "let", "const", "function", "class", "if", "for", "while", "do", "switch", "try", "throw", "with", "debugger", "break", "continue",
]);
const CONTINUATION_BEFORE = new Set(["", "(", ",", "=", "+", "-", "*", "/", "%", "&", "|", "^", "!", "?", ":", ".", "<", ">", "[", "{", "~"]);
const CONTINUATION_AFTER = new Set([".", "?", ")", "]", ",", ":", "=", "+", "-", "*", "/", "%", "&", "|", "^", "<", ">"]);

/** Lexes JS into tokens. Templates split around `${ … }`; regex literals are chosen by the previous significant token. */
function tokenize(code) {
	const tokens = [];
	const braces = [];
	const n = code.length;
	let newline = false;
	let i = 0;
	const push = (t, v, s, e) => {
		const token = { t, v, s, e, nl: newline };
		newline = false;
		tokens.push(token);
		return token;
	};
	const readTemplate = from => {
		let j = from;
		while (j < n) {
			const c = code[j];
			if (c === "\\") j += 2;
			else if (c === "`") return { end: j + 1, open: false };
			else if (c === "$" && code[j + 1] === "{") return { end: j + 2, open: true };
			else j++;
		}
		return { end: n, open: false };
	};
	while (i < n) {
		const ch = code[i];
		if (ch === "\n" || ch === "\r" || ch === "\u2028" || ch === "\u2029") {
			newline = true;
			i++;
			continue;
		}
		if (/\s/.test(ch)) {
			i++;
			continue;
		}
		if (ch === "/" && code[i + 1] === "/") {
			const end = code.indexOf("\n", i);
			i = end === -1 ? n : end;
			continue;
		}
		if (ch === "/" && code[i + 1] === "*") {
			const end = code.indexOf("*/", i + 2);
			const stop = end === -1 ? n : end + 2;
			if (/[\n\r\u2028\u2029]/.test(code.slice(i, stop))) newline = true;
			i = stop;
			continue;
		}
		if (ch === '"' || ch === "'") {
			let j = i + 1;
			while (j < n && code[j] !== ch) j += code[j] === "\\" ? 2 : 1;
			const end = Math.min(j + 1, n);
			push("str", code.slice(i + 1, Math.min(j, n)), i, end);
			i = end;
			continue;
		}
		if (ch === "`") {
			const chunk = readTemplate(i + 1);
			const token = push("tpl", "", i, chunk.end);
			token.head = true;
			token.open = chunk.open;
			if (chunk.open) braces.push("tpl");
			i = chunk.end;
			continue;
		}
		if (ch === "}" && braces.at(-1) === "tpl") {
			braces.pop();
			const chunk = readTemplate(i + 1);
			const token = push("tpl", "", i, chunk.end);
			token.head = false;
			token.open = chunk.open;
			if (chunk.open) braces.push("tpl");
			i = chunk.end;
			continue;
		}
		IDENT_TOKEN_RE.lastIndex = i;
		const ident = IDENT_TOKEN_RE.exec(code);
		if (ident) {
			push("id", ident[0], i, i + ident[0].length);
			i += ident[0].length;
			continue;
		}
		if (/[0-9]/.test(ch) || (ch === "." && /[0-9]/.test(code[i + 1] ?? ""))) {
			NUMBER_TOKEN_RE.lastIndex = i;
			const num = NUMBER_TOKEN_RE.exec(code);
			push("num", num[0], i, i + num[0].length);
			i += num[0].length;
			continue;
		}
		if (ch === "/") {
			const prev = tokens.at(-1);
			const regexAllowed =
				!prev ||
				(prev.t === "p" && !")]}".includes(prev.v)) ||
				(prev.t === "id" && REGEX_AFTER_KEYWORDS.has(prev.v)) ||
				(prev.t === "tpl" && prev.open);
			const end = regexAllowed ? regexLiteralEnd(code, i) : -1;
			if (end !== -1) {
				push("re", code.slice(i, end), i, end);
				i = end;
				continue;
			}
		}
		push("p", ch, i, i + 1);
		if (ch === "{") braces.push("{");
		else if (ch === "}") braces.pop();
		i++;
	}
	return tokens;
}

function regexLiteralEnd(code, start) {
	let j = start + 1;
	let inClass = false;
	while (j < code.length) {
		const c = code[j];
		if (c === "\n" || c === "\r") return -1;
		if (c === "\\") {
			j += 2;
			continue;
		}
		if (inClass) {
			if (c === "]") inClass = false;
		} else if (c === "[") inClass = true;
		else if (c === "/") {
			j++;
			while (j < code.length && /[\w$]/.test(code[j])) j++;
			return j;
		}
		j++;
	}
	return -1;
}

function isPunct(tok, v) {
	return tok?.t === "p" && tok.v === v;
}

function isWord(tok, v) {
	return tok?.t === "id" && tok.v === v;
}

function tokenOpens(tok) {
	if (tok.t === "p") return tok.v === "(" || tok.v === "[" || tok.v === "{" ? 1 : 0;
	if (tok.t === "tpl") return tok.open ? 1 : 0;
	return 0;
}

function tokenCloses(tok) {
	if (tok.t === "p") return tok.v === ")" || tok.v === "]" || tok.v === "}" ? 1 : 0;
	if (tok.t === "tpl") return tok.head ? 0 : 1;
	return 0;
}

function continuesStatement(prev, tok) {
	const before = prev.t === "p" ? prev.v : "id";
	return CONTINUATION_BEFORE.has(before) || (tok.t === "p" && CONTINUATION_AFTER.has(tok.v));
}

/** Top-level statements as token index ranges. `last` is inclusive and keeps a trailing `;`. */
function topLevelStatements(tokens) {
	const statements = [];
	let depth = 0;
	let first = 0;
	for (let k = 0; k < tokens.length; k++) {
		const tok = tokens[k];
		const prev = tokens[k - 1];
		if (k > 0 && depth === 0 && (isPunct(prev, ";") || (tok.nl && !continuesStatement(prev, tok)))) {
			statements.push({ first, last: k - 1 });
			first = k;
		}
		depth += tokenOpens(tok) - tokenCloses(tok);
	}
	if (tokens.length > 0) statements.push({ first, last: tokens.length - 1 });
	return statements;
}

/** Start offsets of top-level statements: after `;`, or at a newline that ends a statement (ASI). */
function topLevelStatementStarts(code) {
	const tokens = tokenize(code);
	const starts = [0];
	for (const st of topLevelStatements(tokens)) if (st.first > 0) starts.push(tokens[st.first].s);
	return starts;
}

/** Rewrites the trailing statement into a final-value capture. Null when nothing is captured. */
function rewriteFinalExpression(code) {
	const starts = topLevelStatementStarts(code);
	for (let k = starts.length - 1; k >= 0; k--) {
		const end = k + 1 < starts.length ? starts[k + 1] : code.length;
		const statement = code.slice(starts[k], end).trim().replace(/;+$/, "").trim();
		if (!statement) continue;
		const prefix = code.slice(0, starts[k]);
		const suffix = code.slice(end);
		const keyword = /^[A-Za-z_$][\w$]*/.exec(statement)?.[0] ?? "";
		if (keyword === "return") {
			const expr = statement.slice("return".length).trim();
			return expr ? `${prefix}__dial_set_final_expr__((${expr}));${suffix}` : null;
		}
		if (STATEMENT_KEYWORDS.has(keyword) || statement.startsWith("{")) return null;
		if (keyword === "async" && /^async\s+function\b/.test(statement)) return null;
		if (keyword === "import" && !/^import\s*[(.]/.test(statement)) return null;
		return `${prefix}__dial_set_final_expr__((${statement}));${suffix}`;
	}
	return null;
}

function isScriptSource(source) {
	try {
		new vm.Script(source);
		return true;
	} catch {
		return false;
	}
}

function compiles(source) {
	if (isScriptSource(source)) return true;
	try {
		new AsyncFunction(source);
		return true;
	} catch {
		return false;
	}
}

const IDENT_TOKEN_RE = /[\p{ID_Start}$_][\p{ID_Continue}$\u200c\u200d]*/uy;
const NUMBER_TOKEN_RE = /(?:0[xXoObB][\da-fA-F_]+n?|(?:\d[\d_]*\.?[\d_]*|\.\d[\d_]*)(?:[eE][+-]?\d+)?n?)/y;
const REGEX_AFTER_KEYWORDS = new Set(["return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else", "yield", "await"]);
const LOCAL_SPECIFIER_RE = /^(\.{1,2}(\/|$)|\/)/;
const WINDOWS_DRIVE_RE = /^[A-Za-z]:[\\/]/;
const LOCAL_MODULE_EXTENSIONS = new Set([".js", ".jsx", ".mjs", ".ts", ".tsx", ".mts"]);
const MODULE_PROBE_EXTENSIONS = [".ts", ".tsx", ".mts", ".js", ".mjs", ".cjs"];
// Dynamic import() calls are routed through the session-cwd helper; falls back to native import() in foreign realms.
const DYNAMIC_IMPORT_CALLEE = '(typeof __dial_import__ === "function" ? __dial_import__ : (s, o) => import(s, o))';
// Same gate as the TypeScript-detection heuristic: only code matching a type-only construct goes through the TypeScript stripper.
const LOOKS_LIKE_TS = /(?:\bimport\s+type\b|\bexport\s+type\b|\b(?:import|export)\s*\{[^}\n]*\btype\s+\w|\binterface\s+\w|\btype\s+\w+\s*=|\b(?:as|satisfies)\s+(?:[A-Z]|\bconst\b)|:\s*(?:string|number|boolean|any|unknown|void|never|object|[A-Z]\w*)\b|<\s*[A-Z]\w*\s*[,>])/;

/** Strips TypeScript syntax with Node's built-in stripper; returns the input unchanged when it fails to transpile. */
function stripTypeScript(code, options = {}) {
	if (!options.force && !LOOKS_LIKE_TS.test(code)) return code;
	if (typeof nodeModule.stripTypeScriptTypes !== "function") {
		throw new ToolError(`TypeScript syntax requires Node 22.13+ (module.stripTypeScriptTypes); running ${process.version}`);
	}
	try {
		return nodeModule.stripTypeScriptTypes(code, { mode: "transform" });
	} catch (error) {
		if (options.force) throw new ToolError(`cannot strip TypeScript from ${options.file ?? "module"}: ${error.message}`);
		return code;
	}
}

/** Applies non-overlapping token-range edits; insertions at the same offset keep their push order. */
function applyEdits(code, edits) {
	if (edits.length === 0) return code;
	edits.sort((a, b) => a.s - b.s);
	let out = "";
	let at = 0;
	for (const edit of edits) {
		out += code.slice(at, edit.s) + edit.text;
		at = edit.e;
	}
	return out + code.slice(at);
}

function isRestToken(tokens, p) {
	return isPunct(tokens[p], ".") && isPunct(tokens[p + 1], ".") && isPunct(tokens[p + 2], ".");
}

/** Skips an expression from `p` until a depth-0 token in `stops` or `limit`. */
function skipExpression(tokens, p, stops, limit) {
	let depth = 0;
	while (p < limit) {
		const tok = tokens[p];
		if (depth === 0 && tok.t === "p" && stops.includes(tok.v)) return p;
		depth += tokenOpens(tok) - tokenCloses(tok);
		p++;
	}
	return p;
}

/** Binding names of the pattern starting at token `k`; `end` is the index after the pattern. */
function bindingPattern(tokens, k) {
	const tok = tokens[k];
	if (tok?.t === "id") return { names: [tok.v], end: k + 1 };
	const names = [];
	let p = k + 1;
	if (isPunct(tok, "[")) {
		while (p < tokens.length && !isPunct(tokens[p], "]")) {
			if (isPunct(tokens[p], ",")) {
				p++;
				continue;
			}
			if (isRestToken(tokens, p)) p += 3;
			const sub = bindingPattern(tokens, p);
			names.push(...sub.names);
			p = Math.max(sub.end, p + 1);
			if (isPunct(tokens[p], "=")) p = skipExpression(tokens, p + 1, [",", "]"], tokens.length);
		}
		return { names, end: p + 1 };
	}
	if (isPunct(tok, "{")) {
		while (p < tokens.length && !isPunct(tokens[p], "}")) {
			if (isPunct(tokens[p], ",")) {
				p++;
				continue;
			}
			if (isRestToken(tokens, p)) {
				const sub = bindingPattern(tokens, p + 3);
				names.push(...sub.names);
				p = Math.max(sub.end, p + 4);
				continue;
			}
			const key = tokens[p];
			if (isPunct(key, "[")) p = skipExpression(tokens, p + 1, ["]"], tokens.length) + 1;
			else p++;
			if (isPunct(tokens[p], ":")) {
				const sub = bindingPattern(tokens, p + 1);
				names.push(...sub.names);
				p = Math.max(sub.end, p + 1);
			} else if (key.t === "id") names.push(key.v);
			if (isPunct(tokens[p], "=")) p = skipExpression(tokens, p + 1, [",", "}"], tokens.length);
		}
		return { names, end: p + 1 };
	}
	return { names, end: k };
}

/** Publishes a top-level const/let/var declaration as globalThis assignments; returns replacement text and pushes bound names. */
function publishedDeclaration(code, tokens, keyword, start, limit, names) {
	const parts = [];
	let p = start;
	while (p < limit) {
		const pattern = bindingPattern(tokens, p);
		const patternStart = p;
		names.push(...pattern.names);
		p = pattern.end;
		let init = null;
		if (isPunct(tokens[p], "=")) {
			const from = p + 1;
			p = skipExpression(tokens, from, [","], limit);
			init = code.slice(tokens[from].s, tokens[p - 1].e);
		}
		if (tokens[patternStart].t === "id") {
			const name = JSON.stringify(pattern.names[0]);
			if (init !== null) parts.push(`globalThis[${name}] = (${init});`);
			else if (keyword === "let") parts.push(`globalThis[${name}] = undefined;`);
		} else if (init !== null) {
			const target = code.slice(tokens[patternStart].s, tokens[pattern.end - 1].e);
			parts.push(`;(${target} = (${init}));`);
		}
		if (isPunct(tokens[p], ",")) p++;
		else break;
	}
	return parts.join(" ");
}

/** Demotes top-level lexical bindings. Sync: const/let become var and class becomes var. Publish: bindings are stored on globalThis. */
function demoteTopLevelLexicals(code, publish) {
	const tokens = tokenize(code);
	const statements = topLevelStatements(tokens);
	const edits = [];
	const names = [];
	const hoisted = [];
	let insertAt = 0;
	let directives = true;
	for (const st of statements) {
		const first = tokens[st.first];
		const hasSemi = isPunct(tokens[st.last], ";");
		const lastNoSemi = hasSemi ? st.last - 1 : st.last;
		if (directives) {
			if (first.t === "str" && lastNoSemi === st.first) insertAt = tokens[st.last].e;
			else directives = false;
		}
		const segStart = first.s;
		const segEnd = tokens[lastNoSemi].e;
		const keyword = first.t === "id" ? first.v : "";
		if (keyword === "const" || keyword === "let" || keyword === "var") {
			if (!publish) {
				if (keyword !== "var") edits.push({ s: first.s, e: first.e, text: "var" });
				continue;
			}
			const text = publishedDeclaration(code, tokens, keyword, st.first + 1, lastNoSemi + 1, names);
			edits.push({ s: segStart, e: tokens[st.last].e, text });
		} else if (keyword === "class") {
			const nameTok = tokens[st.first + 1];
			if (nameTok?.t !== "id") continue;
			if (!publish) {
				edits.push({ s: first.s, e: first.e, text: `var ${nameTok.v} = class` });
				if (!hasSemi) edits.push({ s: tokens[st.last].e, e: tokens[st.last].e, text: ";" });
			} else {
				names.push(nameTok.v);
				const segment = code.slice(segStart, segEnd);
				edits.push({ s: segStart, e: tokens[st.last].e, text: `globalThis[${JSON.stringify(nameTok.v)}] = (${segment});` });
			}
		} else if (publish && (keyword === "function" || (keyword === "async" && isWord(tokens[st.first + 1], "function")))) {
			let nameIdx = st.first + (keyword === "async" ? 2 : 1);
			if (isPunct(tokens[nameIdx], "*")) nameIdx++;
			const nameTok = tokens[nameIdx];
			if (nameTok?.t !== "id") continue;
			names.push(nameTok.v);
			const expression = code.slice(segStart, nameTok.s) + code.slice(nameTok.e, segEnd);
			hoisted.push(`globalThis[${JSON.stringify(nameTok.v)}] = (${expression});`);
			edits.push({ s: segStart, e: tokens[st.last].e, text: "" });
		}
	}
	if (!publish) return applyEdits(code, edits);
	const prelude = [...new Set(names)].map(name => `if (!(${JSON.stringify(name)} in globalThis)) globalThis[${JSON.stringify(name)}] = undefined;`);
	const head = [...prelude, ...hoisted].join("\n");
	if (head) edits.unshift({ s: insertAt, e: insertAt, text: `${head}\n` });
	return applyEdits(code, edits);
}

/** Parses one static import declaration whose `import` token is at index `k`; null when the syntax is not recognized. */
function parseImportDeclaration(tokens, k) {
	let p = k + 1;
	let defaultName;
	let namespaceName;
	const namedPairs = [];
	let source;
	if (tokens[p]?.t === "str") {
		source = tokens[p];
		p++;
	} else {
		if (tokens[p]?.t === "id") {
			defaultName = tokens[p].v;
			p++;
			if (isPunct(tokens[p], ",")) p++;
		}
		if (isPunct(tokens[p], "*")) {
			if (!isWord(tokens[p + 1], "as") || tokens[p + 2]?.t !== "id") return null;
			namespaceName = tokens[p + 2].v;
			p += 3;
		} else if (isPunct(tokens[p], "{")) {
			p++;
			for (;;) {
				if (isPunct(tokens[p], "}")) {
					p++;
					break;
				}
				const first = tokens[p];
				if (first?.t !== "id" && first?.t !== "str") return null;
				p++;
				let local = first.v;
				if (isWord(tokens[p], "as")) {
					if (tokens[p + 1]?.t !== "id") return null;
					local = tokens[p + 1].v;
					p += 2;
				} else if (first.t === "str") return null;
				const imported = first.t === "str" ? JSON.stringify(first.v) : first.v;
				namedPairs.push([imported, local]);
				if (isPunct(tokens[p], ",")) p++;
				else if (!isPunct(tokens[p], "}")) return null;
			}
		}
		if (!isWord(tokens[p], "from") || tokens[p + 1]?.t !== "str") return null;
		source = tokens[p + 1];
		p += 2;
	}
	const attributes = [];
	if ((isWord(tokens[p], "with") || isWord(tokens[p], "assert")) && isPunct(tokens[p + 1], "{")) {
		p += 2;
		for (;;) {
			if (isPunct(tokens[p], "}")) {
				p++;
				break;
			}
			const key = tokens[p];
			if (key?.t !== "id" && key?.t !== "str") return null;
			if (!isPunct(tokens[p + 1], ":") || tokens[p + 2]?.t !== "str") return null;
			attributes.push(`${key.t === "str" ? JSON.stringify(key.v) : key.v}: ${JSON.stringify(tokens[p + 2].v)}`);
			p += 3;
			if (isPunct(tokens[p], ",")) p++;
		}
	}
	const last = isPunct(tokens[p], ";") ? p : p - 1;
	const text = importStatementText(importCallSource(source.v, attributes), defaultName, namespaceName, namedPairs);
	return { index: last, end: tokens[last].e, text };
}

/** Rewrites static imports and dynamic import() calls so they resolve through the session loader; strings, templates, and comments are untouched. */
function rewriteImports(code) {
	if (!code.includes("import")) return code;
	const tokens = tokenize(code);
	const edits = [];
	for (let k = 0; k < tokens.length; k++) {
		const tok = tokens[k];
		if (!isWord(tok, "import") || isPunct(tokens[k - 1], ".")) continue;
		if (isPunct(tokens[k + 1], "(")) {
			edits.push({ s: tok.s, e: tok.e, text: DYNAMIC_IMPORT_CALLEE });
			continue;
		}
		const prev = tokens[k - 1];
		const atStatementStart = !prev || isPunct(prev, ";") || isPunct(prev, "{") || isPunct(prev, "}") || tok.nl;
		if (!atStatementStart || isPunct(tokens[k + 1], ".")) continue;
		const decl = parseImportDeclaration(tokens, k);
		if (!decl) continue;
		edits.push({ s: tok.s, e: decl.end, text: decl.text });
		k = decl.index;
	}
	return applyEdits(code, edits);
}

/** Replaces the specifier of each static import/export-from in a module with `mapSpec(specifier)`. */
function rewriteStaticSpecifiers(code, mapSpec) {
	const tokens = tokenize(code);
	const edits = [];
	for (let k = 1; k < tokens.length; k++) {
		const tok = tokens[k];
		if (tok.t !== "str") continue;
		if (!isWord(tokens[k - 1], "from") && !isWord(tokens[k - 1], "import")) continue;
		edits.push({ s: tok.s, e: tok.e, text: JSON.stringify(mapSpec(tok.v)) });
	}
	return applyEdits(code, edits);
}

/** Rewrites `import.meta` to a literal object and dynamic import() to a loader bound to the module's directory. */
function rewriteModuleSyntax(code, meta, dir) {
	const tokens = tokenize(code);
	const edits = [];
	for (let k = 0; k < tokens.length; k++) {
		const tok = tokens[k];
		if (!isWord(tok, "import") || isPunct(tokens[k - 1], ".")) continue;
		if (isPunct(tokens[k + 1], "(")) {
			edits.push({ s: tok.s, e: tok.e, text: `__dial_import_from__.bind(null, ${JSON.stringify(dir)})` });
		} else if (isPunct(tokens[k + 1], ".") && isWord(tokens[k + 2], "meta")) {
			edits.push({ s: tok.s, e: tokens[k + 2].e, text: `(${JSON.stringify(meta)})` });
		}
	}
	return applyEdits(code, edits);
}

let moduleRoot;

/** Per-process root holding materialized module graphs; removed on exit. */
function moduleRootDir() {
	if (!moduleRoot) {
		moduleRoot = mkdtempSync(path.join(os.tmpdir(), "dial-computer-modules-"));
		process.once("exit", () => rmSync(moduleRoot, { recursive: true, force: true }));
	}
	return moduleRoot;
}

function isManagedLocalModule(file) {
	return LOCAL_MODULE_EXTENSIONS.has(path.extname(file)) && !file.split(path.sep).includes("node_modules");
}

function resolveModuleFile(candidate) {
	const typed = candidate.replace(/\.[cm]?js$/, "");
	const attempts = [
		candidate,
		...MODULE_PROBE_EXTENSIONS.map(ext => candidate + ext),
		...MODULE_PROBE_EXTENSIONS.map(ext => path.join(candidate, `index${ext}`)),
	];
	if (typed !== candidate) attempts.push(`${typed}.ts`, `${typed}.tsx`, `${typed}.mts`);
	for (const attempt of attempts) {
		try {
			if (statSync(attempt).isFile()) return attempt;
		} catch {
			// Not a candidate; try the next one.
		}
	}
	return candidate;
}

const ESM_CONDITIONS = new Set(["import", "node", "default"]);

/** First ESM-matching target of a package `exports` value; null when no condition matches. */
function resolveConditionalTarget(target) {
	if (typeof target === "string") return target;
	if (Array.isArray(target)) {
		for (const option of target) {
			const resolved = resolveConditionalTarget(option);
			if (resolved) return resolved;
		}
		return null;
	}
	if (target === null || typeof target !== "object") return null;
	for (const [condition, value] of Object.entries(target)) {
		if (!ESM_CONDITIONS.has(condition)) continue;
		const resolved = resolveConditionalTarget(value);
		if (resolved) return resolved;
	}
	return null;
}

/** Resolves a package `exports` subpath (".", "./feature") under ESM conditions, including `*` patterns. */
function resolveExportsSubpath(exportsField, subpath) {
	const isSubpathMap =
		typeof exportsField === "object" &&
		exportsField !== null &&
		!Array.isArray(exportsField) &&
		Object.keys(exportsField).some(key => key.startsWith("."));
	const map = isSubpathMap ? exportsField : { ".": exportsField };
	if (Object.hasOwn(map, subpath)) return resolveConditionalTarget(map[subpath]);
	for (const [key, value] of Object.entries(map)) {
		const star = key.indexOf("*");
		if (star === -1) continue;
		const prefix = key.slice(0, star);
		const suffix = key.slice(star + 1);
		if (subpath.length < prefix.length + suffix.length || !subpath.startsWith(prefix) || !subpath.endsWith(suffix)) continue;
		const match = subpath.slice(prefix.length, subpath.length - suffix.length);
		const target = resolveConditionalTarget(value);
		return target === null ? null : target.replaceAll("*", match);
	}
	return null;
}

/** Resolves a bare specifier from `baseDir`: ESM `exports` first, then CJS resolution for packages with no `exports`. */
function resolvePackageFile(specifier, baseDir) {
	const segments = specifier.split("/");
	const name = specifier.startsWith("@") ? segments.slice(0, 2).join("/") : segments[0];
	const subpath = `.${specifier.slice(name.length)}`;
	for (let dir = path.resolve(baseDir); ; dir = path.dirname(dir)) {
		const pkgDir = path.join(dir, "node_modules", name);
		const manifest = path.join(pkgDir, "package.json");
		if (existsSync(manifest)) {
			const pkg = JSON.parse(readFileSync(manifest, "utf8"));
			if (pkg.exports !== undefined) {
				const target = resolveExportsSubpath(pkg.exports, subpath);
				if (target === null) throw new ToolError(`'${specifier}' is not exported for import by ${name}`);
				return path.join(pkgDir, target);
			}
			return resolveModuleFile(subpath === "." ? path.join(pkgDir, pkg.main ?? "index.js") : path.join(pkgDir, subpath));
		}
		if (path.dirname(dir) === dir) break;
	}
	try {
		return createRequire(path.join(baseDir, "[module]")).resolve(specifier);
	} catch (error) {
		throw new ToolError(`cannot resolve '${specifier}' from ${baseDir}: ${error.message}`);
	}
}

/** Maps a specifier from `baseDir` to a managed local module file, or to an external URL that Node loads natively. */
function resolveSpecifierTarget(specifier, baseDir) {
	if (specifier.startsWith("node:") || specifier.startsWith("data:") || specifier.startsWith("http:") || specifier.startsWith("https:") || isBuiltin(specifier)) {
		return { external: specifier };
	}
	let file;
	if (specifier.startsWith("file:")) {
		file = fileURLToPath(specifier);
	} else if (LOCAL_SPECIFIER_RE.test(specifier) || specifier.startsWith("~/") || WINDOWS_DRIVE_RE.test(specifier)) {
		const expanded = specifier.startsWith("~/") ? path.join(os.homedir(), specifier.slice(2)) : specifier;
		file = resolveModuleFile(path.resolve(baseDir, expanded));
	} else {
		file = resolvePackageFile(specifier, baseDir);
	}
	return isManagedLocalModule(file) ? { file } : { external: pathToFileURL(file).href };
}

/** Reads one managed module: its source (TypeScript stripped) and the target of each static specifier. */
function readModuleNode(file) {
	const raw = readFileSync(file, "utf8");
	const source = /\.(?:ts|tsx|mts)$/.test(file) ? stripTypeScript(raw, { force: true, file }) : raw;
	const dir = path.dirname(file);
	const specifiers = new Set();
	rewriteStaticSpecifiers(source, spec => {
		specifiers.add(spec);
		return spec;
	});
	const targets = new Map([...specifiers].map(spec => [spec, resolveSpecifierTarget(spec, dir)]));
	return { source, targets };
}

/**
 * Materializes the whole module graph reachable from `entry` into one directory named by the graph's content. Edits
 * anywhere in the graph produce a new directory, so every module re-evaluates on the next run; an unchanged graph reuses
 * its directory and keeps its instances. Relative specifiers let Node link cycles natively.
 */
function materializeModuleGraph(entry) {
	const nodes = new Map();
	const pending = [entry];
	while (pending.length > 0) {
		const file = pending.pop();
		if (nodes.has(file)) continue;
		const node = readModuleNode(file);
		nodes.set(file, node);
		for (const target of node.targets.values()) if (target.file) pending.push(target.file);
	}
	const files = [...nodes.keys()].sort();
	const names = new Map(files.map((file, index) => [file, `m${index}.mjs`]));
	const bodies = files.map(file => {
		const node = nodes.get(file);
		const dir = path.dirname(file);
		const linked = rewriteStaticSpecifiers(node.source, spec => {
			const target = node.targets.get(spec);
			return target.file ? `./${names.get(target.file)}` : target.external;
		});
		const href = pathToFileURL(file).href;
		const withSyntax = rewriteModuleSyntax(linked, { url: href, path: file, dir }, dir);
		return [
			`const require = globalThis.__dial_get_require__(${JSON.stringify(href)});`,
			`const __filename = ${JSON.stringify(file)};`,
			`const __dirname = ${JSON.stringify(dir)};`,
			withSyntax,
		].join("\n");
	});
	const digest = createHash("sha256");
	for (const body of bodies) digest.update(body).update("\0");
	const graphDir = path.join(moduleRootDir(), digest.digest("hex"));
	if (!existsSync(graphDir)) {
		const staging = mkdtempSync(path.join(moduleRootDir(), "stage-"));
		files.forEach((file, index) => writeFileSync(path.join(staging, `m${index}.mjs`), bodies[index]));
		renameSync(staging, graphDir);
	}
	return pathToFileURL(path.join(graphDir, names.get(entry))).href;
}

/** Imports a specifier relative to `baseDir`; managed local graphs are materialized per call so edits take effect. */
async function importFrom(baseDir, specifier, options) {
	const target = resolveSpecifierTarget(String(specifier), baseDir);
	return import(target.file ? materializeModuleGraph(target.file) : target.external, options);
}

Object.assign(globalThis, {
	__dial_import__: (specifier, options) => importFrom(process.cwd(), specifier, options),
	__dial_import_from__: (dir, specifier, options) => importFrom(dir, specifier, options),
	__dial_get_require__: href => createRequire(fileURLToPath(href)),
});

class ComputerRuntime {
	setRunScope(scope) {
		Object.assign(globalThis, scope);
	}

	/** Sync code runs as indirect eval (completion value); code with top-level await/return runs as an async body. */
	async run(code, filename, hooks) {
		const state = { hooks, final: undefined };
		return await runStore.run(state, async () => {
			const stripped = stripTypeScript(code);
			const finalRewritten = rewriteFinalExpression(stripped);
			const returned = finalRewritten !== null && compiles(rewriteImports(finalRewritten));
			const imported = rewriteImports(returned ? finalRewritten : stripped);
			// Rewriting a top-level return can make `await (...)` parse as a
			// synchronous call to an identifier named await. Preserve the original
			// source's requirement for an async body (including generated fn runs).
			const publish = !isScriptSource(rewriteImports(stripped)) || !isScriptSource(imported);
			const source = demoteTopLevelLexicals(imported, publish);
			const body = `${source}\n//# sourceURL=${filename}`;
			const value = publish ? await new AsyncFunction(body)() : (0, eval)(body);
			const awaited = await value;
			if (returned && state.final !== undefined) return await state.final.value;
			return awaited;
		});
	}
}

// ---------------------------------------------------------------------------
// Desktop object model exposed to scripts
// ---------------------------------------------------------------------------

async function captureScreenshot(session, getContext, target, options, region) {
	const context = getContext();
	const caps = {
		maxWidth: context.snapshot.captureMaxWidth,
		maxHeight: context.snapshot.captureMaxHeight,
	};
	const frame = await nativeCall(context.signal, () =>
		region === undefined ? session.capture(target, caps) : session.captureRegion(target, region, caps),
	);
	return await emitScreenshot(context, frame, options);
}

async function emitScreenshot(context, frame, options) {
	const bytes = frameBytes(frame.data);
	const destination = path.join(os.tmpdir(), `dial-computer-${randomUUID()}.png`);
	await fsp.writeFile(destination, bytes);
	throwIfAborted(context.signal);
	const result = {
		path: destination,
		width: frame.width,
		height: frame.height,
		coordinateWidth: frame.coordinateWidth,
		coordinateHeight: frame.coordinateHeight,
		...(frame.region ? { region: frame.region } : {}),
	};
	const scaled = frame.width !== frame.sourceWidth || frame.height !== frame.sourceHeight;
	context.screenshots.push({
		...result,
		sourceWidth: frame.sourceWidth,
		sourceHeight: frame.sourceHeight,
		target: frame.target,
	});
	if (!options?.silent) {
		const dimensions = `${frame.width}×${frame.height}${scaled ? ` (scaled from ${frame.sourceWidth}×${frame.sourceHeight})` : ""}`;
		const coordinates = `coordinateWidth=${frame.coordinateWidth} coordinateHeight=${frame.coordinateHeight}`;
		context.output.push({
			type: "text",
			text: frame.region
				? `zoom ${frame.target} ${dimensions}; region=${JSON.stringify(frame.region)}; ${coordinates}; use the base full screenshot coordinates for input, not zoom pixels → ${destination}`
				: `screenshot ${frame.target} ${dimensions}; ${coordinates} → ${destination}`,
		});
		context.output.push({
			type: "image",
			data: bytes.toString("base64"),
			mimeType: "image/png",
			detail: "original",
		});
	}
	return result;
}

class El {
	#session;
	#getContext;

	constructor(session, getContext, node) {
		this.#session = session;
		this.#getContext = getContext;
		this.ref = node.ref;
		this.role = node.role;
		this.nativeRole = node.nativeRole;
		this.title = node.title;
		this.description = node.description;
		this.enabled = node.enabled;
		this.focused = node.focused;
		this.childCount = node.childCount;
	}

	async value() {
		const { signal } = this.#getContext();
		return (await nativeCall(signal, () => this.#session.axNode(this.ref))).value;
	}

	async setValue(value) {
		const context = this.#getContext();
		guardRun(context, "setValue");
		await nativeCall(context.signal, () => this.#session.axSetValue(this.ref, value));
	}

	async bounds() {
		const { signal } = this.#getContext();
		const node = await nativeCall(signal, () => this.#session.axNode(this.ref));
		if (node.x === undefined || node.y === undefined || node.width === undefined || node.height === undefined) return null;
		return { x: node.x, y: node.y, width: node.width, height: node.height };
	}

	async attributes() {
		const { signal } = this.#getContext();
		return Object.fromEntries(await nativeCall(signal, () => this.#session.axAttributes(this.ref)));
	}

	async actions() {
		const { signal } = this.#getContext();
		return (await nativeCall(signal, () => this.#session.axNode(this.ref))).actions ?? [];
	}

	async perform(action) {
		const context = this.#getContext();
		guardRun(context, "perform");
		await nativeCall(context.signal, () => this.#session.axPerform(this.ref, action));
	}

	async press() {
		const context = this.#getContext();
		guardRun(context, "press");
		await nativeCall(context.signal, () => this.#session.axPerform(this.ref, "press"));
	}

	async click(options) {
		const context = this.#getContext();
		guardRun(context, "click");
		await nativeCall(context.signal, () => this.#session.axClick(this.ref, pointerOptions(options)));
	}

	async focus() {
		const context = this.#getContext();
		guardRun(context, "focus");
		await nativeCall(context.signal, () => this.#session.axFocus(this.ref));
	}

	async parent() {
		const { signal } = this.#getContext();
		const node = await nativeCall(signal, () => this.#session.axParent(this.ref));
		return node ? new El(this.#session, this.#getContext, node) : null;
	}

	async children() {
		const { signal } = this.#getContext();
		return (await nativeCall(signal, () => this.#session.axChildren(this.ref))).map(
			node => new El(this.#session, this.#getContext, node),
		);
	}
}

class Win {
	#session;
	#getContext;

	constructor(session, getContext, window) {
		this.#session = session;
		this.#getContext = getContext;
		this.id = window.id;
		this.app = window.app;
		this.title = window.title;
		this.pid = window.pid;
		this.bounds = { x: window.x, y: window.y, width: window.width, height: window.height };
		this.focused = window.focused;
	}

	screenshot(options) {
		return captureScreenshot(this.#session, this.#getContext, this.id, options);
	}

	zoom(region, options) {
		if (!region || typeof region !== "object" || Array.isArray(region)) {
			throw new ToolError("zoom requires a region { x, y, width, height } in the last full screenshot's pixels");
		}
		return captureScreenshot(this.#session, this.#getContext, this.id, options, region);
	}

	async click(x, y, options) {
		const context = this.#getContext();
		guardRun(context, "click");
		await nativeCall(context.signal, () => this.#session.click(this.id, x, y, pointerOptions(options)));
	}

	async doubleClick(x, y, options) {
		const context = this.#getContext();
		guardRun(context, "doubleClick");
		await nativeCall(context.signal, () => this.#session.click(this.id, x, y, pointerOptions({ ...options, count: 2 })));
	}

	async move(x, y) {
		const context = this.#getContext();
		guardRun(context, "move");
		await nativeCall(context.signal, () => this.#session.moveMouse(this.id, x, y, pointerOptions()));
	}

	async drag(points, options) {
		const context = this.#getContext();
		guardRun(context, "drag");
		await nativeCall(context.signal, () =>
			this.#session.drag(
				this.id,
				points.map(([x, y]) => ({ x, y })),
				pointerOptions(options),
			),
		);
	}

	async scroll(x, y, options = {}) {
		const context = this.#getContext();
		guardRun(context, "scroll");
		await nativeCall(context.signal, () =>
			this.#session.scroll(this.id, x, y, options.dx ?? 0, options.dy ?? 0, pointerOptions(options)),
		);
	}

	async type(text, options) {
		const context = this.#getContext();
		guardRun(context, "type");
		await nativeCall(context.signal, () => this.#session.typeText(this.id, text, pointerOptions(options)));
	}

	async press(chord, options) {
		const context = this.#getContext();
		guardRun(context, "press");
		await nativeCall(context.signal, () => this.#session.keyChord(this.id, chordKeys(chord), pointerOptions(options)));
	}

	async holdKeys(keys, options) {
		const context = this.#getContext();
		guardRun(context, "holdKeys");
		validateHold(options);
		validateKeys(keys, "keys");
		await nativeCall(context.signal, () => this.#session.holdKeys(this.id, keys, options));
	}

	async holdMouse(x, y, options) {
		const context = this.#getContext();
		guardRun(context, "holdMouse");
		validateHold(options);
		if (options.keys !== undefined) validateKeys(options.keys, "keys");
		await nativeCall(context.signal, () => this.#session.holdMouse(this.id, x, y, options));
	}

	async observe(options) {
		const context = this.#getContext();
		const result = await nativeCall(context.signal, () =>
			this.#session.observe(
				this.id,
				{
					maxWidth: context.snapshot.captureMaxWidth,
					maxHeight: context.snapshot.captureMaxHeight,
				},
				options && { all: options.all, maxDepth: options.maxDepth },
			),
		);
		const screenshot = await emitScreenshot(context, result.capture, options);
		if (!options?.silent) context.output.push({ type: "text", text: result.accessibility.text });
		return {
			...screenshot,
			ax: result.accessibility.text,
			nodeCount: result.accessibility.nodeCount,
			truncated: result.accessibility.truncated,
		};
	}

	get menu() {
		return {
			items: async path => {
				const context = this.#getContext();
				const segments = path === undefined ? undefined : typeof path === "string" ? [path] : path;
				if (segments !== undefined) validateKeys(segments, "menu path", { allowEmpty: true });
				return await nativeCall(context.signal, () => this.#session.menuItems(this.id, segments));
			},
			select: async (path, options) => {
				const context = this.#getContext();
				guardRun(context, "menu.select");
				validateKeys(path, "menu path");
				await nativeCall(context.signal, () => this.#session.menuSelect(this.id, path, options));
			},
		};
	}

	async bringToCurrentSpace() {
		const context = this.#getContext();
		guardRun(context, "bringToCurrentSpace");
		await nativeCall(context.signal, () => this.#session.bringToCurrentSpace(this.id));
	}

	async raise() {
		const context = this.#getContext();
		guardRun(context, "raise");
		await nativeCall(context.signal, () => this.#session.raiseWindow(this.id));
	}

	async ax(options) {
		const { signal } = this.#getContext();
		return (await nativeCall(signal, () => this.#session.axSnapshot(this.id, options))).text;
	}

	async find(query) {
		const { signal } = this.#getContext();
		return (await nativeCall(signal, () => this.#session.axQuery(this.id, query))).map(
			node => new El(this.#session, this.#getContext, node),
		);
	}

	async ref(ref) {
		const { signal } = this.#getContext();
		return new El(this.#session, this.#getContext, await nativeCall(signal, () => this.#session.axNode(ref)));
	}
}

// ---------------------------------------------------------------------------
// Worker core
// ---------------------------------------------------------------------------

class ComputerWorkerCore {
	#transport;
	#session;
	#runtime = new ComputerRuntime();
	#active = null;
	#runContexts = new AsyncLocalStorage();
	#closed = false;

	constructor(transport) {
		this.#transport = transport;
		transport.onMessage(message => this.handle(message));
		this.#transport.send({ type: "ready" });
	}

	/** Routes one host command into the persistent worker state. */
	handle(message) {
		switch (message.type) {
			case "ping":
				this.#transport.send({ type: "pong", id: message.id });
				return;
			case "run":
				this.#run(message).catch(error =>
					this.#transport.send({ type: "result", id: message.id, ok: false, error: errorPayload(error) }),
				);
				return;
			case "capabilities":
				void this.#capabilities(message);
				return;
			case "abort":
				if (this.#active?.id === message.id) this.#active.ac.abort(new ToolAbortError());
				return;
			case "revoke-control":
				this.#active?.ac.abort(new ToolAbortError("Computer control revoked"));
				Promise.resolve(this.#session?.cancel("control_released")).then(() => {
					this.#transport.send({ type: "control-revoked", id: message.id });
				}).catch(error => {
					this.#transport.send({ type: "control-revoked", id: message.id, error: errorPayload(error) });
				});
				return;
			case "tool-reply":
				this.#deliverToolReply(message.id, message.reply);
				return;
			case "close":
				void this.#close();
		}
	}

	#ensureSession() {
		this.#session ??= createNativeSession();
		return this.#session;
	}

	async #run(message) {
		if (this.#closed) {
			this.#transport.send({
				type: "result",
				id: message.id,
				ok: false,
				error: errorPayload(new ToolError("Computer worker is closed")),
			});
			return;
		}
		if (this.#active) {
			this.#transport.send({
				type: "result",
				id: message.id,
				ok: false,
				error: errorPayload(new ToolError("Computer worker is busy")),
			});
			return;
		}
		const timeoutSignal = AbortSignal.timeout(message.timeoutMs);
		const ac = new AbortController();
		const runAc = new AbortController();
		const signal = AbortSignal.any([timeoutSignal, ac.signal, runAc.signal]);
		const active = { id: message.id, ac, signal, pendingTools: new Map() };
		this.#active = active;
		// Cancel synchronously while this run owns the native session, including
		// fire-and-forget operations still pending when the script returns.
		let nativeCancellation;
		const onNativeCancel = () => {
			if (this.#active === active && this.#session) {
				nativeCancellation = this.#session.cancel(timeoutSignal.aborted ? "run_timeout" : "run_aborted");
				nativeCancellation.catch(() => undefined);
			}
		};
		signal.addEventListener("abort", onNativeCancel, { once: true });
		const output = new RunOutput();
		const screenshots = [];
		const runContext = {
			signal,
			readOnly: message.session.readOnly,
			snapshot: message.session,
			output,
			screenshots,
			confirmControl: reason => this.#confirmControl(active, reason),
		};
		let returnValue;
		let failure;
		let completed = false;
		try {
			throwIfAborted(signal);
			const session = await this.#ensureSession(message.session);
			throwIfAborted(signal);
			const desktop = this.#createDesktopScope(session);
			this.#runtime.setRunScope({
				desktop: bindRunFacade(desktop, signal),
				assert: (condition, text) => {
					if (!condition) throw new ToolError(text ?? "Assertion failed");
				},
				wait: (msOrPredicate, options) => {
					const resolved =
						typeof msOrPredicate === "number"
							? undefined
							: {
									timeout: resolvePredicateTimeout(message.timeoutMs, options?.timeout),
									interval: options?.interval,
								};
					return waitForRun(msOrPredicate, signal, resolved);
				},
			});
			let rejectCancel;
			const cancelRejection = new Promise((_resolve, reject) => {
				rejectCancel = reject;
			});
			cancelRejection.catch(() => undefined);
			const onCancel = () => {
				rejectCancel(
					timeoutSignal.aborted
						? new ToolError(`Computer code execution timed out after ${message.timeoutMs}ms`)
						: abortError(signal),
				);
				const toolAbort = timeoutSignal.aborted
					? new ToolAbortError(undefined, { cause: timeoutSignal.reason })
					: abortError(signal);
				for (const pending of active.pendingTools.values()) pending.reject(toolAbort);
				active.pendingTools.clear();
			};
			if (signal.aborted) onCancel();
			else signal.addEventListener("abort", onCancel, { once: true });
			try {
				returnValue = await Promise.race([
					this.#runContexts.run(runContext, () =>
						this.#runtime.run(message.code, `computer-run-${message.id}.js`, this.#runtimeHooks(active, output)),
					),
					cancelRejection,
				]);
				completed = true;
			} finally {
				signal.removeEventListener("abort", onCancel);
			}
		} catch (error) {
			failure = { error: hintWrongGlobal(error) };
		} finally {
			// Successful completion retires outstanding native work but keeps an explicitly acquired grant; errors revoke it.
			signal.removeEventListener("abort", onNativeCancel);
			// The host dispatches native calls on threads. Await cleanup before
			// publishing a result, otherwise late retirement can cancel the NEXT run.
			try {
				if (nativeCancellation) await nativeCancellation;
				else if (failure === undefined && !signal.aborted) await this.#session?.retire();
				else await this.#session?.cancel("run_failed");
			} finally {
				runAc.abort(new ToolAbortError("Computer run ended"));
				if (this.#active?.id === message.id) this.#active = null;
			}
		}
		if (failure !== undefined) {
			this.#transport.send({ type: "result", id: message.id, ok: false, error: errorPayload(failure.error) });
			return;
		}
		if (completed) {
			let capabilities;
			try {
				capabilities = await (await this.#ensureSession(message.session)).capabilities();
			} catch (error) {
				this.#transport.send({ type: "result", id: message.id, ok: false, error: errorPayload(nativeError(error)) });
				return;
			}
			this.#transport.send({
				type: "result",
				id: message.id,
				ok: true,
				payload: { displays: output.finish(), returnValue: cloneSafe(returnValue), screenshots, capabilities },
			});
		}
	}

	/** Answers a capabilities request without running a script; never touches `#active`. */
	async #capabilities(message) {
		if (this.#closed) {
			this.#transport.send({
				type: "capabilities",
				id: message.id,
				ok: false,
				error: errorPayload(new ToolError("Computer worker is closed")),
			});
			return;
		}
		try {
			const session = await this.#ensureSession(message.session);
			this.#transport.send({ type: "capabilities", id: message.id, ok: true, capabilities: await session.capabilities() });
		} catch (error) {
			this.#transport.send({
				type: "capabilities",
				id: message.id,
				ok: false,
				error: errorPayload(error instanceof ToolAbortError ? error : nativeError(error)),
			});
		}
	}

	#runtimeHooks(active, output) {
		return {
			onText: chunk => {
				throwIfAborted(active.signal);
				output.pushText(chunk);
			},
			onDisplay: display => {
				throwIfAborted(active.signal);
				output.pushDisplay(display);
			},
			callTool: (name, args) => {
				throwIfAborted(active.signal);
				return this.#callTool(active, name, args);
			},
		};
	}

	async #callTool(active, name, args) {
		const id = `computer-tc-${active.id}-${randomUUID()}`;
		const pending = Promise.withResolvers();
		active.pendingTools.set(id, pending);
		this.#transport.send({ type: "tool-call", id, runId: active.id, name, args });
		return await pending.promise;
	}

	async #confirmControl(active, reason) {
		throwIfAborted(active.signal);
		const id = `computer-control-${active.id}-${randomUUID()}`;
		const pending = Promise.withResolvers();
		active.pendingTools.set(id, pending);
		this.#transport.send({ type: "control-request", id, runId: active.id, reason });
		return (await pending.promise) === true;
	}

	#deliverToolReply(id, reply) {
		const pending = this.#active?.pendingTools.get(id);
		if (!pending) return;
		this.#active?.pendingTools.delete(id);
		if (reply.ok) pending.resolve(reply.value);
		else pending.reject(replyError(reply.error));
	}

	#currentRunContext = () => {
		const context = this.#runContexts.getStore();
		if (!context) throw new ToolError("no active computer run");
		return context;
	};

	#createDesktopScope(session) {
		const getContext = this.#currentRunContext;
		const makeWin = window => new Win(session, getContext, window);
		const el = node => new El(session, getContext, node);
		const desktopTarget = new Win(session, getContext, {
			id: "desktop",
			app: "desktop",
			title: "desktop",
			x: 0,
			y: 0,
			width: 0,
			height: 0,
			focused: false,
		});
		return {
			capabilities: async () => {
				const { signal } = getContext();
				return await nativeCall(signal, () => session.capabilities());
			},
			displays: async () => {
				const { signal } = getContext();
				return await nativeCall(signal, () => session.listDisplays());
			},
			display: async selector => {
				const { signal } = getContext();
				if (typeof selector !== "string" || !selector) throw new ToolError("display requires an id, 'active', or 'all'");
				if (selector !== "active" && selector !== "all") {
					const displays = await nativeCall(signal, () => session.listDisplays());
					if (!displays.some(display => display.id === selector)) throw new ToolError(`Unknown display: ${selector}`);
				}
				const target = new Win(session, getContext, {
					id: `display:${selector}`,
					app: "",
					title: "",
					x: 0,
					y: 0,
					width: 0,
					height: 0,
					focused: false,
				});
				return {
					id: selector,
					screenshot: target.screenshot.bind(target),
					zoom: target.zoom.bind(target),
					click: target.click.bind(target),
					doubleClick: target.doubleClick.bind(target),
					move: target.move.bind(target),
					drag: target.drag.bind(target),
					scroll: target.scroll.bind(target),
					type: target.type.bind(target),
					press: target.press.bind(target),
					holdKeys: target.holdKeys.bind(target),
					holdMouse: target.holdMouse.bind(target),
				};
			},
			apps: {
				list: async options => {
					const { signal } = getContext();
					return await nativeCall(signal, () => session.listApplications(options));
				},
				open: async (id, options) => {
					const context = getContext();
					guardRun(context, "apps.open");
					return await nativeCall(context.signal, () => session.openApplication(id, options));
				},
			},
			control: {
				acquire: async options => {
					const context = getContext();
					guardRun(context, "control.acquire");
					strictObject(options, ["reason"], "control.acquire");
					if (typeof options.reason !== "string" || !options.reason.trim()) {
						throw new ToolError("control.acquire requires a non-empty reason");
					}
					if ((await nativeCall(context.signal, () => session.controlState())).active) return { active: true };
					const approved = await context.confirmControl(options.reason);
					throwIfAborted(context.signal);
					if (!approved) return { active: false };
					return await nativeCall(context.signal, () => session.acquireControl());
				},
				release: async () => {
					const context = getContext();
					guardRun(context, "control.release");
					await nativeCall(context.signal, () => session.releaseControl());
				},
				state: async () => {
					const { signal } = getContext();
					return await nativeCall(signal, () => session.controlState());
				},
			},
			windows: async filter => {
				const { signal, snapshot } = getContext();
				return (await listWindowSnapshot(session, signal, snapshot.runtimeIdentity)).windows.filter(window => matchesFilter(window, filter));
			},
			window: async selector => {
				const context = getContext();
				if (typeof selector === "string" || typeof selector === "number") {
					const window = await nativeCall(context.signal, () => session.getWindow(String(selector)));
					return makeWin(window);
				}
				const snapshot = await listWindowSnapshot(session, context.signal, context.snapshot.runtimeIdentity);
				const windows = snapshot.windows;
				const matches = windows.filter(window => matchesFilter(window, selector));
				if (matches.length === 0) {
					throw new ToolError(`no window matches ${JSON.stringify(selector)} ${describeWindowSnapshot(snapshot)}`);
				}
				if (matches.length > 1) {
					const candidates = matches.map(window => `${window.id} ${window.app} ${JSON.stringify(window.title)}`).join("\n");
					throw new ToolError(
						`multiple windows match ${JSON.stringify(selector)}:\n${candidates}\n${describeWindowSnapshot(snapshot)}`,
					);
				}
				return makeWin(matches[0]);
			},
			focusedWindow: async () => {
				const { signal } = getContext();
				const window = (await nativeCall(signal, () => session.listWindows())).find(candidate => candidate.focused);
				return window ? makeWin(window) : null;
			},
			screenshot: options => captureScreenshot(session, getContext, "desktop", options),
			zoom: desktopTarget.zoom.bind(desktopTarget),
			click: desktopTarget.click.bind(desktopTarget),
			doubleClick: desktopTarget.doubleClick.bind(desktopTarget),
			move: desktopTarget.move.bind(desktopTarget),
			drag: desktopTarget.drag.bind(desktopTarget),
			scroll: desktopTarget.scroll.bind(desktopTarget),
			type: desktopTarget.type.bind(desktopTarget),
			press: desktopTarget.press.bind(desktopTarget),
			holdKeys: desktopTarget.holdKeys.bind(desktopTarget),
			holdMouse: desktopTarget.holdMouse.bind(desktopTarget),
			elementAt: async (x, y) => {
				const { signal } = getContext();
				const node = await nativeCall(signal, () => session.axElementAt("desktop", x, y));
				return node ? el(node) : null;
			},
			focusedElement: async () => {
				const { signal } = getContext();
				const node = await nativeCall(signal, () => session.axFocused());
				return node ? el(node) : null;
			},
			ref: async ref => {
				const { signal } = getContext();
				return el(await nativeCall(signal, () => session.axNode(ref)));
			},
			clipboard: {
				read: async () => {
					const { signal } = getContext();
					return await nativeCall(signal, () => native("clipboardRead"));
				},
				write: async text => {
					const context = getContext();
					guardRun(context, "clipboard.write");
					await nativeCall(context.signal, () => native("clipboardWrite", { text }));
				},
			},
		};
	}

	async #close() {
		if (this.#closed) return;
		this.#closed = true;
		this.#active?.ac.abort(new ToolAbortError());
		this.#session = undefined;
		this.#transport.send({ type: "closed" });
		this.#transport.close();
	}
}

// ---------------------------------------------------------------------------
// Entry: line-delimited JSON over stdin/stdout
// ---------------------------------------------------------------------------

let inboundHandler = null;

const core = new ComputerWorkerCore({
	send: message => writeLine(message),
	onMessage: handler => {
		inboundHandler = handler;
	},
	close: () => {
		process.stdin.pause();
		protocolOut("", () => process.exit(0));
	},
});

const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on("line", line => {
	if (!line.trim()) return;
	let message;
	try {
		message = JSON.parse(line);
	} catch (error) {
		process.stderr.write(`computer worker: invalid protocol line: ${error.message}\n`);
		return;
	}
	if (message === null || typeof message !== "object") {
		process.stderr.write("computer worker: protocol line is not an object\n");
		return;
	}
	if (message.type === "native-result") {
		settleNative(message);
		return;
	}
	inboundHandler?.(message);
});
lines.on("close", () => process.exit(0));

export { core as _computerWorkerCore };
