# Opt-in session preparation

Session preparation is disabled by default. Adding the capability does not edit your settings.
The independent controls are declared in the normal settings schema:

| Setting | Default | Work when explicitly enabled |
| --- | --- | --- |
| `preparation.local_prefix` | `false` | One local, one-output-token prefix warming request |
| `preparation.lsp_paths` | `[]` | Ordinary LSP document-symbol requests for up to eight workspace-relative source files |
| `preparation.cargo_check` | `false` | `cargo check --locked --offline --workspace` |
| `preparation.timeout_secs` | `60` | Per-action deadline, from 1 to 300 seconds |

Preparation runs only at an idle maintenance boundary before the first conversation turn in an
interactive session. Print/hermetic runs, plan mode, busy sessions, live workers and interrupted
sessions do not start it. Each action is attempted once for the current session/root/model/settings
identity. A denial or failure is reported, not silently retried on every idle tick. Resuming a session
with existing conversation history does not replay startup actions.

## Local prefix warming

This reuses the selected resident local backend and the existing local compute lease. It sends the
session's system/project prefix and offered tool schemas, with a one-token generation bound. It does
not send conversation history, execute generated tools, select a fallback, contact a hosted provider,
or download missing model files. Request hooks remain effective, but cannot expand the one-token or
tool-schema bound. Output is discarded; usage is attributed to cache warming rather than a user turn.

This is a **prefix warming request**, not a prefill-only backend API. Whether it improves the next
request depends on the backend's cache behavior; a cache hit or a latency improvement is not promised.
Local serving endpoints are considered local according to the existing backend routing rules, not
because they happen to support an OpenAI-shaped API.

## Language servers and Cargo

Configured LSP preparation uses an ordinary tool request. A setting is not permission to start a
server, install software or bypass the normal approval dialog. Missing files/tools and refused
startup are reported and consumed for this session. The file must resolve inside the current
workspace, not a URL or an added external directory.

Cargo preparation also passes ordinary tool admission, hooks, shell approval and sandbox rules.
It uses the session's Cargo target isolation. `--offline --locked` prevents an implicit dependency
fetch or lockfile update; missing cached dependencies mean preparation did not succeed. Cargo build
scripts still execute project code, so enabling the setting is not treated as a shell grant.

A long Cargo command may become an ordinary session-owned background job according to the existing
shell policy. The preparation notice distinguishes a launch from an observed successful exit; use
`bg` to inspect any continuing command. It remains subject to session teardown, not durable-job
ownership. Cold preparation never satisfies the final focused-verification gate: checks after edits
must establish evidence for their own current inputs.

New foreground input cancels idle preparation through the existing maintenance cancellation owner.
Tool execution that has already entered ordinary background ownership follows that owner's normal
cancellation and teardown rules. Preparation failures do not prevent a subsequent user turn.
