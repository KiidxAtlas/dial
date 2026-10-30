# Execution and web tools

## Background job controls

`/processes` (also `/bg`) lists the current session's background jobs. Enter or click a job
to inspect it. Pipe-based jobs show their retained output without consuming the `bg output` or
`bg wait` cursor. The view follows the newest output by default; arrows and Page Up/Page Down
scroll, Home shows the oldest retained rows, and End resumes following. Esc returns to the list.
Output older than the bounded capture ring may remain in the job's output artifact when
artifact capture is configured, subject to the artifact's own retention cap.

Only running PTY jobs accept keyboard input and paste. While attached to one, Esc and Ctrl+C
belong to the process; Ctrl+] detaches. Exited jobs and pipe-output views are read-only.
`bg wait` reports output-capture failures as errors even when the observed process has exited;
the exit status and capture diagnostic remain visible together.

Execution metadata separates the observation (`snapshot`, `wait`, `readiness`, `input`, or
`termination`, plus whether it was satisfied) from the child's exit code/signal. In particular,
successful `bg wait` observation of exit 3 does not mean the command succeeded. Output metadata
reports capture, retention, and presentation completeness independently. Byte totals count
observed bytes, not unread estimates; generated labels and omission markers are not original
output. Clipping or summarizing a model-facing result downgrades presentation only. Artifact
retention caps can remove the middle of a stream even when capture reached EOF.

## Persistent kernels and evaluation

The `eval` tool runs persistent cells in two languages selected by `language`: `js` (the default) and `py`. Each language keeps its own namespace between successful cells. Recorded `kernel` calls from earlier sessions replay as `eval` Python cells. Interrupting a running Python cell first sends SIGINT; an interrupt that does not unwind within five seconds escalates to terminating the kernel. Failed transport, cancellation and timeout paths discard the namespace. Effects already performed remain; loss of the final cell result is recorded as uncertain. Python interpreters may be selected from the project virtual environment (`.venv`/`VIRTUAL_ENV`) or conda environment when configured/approved, but execution remains subject to the mandatory process-creation fence. The cell deadline pauses while a tool callback requested by the cell is running, so callback time is not charged to the cell.

### Backgrounding `eval` cells and named `bash` sessions

An `eval` cell that is still running after the `[shell] auto_background_secs` threshold (default 15; `0` disables it) moves to the background. The threshold is measured from the start of the cell, including runtime startup. `background: true` starts the cell in the background at once; `background: false` keeps it in the foreground for its whole run. `title` (at most 256 bytes) labels the job.

A moved cell is a `bg` job: read it with `output`, wait for it with `wait`, or stop it with `kill`, and it appears in `list`. Its runtime stays busy until the cell finishes; a foreground `eval` on the same runtime is refused with `runtime busy in background job <id>`. Host tool callbacks from a moved cell are refused with "this cell was moved to the background; host tool calls are unavailable there — rerun with `background:false` to keep it in the foreground". Killing the job stops the cell and discards its namespace.

If the background table already holds its maximum of running processes, a cell that would move stays in the foreground and its result starts with `backgrounding unavailable: <reason>; the cell stayed in the foreground`. A `background: true` request is served the same way, so check the job list when you need the cell detached.

A `bash` command that names a session (`session`) may move to the background under the same threshold. While the job runs, the session name is busy: a foreground command for it is refused with `session busy in background job <id>; wait or kill it`. When the job exits cleanly, the shell state it published (working directory and exported variables) is applied on the session's next use. A failed, timed-out, or killed job discards the session state, and killing a job releases the name.


`eval` with `language: "py"` supports macOS process-fork containment and the Windows native no-child-process mitigation;
Windows additionally requires the explicit unsandboxed setting because filesystem/network isolation
is unavailable. `eval` with `language: "js"` supports macOS only. **Both fail closed on Linux**, even when Bubblewrap is
available or filesystem sandboxing is disabled: Bubblewrap is not the mandatory process-creation
fence. No Linux or unsandboxed fallback is provided. Tool discovery and `status` expose the same
platform availability and reason used at startup; this describes platform support, not approval or
the availability of the configured Python/Node executable. Reset, cancel and status remain usable
on unsupported platforms.

JavaScript eval requires an operator-installed Node 22 or newer. It uses Node's persistent REPL
evaluator for lexical declarations and top-level `await`; runtime and syntax failures are reported
as cell errors without discarding an otherwise healthy namespace. Node 26 uses the public REPL
error hook, while earlier supported releases use the legacy REPL error path. Strings crossing the
JSON transport are normalized to well-formed Unicode, including truncated output and nested values.

Computer helpers in JavaScript and Python consume the host's raw structured tool data. Returned
values, display text, and screenshot artifacts remain available through the same approval-gated
bridge; a denied or malformed response raises an error rather than returning an empty result.

### Execution ownership and transport

Every forked execution owner gets independent Python, JavaScript, browser and computer session
state. Closing a child's computer session does not close its parent's worker, handles or captures.
Native foreground-input arbitration remains global: separate owners cannot bypass the OS input
lease or inherit another owner's foreground-control grant. Computer preludes still dispatch through
the current controller's approved tool boundary; they do not capture a shared desktop session.

`runtime/json_transport.rs` owns bounded JSON-lines transport, request/callback sequencing and
nonblocking deadline/cancellation-aware pipes for kernel/eval/browser. `runtime/process.rs` owns
process-group cleanup; `tools/shell.rs` keeps its compatibility re-export. Tool adapters retain
launch policy, sandbox requirements and authorized callback/prelude dispatch. Computer retains its
separate asynchronous supervisor protocol, with bounded frames and cancellable writes; malformed,
oversized or interrupted transport is discarded rather than reused. No universal driver is assumed.

Integration names, defaults and legacy always-registered behavior live in `tools/catalog.rs` alongside
the existing canonical schema catalog. Saved integration lists still retain browser, eval, find, autoresearch and archive; a saved `kernel` entry is renamed to `eval` when configuration is loaded or saved, and computer and notebook remain opt-in. These declarations describe
availability only and never grant execution authority.

## Browser

Browser sessions support headless launch (the default), optional CDP attachment to an existing browser, and isolated persistent storage state per profile. Browser actions include snapshots, ARIA snapshots, accessibility audits, tracing, and action recording. Linux launches use Bubblewrap containment. Loopback WebSocket connections are permitted for local development servers (including HMR) without disabling the outbound network guard. Orphaned tabs from prior processes are reaped rather than accumulating.

### Request log, routes, and HAR

- `requests` lists the tab's recent requests, newest first (`filter` matches URL text, `resource_type`, `limit` 1–200, default 50). `request` returns one request by `request_id`, with its response body when the body is textual (up to 32 KiB; binary bodies are reported as `body_bytes` with an empty `body`). The log keeps the most recent 200 requests per tab.
- `route` registers a glob route (`pattern`, `*` within a segment, `**` across segments, whole-URL match) with `route_action` `abort`, `continue`, or `fulfill` (`status`, `headers`, `content_type`, `body`; at most 64 KiB). `unroute` removes one pattern, or all routes when `pattern` is omitted. `routes` lists active routes without bodies. A tab holds at most 32 routes.
- Precedence: the allowed-host guard runs first. A request the guard refuses is blocked even if a route would match. Routes apply only to requests the guard permits. The first matching route wins.
- `har_start` begins recording and `har_stop` returns HAR 1.2 entries for requests made since the start, writes the file under the session artifacts directory, and reports `dropped` entries that aged out of the log. `content` is `none`, `text` (default, textual bodies), or `all` (non-textual bodies as base64). Bodies are capped at 64 KiB each and 2 MiB total per recording.
- Authorization, Cookie, Set-Cookie, and API-key headers are redacted in every output and HAR entry. `Location` and `Referer` headers and request URLs keep their query parameter names but replace the value of sensitive parameters with `[redacted]` (names such as `token`, `key`, `secret`, `password`, `auth`, `sig`/`signature`, `code`, `session`, `credential`, `access_token` and `api_key`, matched case-insensitively against each word of the name). The same redacted URL appears in `requests`, `request`, and the HAR `request.url`, `queryString`, and `redirectURL`. Fragments and non-sensitive parameters are unchanged.

### Init scripts and WebMCP

- `add_init_script` adds a document-start script to the browser context, from inline `source` or a workspace file `path` (UTF-8, at most 64 KiB). It returns an `id` and byte count. `remove_init_script` (`script_id`) removes it, and `init_scripts` lists active scripts by `id` and size. Script text is never echoed or logged. Attached browsers receive the scripts on their existing context too.
- `webmcp_list` reports tools registered through `navigator.modelContext` on the active tab's main frame, with bounded names and descriptions (at most 16 tools). Pass `name` for the exact tool's input schema. `webmcp_invoke` runs one tool with `params` (an object, at most 16 KiB) and returns its JSON result, capped at 64 KiB. `webmcp_events` returns registration changes after a `since` cursor (at most 256 buffered per tab) and clears them when `clear` is true.
- A page loaded before the hook was installed reports `status: unavailable` with a reason, not an empty success. When the page exposes no WebMCP tools, `webmcp_list` says so explicitly. All WebMCP output is untrusted page text.
- The bridge never calls the native `navigator.modelContext` getter, and it polyfills the object only when the browser provides none. Native browser-side WebMCP discovery is not supported.

### Browser relay

The browser tool can drive the Chrome tabs you already use, with your existing logins, through the Dial Chrome extension and a relay on this computer. The relay listens on `127.0.0.1` only and requires a token; no remote-debugging flag is needed.

Setup is one time:

1. Run `dial browser-relay install`. It writes the extension to `~/.dial/browser-extension` (or `$DIAL_HOME/browser-extension`) and prints that folder.
2. In Chrome, open `chrome://extensions`, turn on Developer mode, choose "Load unpacked", and select the folder.
3. Turn on `browser.relay` (Settings → Browser → "Use browser relay"). `browser.allow_attach` must also be on, because the relay attaches to your real browser.

When the browser tool runs, Dial starts the relay if nothing is listening yet, then waits up to 20 seconds for the extension to connect. Run `dial browser-relay status` to check the connection, or `dial browser-relay serve` to keep the relay running in the foreground. Set `DIAL_BROWSER_RELAY=1` or `0` to override the setting for one run. An explicit `browser.cdp_url` takes precedence over the relay.

Supported platforms are macOS, Linux, and Windows with Chrome or Chromium that supports extensions. The extension only talks to the loopback relay; it is not published to the Chrome Web Store.

## Web access

`web_fetch` and `web_search` require approval and are unavailable when the session sandbox disables networking. HTTP and HTTPS requests are resolved and checked against the SSRF policy at every redirect; private, loopback, link-local, and metadata endpoints remain blocked. Responses are bounded and returned as untrusted content.
`web_fetch` reports both display truncation and the download-size cutoff when both apply; text beyond
the download cap was not retrieved.

Fetch handles supported binary documents (including PDF and Office formats) with the document converter. For HTML, it probes declared Markdown alternate links and `.md` siblings, tries Markdown content negotiation, then configured readers with a quality gate, then `llms.txt` candidates, before using built-in HTML conversion. Optional external readers are configured through `web.reader_chain`; they receive HTML on standard input. Common GitHub, npm, crates.io, PyPI, and Stack Overflow URLs use public APIs when available, with ordinary fetching as fallback.

Search uses `web.search_provider` or a per-call provider selection. Supported backends include Brave, Tavily, Exa, Serper, SearXNG, Kagi, Perplexity, Jina, Google Custom Search, Bing, Firecrawl (`FIRECRAWL_API_KEY`), Parallel (`PARALLEL_API_KEY`), Z.AI (`ZAI_API_KEY`), xAI (`XAI_API_KEY`), TinyFish (`TINYFISH_API_KEY`), DeepSeek (`DEEPSEEK_API_KEY`), Kimi (`KIMI_SEARCH_API_KEY`), Synthetic (`SYNTHETIC_API_KEY`), Ollama Cloud (`OLLAMA_API_KEY`), and the keyless DuckDuckGo, Startpage, Mojeek and Ecosia HTML scrapers. `auto` tries configured keyed providers first and the keyless ones last; a scraper that meets a captcha, firewall or proof-of-work wall fails with an error so `auto` moves on. `public` is explicit only: it queries Startpage, DuckDuckGo, Ecosia and Mojeek in turn, ranks a result several engines return ahead of others, and fails only when all four fail. Kimi, Synthetic, Ollama and `public` cannot enforce the recency window; the answer says so. Grounded answer providers run the provider's own web search and return its answer with the sources it cited: OpenAI (`openai`, Responses `web_search`, `OPENAI_API_KEY`), Anthropic (`anthropic`, server `web_search` tool, `ANTHROPIC_API_KEY`), Gemini (`gemini`, `google_search` grounding, `GEMINI_API_KEY`), OpenRouter (`openrouter`, web plugin, `OPENROUTER_API_KEY`) and Codex (`codex`, Responses `web_search` on the active ChatGPT subscription session, no API key). They are explicit only, so `auto` never selects them. Each API provider reads its key from the environment or a key saved for that provider. Codex uses the signed-in ChatGPT account from `/providers`, is refused when none is signed in, and its turn is streamed through the same Responses parser as the chat path; its model id is a default that a future release may change. Their native search has no hard date filter, so the recency window is an instruction in the prompt and the answer says so; `domains` and `exclude_domains` are applied to the returned sources. Gemini's grounding redirect links (`vertexaisearch.cloud.google.com/grounding-api-redirect/…`) are resolved to the final page through the guarded fetch client: at most four resolve at once, each within 5 seconds, and a link that fails keeps its redirect URL. A reply that did not run a search, or that carries no sources, is an error, not a result. Provider credentials are supplied through their documented environment variables; no key is embedded in configuration or sent to a different origin. A custom `web.search_url` template remains supported and must contain `{query}`.

`domains` and `exclude_domains` restrict a search to, or drop results from, up to 20 sites each. An entry is a host such as `example.com`, which also matches its subdomains, or a host with a path such as `example.com/docs`, which matches that path and anything below it. TinyFish, Tavily, Exa and DeepSeek receive whole-host entries natively; every provider's results are also checked locally, so path entries and custom `web.search_url` endpoints are filtered too. If a list would remove every result, it is not applied and the answer says so.

## Coordinated edits

All native file commits take reentrant writer leases for their canonical document paths. Multi-document batches acquire them in one sorted order; recursive acquisitions that invert that order are refused. Separate Dial processes coordinate through the same OS file locks, released if their owner exits. Unrelated documents remain independently available. Ordinary text proposals can merge non-overlapping changes made after preparation. The merged proposal passes the existing syntax, permission, witness and transaction checks before commit. Merging never grants read coverage over another writer's unseen lines.

Overlapping edits, ambiguous insertion boundaries, whole-file replacements and projected documents require a fresh read. External editors and overlapping additional roots are still protected by exact source-identity checks; the lease does not claim to coordinate arbitrary external programs. Multi-file transaction rollback and observer behavior remain in `edit_transaction.rs`.

LSP file/directory rename captures its source snapshot before asking servers for `willRenameFiles`
edits, then validates the move sources and returned edit targets against that baseline. Forward
moves and rollback use atomic no-replace operations: a destination created while the request or
approval is pending is preserved, and rollback never overwrites a recreated source. Recovery failures
are reported rather than hidden. Supported implementations use native Linux, macOS and Windows
namespace primitives; unsupported platforms refuse instead of falling back to replacing rename.
These guarantees do not imply crash durability or protection against arbitrary ancestor-directory
replacement by unrelated processes.

## Debug adapters and MCP lifecycle notifications

`[dap.<id>]` defines an operator-owned adapter executable. The `dap` integration exposes
`dap_<id>` actions and requires approval on every call; adapter startup is lazy, and launch program
and cwd paths must resolve inside the workspace. `[mcp.<id>].lifecycle_hooks` opts into
`turn_started`, `turn_finished`, `tool_started`, and `tool_finished` notifications through
`notifications/dial/event`; payloads contain identifiers/status only, never prompts, arguments,
outputs, or source content. Hook delivery is observational and cannot bypass tool approvals.

MCP requests have bounded admission and locally owned dispatch/settlement. Cancelling queued
work prevents its dispatch; cancellation/deadline propagates into transport and credential
refresh work. HTTP initialization does not become ready before `notifications/initialized`
is delivered. Progress can renew inactivity allowance, but not the absolute request lifetime.
Cancellation is not rollback: after dispatch, remote cancellation is advisory and remote effects
may remain unknown. Such results stop automatic continuation rather than triggering a blind retry.
Native DNS/connect helpers have bounded phases but cannot be forcibly aborted inside the OS;
cancellation prevents subsequent dispatch. Escaped descendants, filesystem stalls and arbitrary
consumer callbacks likewise cannot be forcibly preempted by the local controller.

Connection health, notification health and catalog freshness are independent inventory facts.
A working POST channel does not prove a healthy notification stream. Degraded/unsupported
notification channels do not justify reusing indefinitely cached resource/prompt listings.
Reconnection checks the tool catalog against registered definitions, and stale definitions fail
closed instead of retaining an obsolete read-only approval exemption. After a notification gap,
the next call revalidates an unknown catalog without requiring a connection restart; matching
definitions restore freshness, while changed definitions remain stale. Reload the integration
to register a changed catalog. MCP results retain typed output-integrity metadata; structured
content is included alongside text, while binary placeholders and text bounds mark presentation
partial. A protocol-result artifact is labelled complete only if its retention was complete.

## Browser provisioning and local hosts

The browser tool provisions its own pinned Playwright-core, Readability, and Chromium into a private
owner-only cache on first use (`bun install --frozen-lockfile --ignore-scripts`, then Chromium install);
no operator setup is required. Bun comes from `DIAL_BUN`, Dial's own install (`~/.dial/tools/bun`) or
`PATH`. If none has it, the first browser use that needs to install dependencies installs Bun through
the same recipe as `/setup bun yes` (npm into Dial's tools directory, Homebrew, or the pinned
checksummed release; never sudo). That install is authorized like the browser call's other native
effects, so plan mode, `readonly` approval and path-scoped browser rules refuse it. A refusal or
failed install reports the reason and names `/setup bun yes`. With `browser.allow_install = false`,
Dial neither installs Bun nor fetches dependencies. Cached JavaScript dependencies can be used offline with an installed
`[browser] chromium_executable` or CDP attach; those modes do not download private Chromium.
Missing JavaScript dependencies still require registry access, even when attaching to a browser.
Set `[browser] playwright_module` only to override with an operator-owned Playwright entry
(absolute path, Playwright-core >=1.48). Workspace-owned entries (including symlink targets) are
rejected before evaluation. Readability still comes from the built-in runtime. Approved browser `open`/`navigate`
requests to `localhost` or `127.0.0.1` automatically permit only that exact host and port for the browser
session, so local development sites work without configuring each port. Redirects and subresources
cannot grant access to another local endpoint. `/settings` → Tools → Browser controls this behavior;
`auto_localhost=false` restores explicit allowlisting with `allowed_hosts=["127.0.0.1:3000"]`.
Other private hosts still require operator allowlisting. Metadata/link-local addresses remain forbidden.
For an app with a separate local API (for example, Supabase), allowlist its exact host and port as
well as the app's if needed. `localhost` and `127.0.0.1` are distinct; inspect browser network events
for the refused endpoint. Approved WebSockets are limited to exact allowlisted loopback endpoints.
Use managed `bg` processes for previews: list/reuse a running server, start one if needed, wait for
readiness, then open its HTTP URL. Foreground `bash` kills servers at its deadline. The browser accepts
`about:blank`, and tab IDs may be omitted when exactly one tab is open; multiple tabs require an ID.
Browser failures show the underlying network refusal before Chromium's generic error. Networking is
paused between approved browser operations. Python eval cells need an installed Python; PDF
capabilities need Poppler.

Built-in Chromium operations have a 30-second overall deadline by default, including startup and
network settlement. A missing or non-actionable click target must not wait indefinitely. Set
`[browser] timeout_secs` to a larger value (up to 3600) for slower operations; an explicit configured
value of `0` retains unlimited waits. Per-call `timeout` can shorten, but not exceed, a nonzero
configured cap; per-call `0` still uses that cap. Existing configurations explicitly saving `0`
must be updated to enable the deadline. Deadline/cancellation failures may retire Dial's owned
browser session; inspect current tabs before further actions and never blindly retry a click that
may already have taken effect.

Action errors (for example, a selector that never becomes actionable) retain Playwright's diagnostic
and preserve the tab. The action deadline leaves a small response margin before the outer watchdog;
only transport failure/cancellation or an unresponsive action requires session retirement.
After successful navigation (`open`, `navigate`, `reload`, `back`, `forward`), background HTTP gets
up to 15 seconds to settle, including response delivery to Chromium and 500 ms without requests.
Other completed actions retain the separate one-second budget. Both budgets respect the remaining
operation deadline, even when the configured action timeout is unlimited. If settlement expires, the result includes
`network_settlement: { settled: false, pending, note }`: the action completed, but application-level
success has not been established. Inspect the page rather than repeating the action.
HTTP jobs are operation-owned, limited to 16 active jobs and 2 MiB of in-flight request data.
Up to 4,096 additional page requests wait for capacity, with an independent 8 MiB bound on queued
request data. The queue is cancelled at operation end. A single operation admits at most 16,384 HTTP jobs.
Jobs are cancelled/joined before the operation returns. Their network waits do not block reading
driver completion/error messages. They retain the guarded host, redirect and body-size policies.
`events.network_failures` retains the latest 100 blocked-request causes separately from routine
network traffic and honors the same `after` sequence cursor. Consult these causes before interpreting
Chromium's generic `ERR_BLOCKED_BY_CLIENT`: a denied endpoint, resource limit, and unavailable server
are different failures. A successful navigation status alone does not prove the application rendered.

Browser `run` executes an async function body in the page realm: use DOM globals such as `document`,
`assert(condition, message)` and `await sleep(ms)`, and `return` a JSON-serializable result.
For example, `assert(document.querySelector('h1')); return document.title;` checks the page DOM.
There is no injected Playwright `page`, `require`, or Node API; page CSP may block dynamic evaluation.
Use browser actions or the allowlisted `call` chain for Playwright-backed operations.

### Cookies, storage, emulation, clipboard and saved state

`cookies` lists cookies for the browser context, filtered by `urls` or `domain`. Cookie values are
redacted unless `include_values` is true; at most 200 cookies are returned. `set_cookies` takes up to
32 cookies, each with exactly one of `url` or `domain`. `clear_cookies` removes `names`, or every
cookie when `names` is omitted.

`storage` reads, writes or clears `local` or `session` web storage for the current HTTP(S) origin
(`area`, `operation`, `key`, `value`). Reads over 20,000 characters are refused; request one key at
a time. `storage` is not available on `about:blank` or other non-HTTP(S) pages.

`emulate` changes the current tab's `device` preset, `viewport` (width and height 1–7680, scale
0.1–4), `user_agent`, `timezone`, `locale` and `color_scheme`. `offline` and `geolocation` apply to
the whole browser context and are reported as `context_wide`. An empty `timezone` or `locale` resets
it. `devices` lists the device presets available in the pinned Playwright runtime; ten device names
from the reference catalogue are not in that runtime and are unavailable.
`open` accepts `viewport` and `user_agent` for the new tab. `ignore_https_errors` is fixed when the
browser session launches: set it on the first `open`, or run `shutdown` before reopening with a
different value. It cannot be used with an attached browser (`DIAL_CDP_URL`), whose TLS settings the
operator owns.

Clipboard actions use the page's `navigator.clipboard` with granted read and write permissions.
`clipboard_write` sets text (up to 64 KiB); `clipboard_read` returns it, truncated with a flag when
larger. `clipboard_copy` and `clipboard_paste` send the platform copy and paste shortcut to the focused
element. No clipboard shim is installed, so the result depends on the browser's clipboard support.

`save_state` stores cookies and origin storage under `name` in the Dial state directory
(`browser/states/<name>.json`). `load_state` restores those cookies and the current origin's
`localStorage`; `sessionStorage` is not saved and is not restored, and other origins are reported
as skipped.

### Captures, recording and performance

`screenshot` accepts `format` (`png` or `jpeg`), `quality` (1–100, JPEG only), `annotate` (overlays numbered boxes on visible interactive elements and returns each label with its CSS selector, which DOM actions accept) and `if_changed` (returns no image when the capture matches the previous one of the same tab and options). `pdf` writes a PDF to the session artifacts directory. `record_start` begins a screencast and `record_stop` encodes it to an MP4 with `ffmpeg`, which must be on `PATH`; a missing `ffmpeg` is reported as an error. `record_start` fails while a recording runs unless `record_restart` is set. `profile_start`/`profile_stop` write a CPU profile (`.cpuprofile`). `metrics` returns the page's Performance counters. `vitals` reports TTFB, FCP, LCP, CLS, INP and load timings from the buffered performance timeline, which can differ from the page's own live observers.
