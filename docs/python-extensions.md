# Python extensions

Dial's bundled Python SDK lets operator-installed extensions add approved host calls, persistent
workspace views, tools, slash commands, panels, lifecycle hooks, autocomplete and workflows.
It uses the existing plugin protocol, store, execution controller and `/extensions` browser.
Python 3.11 or newer must already be installed. Dial does not install Python globally.

## Create and install

```sh
dial plugin new ./my-extension --name my-extension
dial plugin check ./my-extension
dial plugin install ./my-extension
dial plugin list
```

`new` refuses an existing destination. The generated example includes a `/hello` command,
a form, a persistent visit counter, a status contribution, autocomplete and an async `echo`
tool. `check` parses the manifest and entry-point syntax without importing extension code,
creating a venv or installing packages. It is not a trust review or runtime certification.
Both authoring commands accept `--python /path/to/python3.11` for their helper interpreter;
the manifest separately selects the extension's interpreter.

Install only code you trust. **Installation and updates may execute dependency build code.**
Starting Dial executes imports and initialization of enabled extensions. Commands and hooks
also run Python with your user privileges; model-tool approval is not a sandbox around that
code. Virtual environments isolate dependencies, not filesystem, network or process access.
Capability declarations constrain host-mediated actions, not ordinary Python APIs.
Do not put secrets in manifests, diagnostics, version control or command-line arguments.

Start a new Dial session after installation, update, enable or disable so its registry reflects
the change. An existing session refuses calls after its Python installation changes, rather than
using stale tool contracts. A persistent worker holds its installation lease even while idle;
stop it with `/extension stop NAME`, close the session or reload plugins before management.
Plugins are never automatically executed because a repository contains them.

```sh
dial plugin disable my-extension
dial plugin enable my-extension
dial plugin update my-extension
dial plugin remove my-extension
# Remote distribution uses the same installer and marketplace indexes:
dial plugin install 'git+https://example.com/team/my-extension.git#v1.0.0'
```

`update` follows the recorded source. Local installations are copies: edit the source, check it,
then explicitly update to publish changes. Failed environment preparation does not replace the
previous installation. Updates preserve enabled/disabled status. Marketplace entries use the
same `file:` or `git+` sources as other plugins; see [Extensibility](extensibility.md#marketplaces-and-extensions).

## Package format

An extension directory contains `dial-plugin.json`, its entry point, and any local modules/assets:

```json
{
  "name": "my-extension",
  "version": "0.1.0",
  "description": "My team's Dial workflow",
  "apiVersion": 1,
  "python": {
    "entry": "extension.py",
    "interpreter": "python3",
    "runtime": "worker",
    "dependencies": []
  },
  "settings": {"greeting": "Hello"}
}
```

`python` is mutually exclusive with legacy `program` and `args`. Its fields are strictly
validated; `entry` must resolve to a file inside the installed extension, never an absolute
path or traversal. `interpreter` defaults to `python3`; `dependencies` defaults to an empty
list and holds pip requirement arguments, not shell commands or pip options. Prefer exact
versions for reproducibility. Relative dependency paths resolve from the staged package.
Dependency installation is explicit and may need network access. With no dependencies, venv
creation skips pip bootstrap. No package downloads occur merely while listing extensions or
resolving an installed runtime.

`runtime` is `request` (default, one managed framed exchange) or `worker` (one process per
session registration). `apiVersion` is `1`, or an inclusive `{ "min": 1, "max": 1 }` range.
Incompatible versions fail before execution. Instead of `dependencies`, `lockfile` names a
confined relative requirements file; dependency installation uses pip's hash-required mode.
Do not combine a nonempty dependencies list with a lockfile. Include exact versions and hashes
for every transitive requirement. Legacy configured stdio programs retain their EOF transport
unless explicitly configured with `DIAL_EXTENSION_RUNTIME=request` or `worker`.

Dial stages a private `.dial-python` directory with the bundled SDK and launcher, and keeps each
venv at a stable store-owned path so dependency console-script interpreter paths survive updates.
`.dial-python` is reserved: do not ship it in source packages. The runtime checks environment
completeness and SDK compatibility; an incompatible/missing environment requires explicit
`dial plugin update NAME`, even if source content is unchanged. Dial never silently repairs or
installs dependencies during startup. Runtime stdout is JSON-RPC; Python prints from imports and
callbacks are redirected to stderr. Direct native writes to file descriptor 1 are not intercepted
and will break protocol replies.

Optional manifest `env` holds explicit string-valued environment overrides. `DIAL_EXTENSION_*`
variables are reserved and cannot be overridden there. The launcher uses Python isolated mode,
ignoring `PYTHONPATH` and user site packages, then adds the bundled SDK and entry directory.
Extensions still have ordinary user-level access; isolation is not a security boundary.

## Python API

```python
from dial_extensions import Extension, Reply, ToolResult

extension = Extension("my-extension", capabilities=["ui.status"])

@extension.command("greet")
def greet(args):
    greeting = extension.settings.get("greeting", "Hello")
    return Reply().text(f"{greeting}, {args}!").ui("status", ["Greeted"])

@extension.on("user_prompt_submit")
def on_prompt(payload):
    # Blocking hooks may rewrite their documented payload fields.
    return Reply().rewrite(prompt=payload["prompt"].strip())

@extension.tool("echo", "Return supplied text", {
    "type": "object",
    "properties": {"text": {"type": "string"}},
    "required": ["text"],
    "additionalProperties": False,
}, revision=1)
async def echo(arguments, workspace):
    return ToolResult(arguments["text"])

if __name__ == "__main__":
    extension.serve()
```

No separately installed SDK package is needed: Dial bundles it into each managed extension.
For direct development from this checkout, `PYTHONPATH=python python3 -m dial_extensions new DIR`
and `... check DIR` expose the same authoring helpers.

| Registration | Callback arguments | Result |
| --- | --- | --- |
| `@extension.command(name)` | argument text | `Reply`, dict, string or `None` |
| `@extension.on(event)` | event payload dict | same |
| `@extension.panel_close(command)` | outcome dict, generation integer | same |
| `@extension.autocomplete` | current query string | list of suggestion dicts or `{items: [...]}` |
| `@extension.tool(name, description, parameters, revision=1)` | arguments dict, workspace path string | string or `ToolResult(text, is_error=False)` |

Callbacks may be synchronous or `async def`. Duplicate registrations are rejected.
Autocomplete automatically declares its capability. Other capabilities are explicit.
Tool names are local; Dial exposes `ext__my-extension__echo` for the example above.
Namespaces escape `_` as `_u` and `.` as `_d`, preserving `-`; final names must fit 128 bytes.
Tools always require the normal approval boundary and are conservatively considered mutating.
Increment `revision` whenever the same arguments can acquire a new meaning. Changed schemas
and revisions participate in session-resume contract validation, rather than silently accepting
old tool calls. A failed/cancelled process or malformed reply after dispatch leaves effects
unknown; inspect state before retrying.

### Replies and host customization

`Reply()` supports fluent `.text(...)`, `.panel(title, elements, generation=0)`,
`.rewrite(...)`, `.block(reason)`, and these action helpers:

| Helper | Required capability |
| --- | --- |
| `.ui(slot, lines)` | `ui.status`, `ui.header`, `ui.footer` or `ui.title` |
| `.append_entry(kind, text, renderer=None)` | `actions.append_entry` |
| `.send_user_message(text)` | `actions.send_user_message` |
| `.set_model(model, effort=None)` | `actions.set_model` |
| `.set_thinking(effort)` | `actions.set_thinking` |
| `.register_provider(name, config)` | `actions.register_provider` |
| `.unregister_provider(name)` | `actions.unregister_provider` |

`.action(type, **fields)` allows raw protocol actions. The host validates capabilities,
shapes and bounds. Sensitive actions are queued for idle-boundary approval; they do not
change a running turn. Provider configuration must reference a credential environment
variable, not contain a literal key. Entries and UI slots are display-only, not hidden
model instructions. Blocking hooks support documented rewrites and context contributions;
observational events cannot block or rewrite. See the [protocol reference](extensibility.md#protocol-2-capabilities-actions-and-ui-slots)
for event names, approval rules, limits and payloads.

Panels use dictionaries for `text`, `markdown`, `list`, `input`, `select` and `button`
elements. The generated starter demonstrates panel creation and close handling. A panel
requires a title and button, with bounded strings, unique interactive IDs and valid choices.
Closing returns `submitted`, `cancelled` or `unsupported`; generation identifies the opened
panel. Submitting a form is human input, not approval to execute tools.

### Settings and durable state

`extension.settings` retains the manifest's explicit settings object for compatibility.
For typed live settings, declare `extension.settings_schema(schema)` and read
`extension.host.settings()` during a callback. Defaults merge with user overrides, then
workspace overrides, stored outside the source tree. Invalid values fail instead of resetting.
`extension.root` is the installed package root; load bundled assets relative to it.

`extension.state` stores named JSON files under the host-provided
`<user_dir>/plugins/.state/<name>/`, separate from version-swapped package contents:

```python
counter = extension.state.load("counter", default={"count": 0})
counter["count"] += 1
extension.state.save(counter, "counter")
# extension.state.delete("counter")
```

Request-mode callbacks run in fresh processes. Worker-mode globals survive until shutdown,
failure or restart, but are not durable state. JSON state writes use atomic replacement;
concurrent read/modify/write is not a transaction and requires author coordination.
Missing files return the requested default; invalid JSON is an error and is never silently
reset. State names are bounded identifiers, not paths. State survives package updates and
removal by default; `dial plugin remove NAME --purge-state` explicitly deletes extension state,
tracked credentials, global settings and the current workspace overrides (not other workspaces).
A missing state environment variable is an error, never a fallback to writing into the workspace.

## Host APIs and managed workers

Managed callbacks may call `extension.host`. These operations return structured results; a tool's
`is_error`, `data` and execution facts are retained instead of treating every failure as an exception.
Transport/admission failures raise `HostError`. Host calls are unavailable from detached threads.

| Python helper | Capability | Contract |
| --- | --- | --- |
| `host.context(entries=None)` | `host.context` (plus `host.conversation` for selected content) | Workspace/session metadata and conversation descriptors; explicit selected content requires approval, excludes reasoning and tool bodies |
| `host.tools()`, `host.tool(name, arguments)` | `host.tools` | Only offered, non-reentrant tools; schema validation and normal approvals/confinement still apply |
| `host.start_task(...)`, `task_status(id)`, `cancel_task(id)` | `host.tasks` | Existing owned task engine; status/cancel only for this registration's tasks |
| `host.model(prompt, schema=None, max_tokens=2048)` | `host.models` | Approved tool-free call on the active model, bounded to 4096 output tokens; optional JSON validated against schema |
| `host.workflow(id, arguments=None)` | `host.workflows` | Declared sequential approval/tool/agent steps through the same controller |
| `host.settings()` | `host.context` | Validated effective public settings |
| `host.secret(name)` | `host.secrets` | This extension's credential namespace, not provider credentials |
| `host.update_view(id, elements, revision=...)`, `remove_view(id)` | `views` | Owned, bounded, revision-checked workspace snapshots |
| `host.progress(text)`, `notify(text)` | `host.context` | Display-only streaming notices, never model instructions |

Conversation identifiers belong to the current session snapshot; request only the entries actually
needed, and do not persist them as permanent archive identifiers. Models, tasks and tool calls can
incur costs or effects; capability declarations and view-button activation are not approval.

For a persistent extension use `"runtime": "worker"`. `@extension.every(seconds)` coalesces missed
timer ticks; `@extension.watch(["src/**/*.rs"], debounce=0.25)` reports relative changed filenames.
Timers and subscriptions run when the host polls at safe idle boundaries, not on hard real-time
deadlines or during a busy model turn. Watches cap matching files at 256 and report truncation.
Background threads may `extension.publish(method, params)` for display-only view/progress/events
updates; the bounded queue drains on the next host poll. They cannot queue tool/model/task calls.
`@extension.on_shutdown` registers cleanup for a graceful protocol shutdown; do not rely on it for
durability after cancellation, transport failure or forced process teardown.

## Workspace views, commands and settings

`extension.view(id, title, elements=[...], location="sidebar"|"tab")` registers a persistent surface
and an `event` callback. Components are `text`, `markdown`, `code`, `diff`, `list`, `table`, `tree`,
`tabs`, `progress`, `button`, `input` and `select`. They are data, never raw terminal escape code.
Views are limited to 64 KiB, 256 nodes and eight levels; the host owns layout, focus and scrolling.
Events contain `type`, `node` and optional `row`/`value`; the surrounding request identifies the view
and revision. Updating returns the new revision; retain it for the next update. Stale events and
updates are rejected rather than reaching a new registration. See the working examples for shapes.

Use `/extension-view list`, then `/extension-view owner/id [show|hide|focus]`. Escape returns focus
to the composer. The separate `/watch` pane consumes snapshots only; it cannot approve tool calls.
`ToolResult(text, data=..., renderer="id")` selects an `extension.renderer` declaration for bounded
JSON Pointer field projection without losing structured tool data.

Commands can declare `description`, an object `parameters` schema and `keybinding`; command
callbacks still receive argument text. Built-in commands and shortcuts always win. Use
`/extension-keybindings [global|workspace] list` to inspect conflicts, or
`/extension-keybindings [global|workspace]` to open the shortcut form. Select a command and save
a Ctrl/Alt chord; an empty shortcut disables its binding. Plain typing is never an extension
shortcut. Workspace overrides take precedence over global overrides.

Use `/extension-settings owner [global|workspace]` for the schema-backed form. Edits affect only
the selected layer; defaults and values inherited from another layer are not copied into it.
Secrets marked `writeOnly: true` or `format: "password"` are excluded from settings and presets;
use credential storage, never manifests. CLI equivalents include:

```sh
dial plugin settings NAME get
dial plugin settings NAME set KEY '"JSON string"'
dial plugin settings NAME secret-set TOKEN_NAME  # reads stdin, never a command-line value
dial plugin presets NAME export ./public-preset.json
dial plugin presets NAME import ./public-preset.json
dial plugin diagnostics NAME
```

The extension must initialize once to register its settings schema. `extension.preset(id,title,values)`
declares public settings packs; exported/imported values are validated and never include secrets.

Secret-bearing settings schemas use literal `properties` and homogeneous `items` only.
References/definitions, combinators, dynamic property schemas and tuple/prefix/contains schemas
cannot safely classify secret fields and are refused for secret-bearing settings. Root secret
annotations and public defaults/choices/examples containing nested secrets are also refused.
Public-only settings and command schemas retain their normal supported JSON Schema subset.

## Profiles, workflows and context

`extension.agent_profile(...)` declares guidance, an optional configured model selector and a
scope-lowering tool list. It inherits the existing task harness and permissions; it cannot grant
tools or access unavailable to the parent. `extension.workflow(...)` registers visible `approval`,
`tool` and `agent` steps. `extension.context_source(...)` registers an explicitly selectable bounded
context callback. Context retains extension provenance and is untrusted input, never system authority.
The `before_compact` blocking hook may return `additional_context` for the next explicit
summary operation: attributed untrusted data (4 KiB per answer, 8 KiB aggregate), included before
summary request budgeting, never promoted into system instructions or retained conversation.
Refusal/fail-closed hooks discard contributions. Background compaction does not execute this hook.

```text
/extension list
/extension workflow workflow-pack/review
/extension profile workflow-pack/reviewer Review current changes
/extension context workflow-pack/preferences Security risks
/extension diagnostics
/extension restart NAME
/extension stop NAME
```

## Development and examples

`dial plugin dev ./my-extension` explicitly executes trusted development code in a temporary
managed store, watches bounded source changes and restarts the worker. `--once` checks one runtime
initialization/poll and exits; `--interval-ms 500` controls the watch interval. This CLI is a
diagnostic/display preview, not an agent session: effectful host APIs are unavailable there.
Use an installed extension in Dial for real approval/tool/task/model integration. After publishing
changed contracts use `/reload-plugins` (or a fresh session); do not hot-swap tool identities in flight.

`dial_extensions.testing.FakeHost` provides explicit handlers and fail-closed fake approvals for
offline SDK tests. It proves SDK behavior, not real controller approval or provider compatibility.

Three dependency-free packages live in `python/examples/`:

- `pr-review`: explicit diff and approved model review, displayed in a persistent review tab.
- `build-dashboard`: approved build/test buttons and watched-source stale-result notices; watching
  does not automatically run a build.
- `workflow-pack`: personal review task profile, approval checkpoint, workflow, context source,
  shortcut and public preset.

Review each package, then `dial plugin check python/examples/NAME` and explicitly install it.
Examples do not auto-install, contact providers or start tasks merely on initialization.

## Boundaries and diagnostics

Requests retain the extension deadline (`extensions.timeout_secs`), bounded frames and cancellation.
A failed worker is not silently retried: use `/extension restart NAME`. Restart requires unchanged
declarations; changed contracts need an explicit plugin reload. Forked sessions receive fresh
workers. Unload/session close revokes adapters and terminates owned worker processes.
Callback exceptions produce JSON-RPC errors, with traceback diagnostics on stderr; do not log
secrets. Static checks do not execute code or prove a dependency can import.

This SDK exposes supported host customization, not unrestricted mutation of Rust internals.
Raw terminal drawing and keystroke interception are not supported; use bounded declarative views.
Python is trusted user code, not a sandbox. Capability declarations do not grant permissions.

Developer checks (offline, no package downloads):

```sh
PYTHONPATH=python python3 -m unittest discover -s python -p test_extensions.py -v
cargo test --locked -p dial-tools --lib tools::integrations::plugin
cargo test --locked -p dial-app --lib cli::plugin
```
