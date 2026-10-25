# RPC and ACP

`dial rpc` speaks the newline-delimited JSON control protocol on stdin/stdout; `dial acp` exposes ACP v1 over stdio. Stdout is owned by one blocking writer thread. It uses a private unlinked spool when the consumer falls behind, preserving complete ordered frames without changing shared descriptor flags. Protocol producers reserve positions before mapping events, so concurrent notifications and replies cannot reorder.

RPC query methods remain available during active turns; commands that mutate or interrupt the active conversation retain their turn semantics. ACP requests are served by one core worker that runs one turn at a time: a prompt for a session other than the active one is refused until the running turn ends, and the running turn is never cancelled on another session's behalf. ACP permission requests are sent to the client as `session/request_permission`. Client filesystem, terminal, auth, and MCP bridge methods are not implemented; ACP refuses them with `-32601` rather than executing them with ambient authority.

[rpc-schema.json](rpc-schema.json) is a hand-maintained JSON Schema (draft 2020-12) of the plain `dial rpc` request, response and event frames; ACP uses JSON-RPC 2.0 and is not covered. Tests in `crates/dial-app/src/rpc/tests.rs` fail when its request methods, event names or limits drift from the implementation (method names are checked against the dispatch arms, event names against every emitted `"event"`). Print mode (`dial --print`) answers exactly one prompt per invocation: the positional words are joined with spaces and piped stdin is appended as context. It does not accept image attachments. Stdout is drained before exit. `stream-json` omits repeated full-history snapshots.

## Print mode and session automation

`dial -p --output-format json "prompt"` emits one result object; `--output-format stream-json`
emits newline-delimited system, text, tool, todo, notice and usage events followed by the result.
JSON modes reserve stdout for JSON. Results include session ID, answer, tool outcomes, usage and duration.
Use `--allow read,edit` for named tool approval instead of blanket `-y`; read-only mode and deny rules
still win. With both a prompt argument and piped stdin, stdin is appended as fenced context.
Exit status: `0` completed, `1` error, `2` incomplete, blocked, needs input, no answer or interrupted, `3` a required tool approval was denied (takes precedence over 2).

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
`task_start`, `task_status`, `task_message`, `task_cancel`, `approve` and `shutdown`.
Live voice (`docs/voice.md`): `live_start` (optional `voice`, `instructions`), `live_mute` (`muted`), `live_stop`, and `live_listen` (records one utterance and runs it as a turn; not available over ACP). While a live session is active, `send` text gets its instructions, and each finished answer is read aloud.
Approval responses must name the currently pending `approval_id`; future/stale/replayed decisions fail.
The `approval` event carries `approval_id`, `tool`, `summary` and, when the request belongs to a call card, the call's `call_id`. `approval_resolved` (`approval_id`) follows once the request is decided, so clients can retire it.
Abort and task messaging/cancellation remain usable while inference or an approval wait is active.
Consumers must handle out-of-order response IDs and asynchronous events.

### Ready frame

The first event on every connection is `ready`. Besides `protocol_version` (the version this build speaks), `session_id`, `model`, `tools`, `image_input` and `question_input`, it carries what a client needs to negotiate and size its requests:

- `supported_protocol_versions`: array of versions the server serves (currently `[1]`). Refuse to continue when none is one you implement.
- `methods`: request methods this transport accepts. `capabilities`: the capability groups those methods belong to (`prompt`, `query`, `plan_mode`, `sessions`, `maintenance`, `context_seed`, `tasks`, `steering`, `abort`, `approvals`, `questions`, `live_voice`, `shutdown`). Over ACP the `questions` and `live_voice` groups are absent.
- `limits`: `max_request_bytes` (1 MiB for RPC, 8 MiB for ACP; a longer line is drained and answered with an error), `max_pending_steering` (8), `max_seed_messages` (200), `max_seed_message_bytes` (1 MiB), `max_memory_command_bytes` (8192), `messages_page_default` (50), `messages_page_max` (500) and `tool_result_event_bytes` (24 KiB; `tool_result` event text is clipped, the model-visible result is not).

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

ACP renders `notice`, `background` and `fatal` events as unchanged `agent_message_chunk` text. Each chunk also carries standard `_meta.dial` with `event` and, only when present, `kind`, `lane`, `tokens`, `name` and `replace`, so clients can tell sources apart. This is metadata preservation only; ACP does not implement client-side replacement of background output.

ACP correlates tool calls and final results by the RPC `call_id`, not the tool name, so overlapping
calls to the same tool retain distinct output and completion status. An approval with a `call_id`
attaches to that call's card; an approval without a matching card gets a placeholder that the call's
own `tool_call` adopts or that `approval_resolved` removes. Name-only legacy output is attached to
the latest active call of that name; it does not override explicit call identities.

A prompt response maps the authoritative outcome. Completed turns return a plain `stopReason` of
`end_turn`; interrupted turns return `cancelled`; a turn that used its whole step budget returns
`max_turn_requests`. Failed, blocked, needs-input and incomplete turns return `end_turn` with
`_meta.dial.outcome` (the Dial outcome) and `_meta.dial.report` (the final report text). The RPC prompt
result carries the same information as `outcome` and a boolean `step_limit`.

ACP plan entries map each todo status explicitly: `pending`, `in_progress` and `completed` map directly;
abandoned tasks are `completed` with `_meta.dial.status` `abandoned`; blocked tasks are `pending` with
`_meta.dial.status` `blocked` and the task note in `_meta.dial.note`.

In-flight ACP request IDs are rejected before prompt preparation or cancellation has side effects.
Cancellation requests with IDs receive their own replies; cancellation notifications never produce
responses and cannot consume the pending prompt's final response. Cancelling a parked session does
not interrupt the active session.

## Building a client

See [client compatibility and conformance](client-contracts.md) for the version policy, schema-checked
request fixtures, cancellation/reconnect handling, revision rules and an executable contract matrix.
The [workflow examples](../examples/workflows/README.md) compose existing skills, session exports and
verified learned-artifact packs without adding a transport or workflow interpreter.
