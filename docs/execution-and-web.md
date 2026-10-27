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

Python kernels retain their namespace between successful cells. Interrupting a running cell first sends SIGINT; an interrupt that does not unwind within five seconds escalates to terminating the kernel. Failed transport, cancellation and timeout paths discard the namespace. Effects already performed remain; loss of the final cell result is recorded as uncertain. Kernel interpreters may be selected from the project virtual environment (`.venv`/`VIRTUAL_ENV`) or conda environment when configured/approved, but execution remains subject to the mandatory process-creation fence.

`kernel` supports macOS process-fork containment and the Windows native no-child-process mitigation;
Windows additionally requires the explicit unsandboxed setting because filesystem/network isolation
is unavailable. `eval` supports macOS only. **Both fail closed on Linux**, even when Bubblewrap is
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
the existing canonical schema catalog. Saved integration lists still retain browser, kernel, eval,
find, autoresearch and archive; computer and notebook remain opt-in. These declarations describe
availability only and never grant execution authority.

## Browser

Browser sessions support headless launch (the default), optional CDP attachment to an existing browser, and isolated persistent storage state per profile. Browser actions include snapshots, ARIA snapshots, accessibility audits, tracing, and action recording. Linux launches use Bubblewrap containment. Loopback WebSocket connections are permitted for local development servers (including HMR) without disabling the outbound network guard. Orphaned tabs from prior processes are reaped rather than accumulating.

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

Search uses `web.search_provider` or a per-call provider selection. Supported backends include Brave, Tavily, Exa, Serper, SearXNG, Kagi, Perplexity, Jina, Google Custom Search, Bing, Firecrawl (`FIRECRAWL_API_KEY`), Parallel (`PARALLEL_API_KEY`), Z.AI (`ZAI_API_KEY`), xAI (`XAI_API_KEY`), and the keyless DuckDuckGo, Startpage, Mojeek and Ecosia HTML scrapers. `auto` tries configured keyed providers first and the keyless ones last; a scraper that meets a captcha, firewall or proof-of-work wall fails with an error so `auto` moves on. Provider credentials are supplied through their documented environment variables; no key is embedded in configuration or sent to a different origin. A custom `web.search_url` template remains supported and must contain `{query}`.

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
no operator setup is required. Cached JavaScript dependencies can be used offline with an installed
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
paused between approved browser operations. Python and
Poppler must also be installed when using kernel/PDF capabilities.

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
