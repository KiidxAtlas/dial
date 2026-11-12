# RPC and ACP

`dial rpc` speaks the newline-delimited JSON control protocol on stdin/stdout; `dial acp` exposes ACP v1 over stdio. Stdout is owned by one blocking writer thread. It uses a private unlinked spool when the consumer falls behind, preserving complete ordered frames without changing shared descriptor flags. Protocol producers reserve positions before mapping events, so concurrent notifications and replies cannot reorder.

RPC query methods remain available during active turns; commands that mutate or interrupt the active conversation retain their turn semantics. ACP requests are served by one core worker that runs one turn at a time: a prompt for a session other than the active one is refused until the running turn ends, and the running turn is never cancelled on another session's behalf. ACP permission requests are sent to the client as `session/request_permission`. Negotiated client filesystem methods are supported: when the client offers `fs.writeTextFile`, plain text writes use `fs/write_text_file`; with `fs.readTextFile` too, reads use `fs/read_text_file` so editor buffers are visible. Terminal, authentication and MCP bridge methods remain unsupported and are refused rather than executed with ambient authority. See the [client parity matrix](client-contracts.md#client-parity) and [uncertain editor writes](#uncertain-editor-writes).

[rpc-schema.json](rpc-schema.json) is a hand-maintained JSON Schema (draft 2020-12) of the plain `dial rpc` request, response and event frames; ACP uses JSON-RPC 2.0 and is not covered. Tests in `crates/dial-app/src/rpc/tests.rs` fail when its request methods, event names or limits drift from the implementation (method names are checked against the dispatch arms, event names against every emitted `"event"`). Print mode (`dial --print`) answers exactly one prompt per invocation: the positional words are joined with spaces and piped stdin is appended as context. `--image <path>` (repeatable) attaches images to that prompt through the same pipeline as `@path` in the TUI; if the selected model does not accept image input, the run fails before any request is sent. `--image -` reads one image from stdin instead (PNG, JPEG, GIF or WebP, detected from the leading magic bytes; oversized or non-image input is refused before any request is sent). Stdin then carries image bytes and is not appended as prompt text; a terminal stdin is refused, and at most one `-` is accepted. Stdout is drained before exit. `stream-json` omits repeated full-history snapshots.

## Print mode and session automation

`dial -p --output-format json "prompt"` emits one result object; `--output-format stream-json`
emits newline-delimited system, text, tool, todo, notice and usage events followed by the result.
JSON modes reserve stdout for JSON. Results include session ID, answer, tool outcomes, usage and duration.
Use `--allow read,edit` for named tool approval instead of blanket `-y`; read-only mode and deny rules
still win. With both a prompt argument and piped stdin, stdin is appended as fenced context.
Print runs offer a smaller tool set than the terminal: `ask` is disabled, and `task`/`message`,
`todo`, `find` and `memory` are listed as loadable instead of offered,
unless `task.prefer_subagents=true` keeps `task`. The model can still `tool_load` any of them.
`eval` stays in the initial offer whenever containment and a cell runtime are available and it
fits the route's core budget (always on hosted routes).
Session `bg` jobs end when a print run exits. Before the run ends, Dial tells the model once which
explicitly started jobs are still running, and refuses `finish` once for them. A server the task
needs afterwards should be restarted with `bg persist=true`; otherwise the model should stop it.
Exit status: `0` completed, `1` error, `2` incomplete, blocked, needs input, no answer or interrupted, `3` a required tool approval was denied (takes precedence over 2).

`--output-schema FILE` makes the final answer structured: FILE is a JSON Schema checked before any request
(an unreadable, malformed or invalid schema fails the run with no model call). The answer is validated with the
same validator and bounded correction budget (`task.max_schema_retries`) that subagent `output_schema` uses;
fenced JSON is accepted. On success stdout carries the validated compact JSON (text mode prints it; `json` and
`stream-json` put it in a new `structured` field of the result object, next to `text`). A still-invalid answer
prints nothing, reports the validation error on stderr (and in the result's `error` field), and exits `2`.
`-o FILE` / `--output-last-message FILE` also writes the final answer (the validated JSON when a schema is in
force) to FILE through an atomic replace. When the turn produces no answer, FILE is neither created nor
overwritten. The flags need `-p`. Both are root-only: `dial sessions pack` keeps its own `-o`.

`dial review [--uncommitted | --base BRANCH | --commit SHA | PROMPT...]` runs the TUI `/review` workflow
non-interactively. With no target it reviews the uncommitted changes. The four target kinds are mutually
exclusive (clap rejects combinations), and refs must be single words. The target is frozen by the same review
code the TUI uses. The run is read-only: the approval policy is forced to `readonly` and reviewers have no
write tools. It honours `--output-format` (on the subcommand) and `-o`, and uses the print exit codes.

`dial sessions` lists this project's sessions by latest activity (`--all` lists every project). On a
terminal it prints a table (id, relative age, message count, title, and project with `--all`) with
the resume and export commands; piped output keeps the stable
`id  started  N msgs  title[  project]` lines for scripts. `dial sessions rename <id> <title>` and
`dial sessions export <id> [path] [--html]` expose session management without the TUI.

## Execution tree and result revisions

Execution uses shared, revisioned result bodies backed by the session's branch-aware JSONL journal.
RPC `state` exposes a `tree` containing session, conversation and execution nodes. Each execution
node has an `id`, `parent`, `kind`, `label` and `result` (`text`, optional structured `data`, `phase`,
`revision`). `state_reset` clears the current observer view; `state_node` replaces one node whenever
its metadata (label, kind, parent, call ID, presentation, phase, execution or structured data) changed,
and `state_delta` appends text to a running node at the supplied revision only when the text alone
grew. Worker updates use bounded
replacement snapshots. Legacy events remain available; clients should choose one projection to
avoid displaying a token twice. Tool-result events include original structured `data` when supplied.
Snapshots are written at boundaries, not for every token. Resuming never restarts unfinished workers.
Unassigned child roles inherit the parent model, profile and request controls; the model picker can
still assign an explicit child model.
Ctrl-C interrupts silent HTTP response reads without replacing the configured inference deadline with
a short timeout. Connection establishment and partial writes still use their configured deadlines.

RPC `state` replies come from one worker-owned projection, the same one the worker uses, and never include message history; read `messages` or `messages_page` for history. Typed RPC `notice` events keep their plain `message` and add `kind`, `lane` (`context`, `divider` or `alert`) and `tokens` as additive fields. Plain notices remain valid without these fields.

`messages_page` accepts an unsigned `offset` or a decimal-string `cursor` (offset takes precedence),
and an unsigned `limit` defaulting to 50 and clamped to 1–500. Invalid types, malformed cursors and
offsets past the snapshot end are errors, not a silent restart at page zero. An offset at the end
returns an empty page with no next offset.

## Persistent RPC and ACP transports

`dial rpc` is a persistent version-1 JSON-lines control endpoint. Requests have `id`, `method` and `params`;
responses correlate by `id`, while unsolicited events have `event`. Methods include `send`, `state`,
`messages`, `compact`, `seed_context`, `new_session`, `resume`, `rename`, `plan`, `steer`, `abort`,
`task_start`, `task_status`, `task_message`, `task_cancel`, `approve` and `shutdown`. Also: `set_model` (`provider`, `model_id`; the model must be in the bundled catalog, one of the configured current/role/favorite/profile models, a downloaded local model, or a chat model in a provider listing saved by an earlier `/models` refresh; models whose metadata declares embedding, rerank, image or audio use are refused, and any other selection is an error that leaves the model unchanged. Remote provider listings are not queried), `set_thinking` (`level`, one of the reasoning-effort names, or `default` to drop the session's level so the model's configured or catalog default applies again; unknown names are errors. A model without a `[model_profiles]` entry can still take a level when the bundled catalog or its saved provider listing declares reasoning levels; the profile Dial builds is session-only), `get_session_stats` (session ID, message count, cost, usage and metrics), `get_available_commands` (the built-in slash commands with `kind` `builtin`, then this project's custom commands (`custom`) and enabled skills (`skill`, named `skill:<name>`)), `export` (`format` `html` or `markdown`; optional relative `path` without `..`; default `dial-session-<id>.html` or `.md` in the project root; returns the written `path`), `set_host_tools` and `host_tool_result` (see below).

`send` text and ACP prompts that start with a custom command (`/name args`) or a skill (`/skill:name task`) are expanded exactly as in the terminal before the turn starts; an unknown skill is an error. A built-in command name (`/compact`, `/model`, …) is refused with an error naming the alternative, rather than sent to the model as if the command had run; clients use the matching method. Text that only happens to start with a slash (a pasted path) is sent as typed.
Live voice (`docs/voice.md`): `live_start` (optional `voice`, `instructions`), `live_mute` (`muted`), `live_stop`, and `live_listen` (records one utterance and runs it as a turn; not available over ACP). While a live session is active, `send` text gets its instructions, and each finished answer is read aloud.
Approval responses must name the currently pending `approval_id`; future/stale/replayed decisions fail.
The `approval` event carries `approval_id`, `tool`, `summary` and, when the request belongs to a call card, the call's `call_id`. `approval_resolved` (`approval_id`) follows once the request is decided, so clients can retire it.
Abort and task messaging/cancellation remain usable while inference or an approval wait is active.
Consumers must handle out-of-order response IDs and asynchronous events.

Host tools (`host_tools` capability, native `dial rpc` only): the host declares tools with `set_host_tools` (`tools`: 1–32 entries, each `name` matching `^[a-z0-9_]{1,64}$`, a 1–1024 character `description`, an object `parameters` JSON Schema and an optional `timeout_ms` from 1 to 600000, default 60000; names must be unique and must not collide with built-in tools). A second declaration in the same session is refused, and a refused declaration is not registered. Host tools belong to the session they were declared in: `new_session`, `resume` (of another session), `fork_session` and `close_session` (of the current session) drop them from the session being left and clear the registration, so the host declares again for the session it wants. Parked sessions never hold host tools. The model sees them as ordinary tools with their own schemas and contract identity. Each use needs normal approval (`approval` event) before the host is asked. When the model calls one, the server emits `host_tool_call` (`call_id`, `tool`, `arguments`) and the host answers with `host_tool_result` (`call_id`, `output`, optional `is_error`). Answers name a call that is still pending; stale or unknown `call_id`s are refused. A call with no answer within `timeout_ms` fails with a timeout error to the model; aborting the turn fails the waiting call as cancelled, and so does a session switch that drops the tools. `host_tool_result` is handled by the controller, so it works while a turn is running. ACP does not expose `set_host_tools` or `host_tool_result`: ACP refuses them as unknown methods (`-32601`), and no host tools are offered in ACP sessions.

Host URI schemes (`host_uris` capability, native `dial rpc` only): `set_host_uri_schemes` (`schemes`: up to 16 unique names matching `^[a-z][a-z0-9+.-]{0,31}$`) declares the schemes the host serves. A later call replaces the list, and an empty list stops serving. The built-in and reserved schemes (`file`, `ssh`, `pr`, `issue`, `conflict`, `artifact`, `http`, `https`) are refused. Reading `<scheme>://…` with the `read` tool sends a `host_uri_request` event (`call_id`, `scheme`, `operation` `read`, `url`); the host answers with `host_uri_result` (`call_id`, `output`, `is_error`). Outputs are capped at 256 KiB, and an oversized output is an error. A read with no answer within 30 s, or one that is interrupted, fails. Reads of host schemes need approval, and `offset` and `limit` are rejected. Declarations are dropped on the same session switches as host tools.

Sign-in (`auth` capability, protocol 2 and native `dial rpc` only): `auth_providers` lists providers (`provider`, `name`, `signed_in`, `needs_terminal`). `login` (`provider`) starts a sign-in; its response, sent when the flow finishes, is `{provider, message}`. While it runs the server emits `auth_url` (`provider`, `url`, `instructions`), `auth_progress` (`provider`, `message`) and `auth_prompt` (`provider`, `prompt_id`, `message`, `placeholder`, `allow_empty`). Answer a prompt with `auth_code` (`prompt_id`, `code`). `auth_cancel` (`provider`) cancels a running sign-in. `logout` (`provider`) cancels a running sign-in for that provider, then signs out and replies `{provider, message}`. Only one sign-in runs at a time, and an unanswered prompt times out after 600 s. Tokens and entered codes are never echoed: the prompt event carries only its `prompt_id`, and the answer is not repeated in any event or result. Providers that need a terminal are refused.

Session administration, models and settings (native `dial rpc` only for the `session_admin` and `settings` groups). These controller methods answer while a turn runs:

- `list_sessions` (`offset`, `limit` 1–500 default 50, `include_empty`): this project's sessions in picker order, pinned first then latest activity. Each entry has `session_id`, `title`, `started`, `updated`, `messages`, `user_messages`, `model`, `parent_id`, `first_message`, `last_message`, `last_answer`, `pinned`, `current` and `open_elsewhere`. The page has `offset`, `total` and `next_offset`. Sessions without a user request are omitted unless `include_empty` is true; the current session is always listed. Over ACP, `session/list` keeps its own ACP shape.
- `pin_session` (`session_id`, `pinned`): idempotent; `changed` reports whether anything changed.
- `delete_session` (`session_id`): removes another session of this project together with its pin. The active session and journals held by another writer are refused.
- `fork_session` (optional `session_id`, default the active session): an actor method like `resume`, refused while a turn runs. It forks the session into a new one and switches to it, running the same branch and switch hooks as `/fork`. Returns `session_id`, `forked_from` and `plan_mode`.
- `list_models` (optional `provider`): every model `set_model` accepts without a network round trip. That covers the main, current, role, favorite and profile models, the bundled catalog of the providers they and `[providers]` name, downloaded local models, and the saved `/models` listings of those providers. Models whose metadata declares a non-chat kind (embedding, rerank, image, audio) are left out. Each row has `provider`, `model_id`, `label`, `sources`, `current` and `thinking_levels`, plus catalog or local metadata when known (`context_window`, `max_output_tokens`, `vision`, `reasoning`, `default_thinking`, `pricing`, `kind`). Live provider listings are not queried.
- `settings_schema`: the settings reference (`dial config reference`): each setting's `path`, `label`, `description`, `kind`, `constraints`, `default` and `restart`.
- `get_settings` (optional `key`): every effective setting as `{key, value, redacted}`, or `{key, value}` for one key. Credentials are redacted.
- `set_setting` (`key`, non-null `value`, `scope` `global` or `project`, default `global`) and `reset_setting` (`key`, `scope`) use the same validation and locked write as `dial config set`/`reset`. They change the saved files only: the running `dial rpc` keeps its launch configuration (`restart_required`). Values are never echoed. To apply saved settings, restart the engine into the same session: read `state.session_id`, shut the process down once no request is outstanding, launch a new `dial rpc` and `resume` that session. `dial-client`'s `Client::restart_and_resume` does exactly this. Approval and question ids from the old process are void.

`state` also carries `current_model` (`provider`, `model_id`, and `thinking` with the explicitly chosen `level` (null when none), `default_level` (the catalog's default when no level is chosen), the declared `levels`, and `settable`, which is true when `set_thinking` can choose a level for this model).

Checkpoints and undo (native `dial rpc` only, `checkpoints` group). These call the same owners as `/review`, `/changes` and `/undo`, through the session's checkpoint journal:

- `list_checkpoints`: checkpointed turns that changed files, newest first, each `{turn, label, files}`. A session without checkpoints lists none.
- `checkpoint_files` (`turn`): the files that turn changed, each with `path`, `writers`, `later_turns` and `overlap` (a reason string when another writer or a later turn touched the file, else null). Also returns `label` and `skipped`.
- `checkpoint_diff` (`turn`, `path`): the diff restoring that file would apply, and a `token` binding it to the file's current bytes.
- `restore_checkpoint_files` (`turn`, `files: [{path, token}]`, optional `overlap_confirmed`): restores the reviewed files, all or none. A stale token, or an overlapping file without `overlap_confirmed`, is `conflict`, and nothing is written.
- `undo` (optional `turns`, default 1): an actor method like `/undo`, refused while a turn runs. Restores the files of the dropped turns, then rewinds the conversation, keeping a summary of the abandoned branch. Returns `files` (`restored`, `removed`, `warnings`), `dropped`, `note`, `rollback_error`, `rewind_error`, `cleanup_error` and `conversation_kept` (true when a file rollback failed and the conversation was left intact). A session-change hook veto is `refused`. Nothing left to undo returns `dropped: 0`.

Extensions (native `dial rpc` only, `extensions` group): `list_extensions` returns the rows `/extensions` shows, `{mcp: [row], skills: [row]}`, where each row has `id`, `title`, `summary`, `tags`, `origin` (`installed`, `curated`, `remote`, `foreign`), `state` (`connected`, `needssignin`, `failing`, `disabled`, `enabled`, `available`) and `details`. It is read-only: installing, enabling and MCP sign-in remain terminal flows.

### One process per session

Each `dial rpc` process is the single writer of its active session's journal. A GUI with several tabs runs one engine process per open session. A second process that resumes a session already open elsewhere gets a fork (`forked_from` names the original) instead of sharing the journal, `list_sessions` marks such sessions `open_elsewhere`, and `delete_session` refuses them with `busy`. The owning process continues undisturbed.

### Event envelope

Every plain-RPC event (not replies, not ACP frames) carries `seq`, `session_id` and `request_id`:

- `seq` starts at 1 with `ready` and increases by one per event in stdout order. A gap means frames were lost: refresh `state` and `messages_page` instead of trusting an incrementally built view. `dial-client` counts gaps in `Client::lost_events`.
- `session_id` is the session the engine was serving when it produced the event. An event that already names a session keeps its own value.
- `request_id` is the id of the request whose processing produced the event, or null for background work between requests.

`ready` also carries `engine_version`, the engine build version. Use it for display and diagnostics. Decide features from `methods` and `capabilities`, never by comparing versions.

Tool cards: after `tool_call`, the engine emits `tool_card` (`call_id`, `label`, `target`, `tone`, `body`, `facts`, optional `preview`), the same card the terminal renders. It can be sent again for the same `call_id` as the call progresses; the latest card replaces the earlier one. `tool_result` adds `status` (`ok`, `error`, `blocked`, `skipped`, `timeout`, `aborted`), `effects` (`none`, `known`, `unknown`) and `changed_paths` (absolute paths). Render these fields instead of parsing `summary` or `text`.

When a client stops reading stdout, the engine queues frames (in memory, then spooled to disk) rather than dropping them. The client receives them in order once it reads again.

### Ready frame

The first event on every connection is `ready`. Besides `protocol_version` (the version this connection speaks: `1` until the client negotiates otherwise), `session_id`, `model`, `tools`, `image_input` and `question_input`, it carries what a client needs to negotiate and size its requests:

- `supported_protocol_versions`: array of versions the server serves (currently `[1, 2]`). Refuse to continue when none is one you implement.
- `methods`: request methods this transport accepts. `capabilities`: the capability groups those methods belong to (`prompt`, `query`, `plan_mode`, `sessions`, `session_admin`, `maintenance`, `context_seed`, `tasks`, `steering`, `abort`, `approvals`, `questions`, `live_voice`, `model`, `export`, `host_tools`, `host_uris`, `auth`, `settings`, `checkpoints`, `extensions`, `shutdown`). Over ACP the `questions`, `live_voice`, `host_tools`, `host_uris`, `auth`, `session_admin`, `settings`, `checkpoints` and `extensions` groups are absent. The method table, capability groups and error codes are defined once in the `dial-protocol` crate, which the engine and `dial-client` share.
- `limits`: `max_request_bytes` (1 MiB for RPC, 8 MiB for ACP; a longer line is drained and answered with an error), `max_pending_steering` (8), `max_seed_messages` (200), `max_seed_message_bytes` (1 MiB), `max_memory_command_bytes` (8192), `messages_page_default` (50), `messages_page_max` (500) and `tool_result_event_bytes` (24 KiB; `tool_result` event text is clipped, the model-visible result is not).

Protocol negotiation (`hello`, `params.protocol_version`): send one `hello` as the first request to move the connection to the highest version you speak. The response carries the negotiated `protocol_version` and `supported_protocol_versions`. A client that never sends `hello` stays on version 1 and sees exactly the v1 frames. Version 2 changes one thing: failed requests carry a stable `error.code` next to `error.message`. Codes are:

- `busy`: another operation owns the session (a turn or maintenance is running, the session is closing) or the steering queue is full. Retry after the current operation settles.
- `invalid_params`: the request is malformed or a value is out of range (missing `text`, `approval_id` or `protocol_version`, an unknown approval `decision`, an invalid history cursor, an unparsable question payload or answer, a built-in command name sent as a prompt).
- `not_pending`: the targeted turn, approval or question is not, or no longer, pending (`steer` with no active turn, a stale or already answered `approval_id`, a mismatched, unacknowledged, expired or replayed question). Drop the stale card rather than retrying.
- `not_found`: the named session, setting, checkpoint turn or file does not exist.
- `refused`: a policy or hook declined the operation (for example a session-change hook vetoing `undo`). Nothing changed.
- `conflict`: the target changed since the client last read it (a stale checkpoint review token, an unconfirmed overlap). Read again before retrying.
- `unknown_method`, `unsupported_protocol_version` (no served version is at or below the requested one) and `internal_error` (the actor failed; the session is terminated).
- `failed`: every other request failure.

Version 1 errors carry `message` only. Version 2 clients should branch on `code`, not on message text, and treat unknown codes as `failed`.

For Rust frontends, the `dial-client` crate implements this negotiation, request correlation and disconnect accounting; see [client contracts](client-contracts.md#rust-client).

All three are derived from the same constants the reader and handlers enforce.

`dial acp` exposes the ACP initialize/session/prompt/cancel/permission/mode flow over bounded JSON-RPC
stdio with streamed assistant/tool updates. Loaded sessions retain exclusive writer ownership; parked
sessions are not concurrently writable. ACP does not advertise unsupported filesystem/terminal
capabilities. Both automation transports use bounded serialized stdout and stop honestly on stalled
or broken consumers instead of reporting partially delivered success.

The implementation keeps the controller (`rpc.rs`) separate from the agent-owning actor
(`rpc/actor.rs`). Only the actor mutates sessions or performs model/tool work; the controller
serves cached queries, routes approvals and questions, and initiates cancellation/cleanup.
Each parked ACP session owns its tools and plan mode in one record, so switching and closing
cannot leave independently keyed execution state behind. This separation does not change wire
methods, approval authority, queue limits or cancellation deadlines.

Every activation runs the cancelable `session_before_switch` hook before cancelling
outgoing work or removing a parked session, including implicit ACP selection by a
session-scoped request. A veto leaves the active session and parked writer intact.
Forking runs both the branch and switch gates. Activation restores only the incoming
session's saved, still-existing directory roots and rebinds its checkpoint observer;
outgoing session roots, sensitive native-access grants and file handles do not carry
over. Invalid incoming workspace/checkpoint setup fails before outgoing work is torn down.

ACP renders `notice`, `background` and `fatal` events as unchanged `agent_message_chunk` text. Each chunk also carries standard `_meta.dial` with `event` and, only when present, `kind`, `lane`, `tokens`, `name` and `replace`, so clients can tell sources apart. This is metadata preservation only; ACP does not implement client-side replacement of background output.

ACP sessions offer three `configOptions`: `mode` (Agent or Plan), `model` (values are `provider/model` from the same offline listing as `list_models`) and, when `set_thinking` can choose a level for the model, `thought_level`. Its first choice, `default` ("Model default", with the catalog default level in parentheses when known), drops the session's level so the model default applies again. Changing the model or level answers with the updated options and a `config_option_update`. A prompt that changed the model or mode (for example through `exit_plan`) also sends `config_option_update`. `session/request_permission` offers Allow once, Allow always, Allow always in this project (`project`) and Deny.

ACP correlates tool calls and final results by the RPC `call_id`, not the tool name, so overlapping
calls to the same tool retain distinct output and completion status. An approval with a `call_id`
attaches to that call's card; an approval without a matching card gets a placeholder that the call's
own `tool_call` adopts or that `approval_resolved` removes. Name-only legacy output is attached to
the latest active call of that name; it does not override explicit call identities.

RPC `tool_timing` events also carry `call_id`, alongside `name` and `seconds`. Concurrent calls to
the same tool publish their own timing and result as each finishes; clients should correlate by
call ID rather than attach a duration to the most recently displayed tool name.

Resuming a session whose writer lease is held creates an independent sibling rather than taking
over the live owner. RPC resume results identify the source with `forked_from`; the returned session
ID is the sibling's ID. Integrity errors and non-contention I/O errors still fail instead of forking.

A prompt response maps the authoritative outcome. Completed turns return a plain `stopReason` of
`end_turn`; interrupted turns return `cancelled`; a turn that used its whole step budget returns
`max_turn_requests`. Failed, blocked, needs-input and incomplete turns return `end_turn` with
`_meta.dial.outcome` (the Dial outcome) and `_meta.dial.report` (the final report text). Because ACP
stop reasons cannot say this, such a turn also ends with a visible `agent_message_chunk`
("Dial: the turn ended as needs input.", with `_meta.dial.event` `outcome`). The RPC prompt
result carries the same information as `outcome` and a boolean `step_limit`.

ACP plan entries map each todo status explicitly: `pending`, `in_progress` and `completed` map directly;
abandoned tasks are `completed` and blocked tasks are `pending`. ACP has no status for either, so
their entry text is prefixed `[abandoned]` or `[blocked]` with the task note appended in
parentheses, and `_meta.dial.status` and `_meta.dial.note` carry the same values.

In-flight ACP request IDs are rejected before prompt preparation or cancellation has side effects.
Cancellation requests with IDs receive their own replies; cancellation notifications never produce
responses and cannot consume the pending prompt's final response. Cancelling a parked session does
not interrupt the active session.

### Uncertain editor writes

ACP client filesystem writes wait up to 30 seconds for the editor's response. Refusal before
delivery has no write effects; timeout, cancellation, lost acknowledgement or an error after
delivery leaves effects unknown. Diagnostics retain the originating call identity and never
claim that the editor did not write merely because Dial did not observe success. No local write
is substituted after a client error, and confirmed earlier files in a batch remain reported.

On the current connection, an uncertain write blocks another write to that path. Its late response
remains correlatable after cancellation or prompt completion. Only acknowledgement followed by
a successful fresh read releases the barrier; a read racing an unacknowledged write cannot do so.
The pending-write registry is bounded at 128 paths and refuses further writes when full. A client
that never acknowledges requires inspection rather than automatic replay. Reconnecting is not
evidence that an earlier request finished; these in-memory correlations are not a crash journal.

## Building a client

See [client compatibility and conformance](client-contracts.md) for the version policy, schema-checked
request fixtures, cancellation/reconnect handling, revision rules and an executable contract matrix.
The [workflow examples](../examples/workflows/README.md) compose existing skills, session exports and
verified learned-artifact packs without adding a transport or workflow interpreter.
