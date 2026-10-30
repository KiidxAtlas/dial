# Dial Python extension SDK

A Python 3.11+ standard-library-only SDK for Dial's plugin runtime: framed host callbacks,
session workers, declarative workspaces, tools, settings and workflows. It is **not a sandbox**;
install only code you trust. Host tool approval and action validation still apply.

Preferred authoring commands (using your installed Dial binary):

```sh
dial plugin new /tmp/my-extension --name my-extension
dial plugin check /tmp/my-extension
dial plugin install file:/tmp/my-extension
```

Checkout-only equivalents are `PYTHONPATH=python python3 -m dial_extensions new DIR`
and `PYTHONPATH=python python3 -m dial_extensions check DIR`.

`new` refuses existing directories. It creates `dial-plugin.json` and a runnable
`extension.py`, demonstrating a command, interactive panel, persisted counter,
observation, autocomplete and asynchronous custom tool. `check` parses the manifest
and Python source **without importing or executing it**; it does not validate all
runtime behavior or review trust. Installation is explicit and may run the chosen
interpreter and install declared dependencies in the managed environment.
`check` does not inspect or execute decorator registrations, so it does not verify
every tool declaration or namespaced tool bound. Valid long command-only extension
names remain accepted. `new` checks the generated starter's `echo` tool bound before
creating files.

## Registration

```python
from dial_extensions import Extension, Reply, ToolResult

extension = Extension("example", capabilities=["ui.status", "actions.set_model"])

@extension.command("greet")
def greet(args):
    return Reply().text("Hello " + args).ui("status", ["Greeted"])

@extension.on("user_prompt_submit")
def prompt(payload):
    return Reply().rewrite(prompt=payload["prompt"].strip())

@extension.tool("echo", "Return text", {
    "type": "object", "properties": {"text": {"type": "string"}},
    "required": ["text"], "additionalProperties": False,
}, revision=1)
async def echo(arguments, workspace):
    return ToolResult(arguments["text"])

if __name__ == "__main__":
    extension.serve()
```

Decorators preserve the callback. Duplicate handlers are rejected. Signatures:

| Decorator | Callback arguments | Return |
| --- | --- | --- |
| `command(name)` | `args: str` | `Reply`, dict, string or `None` |
| `on(event)` | `payload: dict` | same |
| `panel_close(command)` | `outcome: dict, generation: int` | same |
| `autocomplete` | `query: str` | list of `{value, label?, description?}` or `{items: [...]}` |
| `tool(name, description, parameters, revision=1)` | `arguments: dict, workspace: str` | string or `ToolResult(text, is_error=False)` |

Any callback may be `async def`. `serve()` owns its synchronous process entry point;
it is not intended to run inside an already-running asyncio event loop. Tool names
are local ASCII letters/digits/underscore/hyphen (1–48 characters), with up to 64
tools. Dial exposes them as `ext__<escaped-extension-name>__<local-tool-name>`:
the host escapes owner `_` to `_u` and `.` to `_d`, preserving `-`.
The complete namespaced tool name must fit 128 characters; escaped underscores and
dots each consume two characters. `Extension.tool` checks this against the actual
installed owner supplied by `DIAL_EXTENSION_NAME`, or `Extension.name` when run
outside the host. Both names must follow the manifest name syntax. A host owner
different from the declared name is supported (for example, an installed rename).
Increment the positive revision when identical arguments acquire a new meaning.

Initialization advertises protocol `"2"`, declared capabilities, registered events,
commands and tool declarations. Autocomplete automatically adds its capability.
All other action capabilities must be explicitly declared in `Extension`.
Observational events cannot block or rewrite; use blocking hook names for rewrites.

## Replies

`Reply()` supports chainable helpers:

- `.text(text)`, `.panel(title, elements, generation=0)`
- `.rewrite(payload=None, **fields)`, `.block(reason)` (blocking hooks only)
- `.ui(slot, lines)`; `slot` is `status`, `header`, `footer`, or `title`; `[]` clears
- `.send_user_message(text)`, `.append_entry(kind, text, renderer=None)`
- `.set_model(model, effort=None)`, `.set_thinking(effort)`
- `.register_provider(name, config)`, `.unregister_provider(name)`
- `.action(action_type, **fields)` for protocol-level extensibility

Actions describe requests, not permissions. Declare `ui.<slot>` or
`actions.<action_type>` capabilities. Sensitive changes still require host approval.
The host enforces panel layouts, size limits, provider credentials and reply bounds.
Panels use plain protocol element dictionaries; the generated example demonstrates
an input and submit button. Close outcomes are `submitted`, `cancelled` or
`unsupported`; submissions are human input, **not tool approval**.

## Settings and state

`extension.settings` reads the explicit `DIAL_EXTENSION_SETTINGS` JSON object;
it defaults to `{}` and never discovers settings from workspace files.
`extension.root` reads `DIAL_EXTENSION_ROOT`. `extension.state` uses
`DIAL_EXTENSION_STATE_DIR` (required, with no workspace fallback):

```python
counter = extension.state.load("counter", default={"count": 0})
counter["count"] += 1
extension.state.save(counter, "counter")
# extension.state.delete("counter")
```

JSON writes are atomic replacements; concurrent read/modify/write is not a
transaction and requires author coordination. Missing files use the default;
corrupt JSON raises an error instead of silently resetting data. State filenames
are validated identifiers, not arbitrary paths. Request-mode globals are not persistent;
worker-mode globals last until process restart, not across sessions.

`serve()` selects the host-provided transport: legacy one-request-through-EOF, managed framed
`request`, or persistent framed `worker`. Declare `python.runtime` in the manifest for managed
extensions. Import/callback prints go to stderr; direct native fd 1 writes still break the protocol.
Do not log secrets. Notifications intentionally emit no response.

## Expanded APIs

Use `extension.host` inside a managed callback for approved tools, owned tasks, active-model
structured inference, selected context, settings/secrets, view updates and progress notices.
These requests retain the host's capabilities, schemas, permissions, approvals and cancellation.
Detached threads cannot dispatch host operations; `extension.publish` queues display-only updates.

Registrations include `view`, `settings_schema`, `agent_profile`, `workflow`, `context_source`,
`renderer`, `keybinding` and `preset`. `every` and `watch` run on host polls at idle boundaries;
they do not create an independent background agent or silently run repository code.

See [the authoring guide](../../docs/python-extensions.md) for capability names, bounds,
schemas, lifecycle, typed settings, credentials and CLI commands. Working dependency-free
packages are in [`python/examples`](../examples): PR review, build dashboard and workflow pack.
`dial plugin dev DIR` explicitly runs a temporary hot-reload diagnostic worker;
`dial_extensions.testing.FakeHost` supplies deterministic offline handlers, not live approval proof.

Run isolated local SDK tests:

```sh
PYTHONPATH=python python3 -m unittest discover -s python -p test_extensions.py -v
```
