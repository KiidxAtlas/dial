# Execution and web tools

## Persistent kernels and evaluation

Python kernels retain their namespace between successful cells. Interrupting a running cell first sends SIGINT; an interrupt that does not unwind within five seconds escalates to terminating the kernel. Failed transport, cancellation and timeout paths discard the namespace. Effects already performed remain; loss of the final cell result is recorded as uncertain. Kernel interpreters may be selected from the project virtual environment (`.venv`/`VIRTUAL_ENV`) or conda environment when configured/approved, but execution remains sandboxed. Linux execution uses Bubblewrap containment where available. JavaScript evaluation can resolve packages from the managed evaluation package directory; installing a missing package is an explicit approval-gated operation.

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

Fetch handles supported binary documents (including PDF and Office formats) with the document converter. For HTML, it probes declared Markdown alternate links and `.md` siblings, tries Markdown content negotiation, then configured readers with a quality gate, then `llms.txt` candidates, before using built-in HTML conversion. Optional external readers are configured through `web.reader_chain`; they receive HTML on standard input. Common GitHub, npm, crates.io, PyPI, and Stack Overflow URLs use public APIs when available, with ordinary fetching as fallback.

Search uses `web.search_provider` or a per-call provider selection. Supported backends include Brave, Tavily, Exa, Serper, SearXNG, Kagi, Perplexity, Jina, Google Custom Search, Bing, and DuckDuckGo fallback. Provider credentials are supplied through their documented environment variables; no key is embedded in configuration or sent to a different origin. A custom `web.search_url` template remains supported and must contain `{query}`.

## Coordinated edits

All native file commits take reentrant writer leases for their canonical document paths. Multi-document batches acquire them in one sorted order; recursive acquisitions that invert that order are refused. Separate Dial processes coordinate through the same OS file locks, released if their owner exits. Unrelated documents remain independently available. Ordinary text proposals can merge non-overlapping changes made after preparation. The merged proposal passes the existing syntax, permission, witness and transaction checks before commit. Merging never grants read coverage over another writer's unseen lines.

Overlapping edits, ambiguous insertion boundaries, whole-file replacements and projected documents require a fresh read. External editors and overlapping additional roots are still protected by exact source-identity checks; the lease does not claim to coordinate arbitrary external programs. Multi-file transaction rollback and observer behavior remain in `edit_transaction.rs`.

## Debug adapters and MCP lifecycle notifications

`[dap.<id>]` defines an operator-owned adapter executable. The `dap` integration exposes
`dap_<id>` actions and requires approval on every call; adapter startup is lazy, and launch program
and cwd paths must resolve inside the workspace. `[mcp.<id>].lifecycle_hooks` opts into
`turn_started`, `turn_finished`, `tool_started`, and `tool_finished` notifications through
`notifications/dial/event`; payloads contain identifiers/status only, never prompts, arguments,
outputs, or source content. Hook delivery is observational and cannot bypass tool approvals.

## Browser provisioning and local hosts

The browser tool provisions its own pinned Playwright-core, Readability, and Chromium into a private
owner-only cache on first use (`bun install --frozen-lockfile --ignore-scripts`, then Chromium install);
no operator setup is required. Set `[browser] playwright_module` only to override with an operator-owned
Playwright entry (absolute path, Playwright-core >=1.48); Readability and Chromium fallback still come from the
built-in runtime. Approved browser `open`/`navigate`
requests to `localhost` or `127.0.0.1` automatically permit only that exact host and port for the browser
session, so local development sites work without configuring each port. Redirects and subresources
cannot grant access to another local endpoint. `/settings` → Tools → Browser controls this behavior;
`auto_localhost=false` restores explicit allowlisting with `allowed_hosts=["127.0.0.1:3000"]`.
Other private hosts still require operator allowlisting. Metadata/link-local addresses remain forbidden.
Use managed `bg` processes for previews: list/reuse a running server, start one if needed, wait for
readiness, then open its HTTP URL. Foreground `bash` kills servers at its deadline. The browser accepts
`about:blank`, and tab IDs may be omitted when exactly one tab is open; multiple tabs require an ID.
Browser failures show the underlying network refusal before Chromium's generic error. Networking is
paused between approved browser operations. Python and
Poppler must also be installed when using kernel/PDF capabilities.
