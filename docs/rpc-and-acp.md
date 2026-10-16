# RPC and ACP

`dial rpc` speaks the newline-delimited JSON control protocol on stdin/stdout; `dial acp` exposes ACP v1 over stdio. Stdout is owned by one blocking writer thread. It uses a private unlinked spool when the consumer falls behind, preserving complete ordered frames without changing shared descriptor flags. Protocol producers reserve positions before mapping events, so concurrent notifications and replies cannot reorder.

RPC query methods remain available during active turns; commands that mutate or interrupt the active conversation retain their turn semantics. ACP requests are served by one core worker that runs one turn at a time: a prompt for a session other than the active one is refused until the running turn ends, and the running turn is never cancelled on another session's behalf. ACP permission requests are sent to the client as `session/request_permission`. Client filesystem, terminal, auth, and MCP bridge methods are not implemented; ACP refuses them with `-32601` rather than executing them with ambient authority.

The Rust RPC request/response types define the wire protocol; no generated JSON schema file is shipped. Print mode (`dial --print`) answers exactly one prompt per invocation: the positional words are joined with spaces and piped stdin is appended as context. It does not accept image attachments. Stdout is drained before exit. `stream-json` omits repeated full-history snapshots.

## Print mode and session automation

`dial -p --output-format json "prompt"` emits one result object; `--output-format stream-json`
emits newline-delimited system, text, tool, todo, notice and usage events followed by the result.
JSON modes reserve stdout for JSON. Results include session ID, answer, tool outcomes, usage and duration.
Use `--allow read,edit` for named tool approval instead of blanket `-y`; read-only mode and deny rules
still win. With both a prompt argument and piped stdin, stdin is appended as fenced context.
Exit status: `0` completed, `1` error, `2` incomplete, blocked, needs input, no answer or interrupted, `3` a required tool approval was denied (takes precedence over 2).

`dial sessions --all` lists sessions across projects. `dial sessions rename <id> <title>` and
`dial sessions export <id> [path] [--html]` expose session management without the TUI.

## Execution tree and result revisions

Execution uses shared, revisioned result bodies backed by the session's branch-aware JSONL journal.
RPC `state` exposes a `tree` containing session, conversation and execution nodes. Each execution
node has an `id`, `parent`, `kind`, `label` and `result` (`text`, optional structured `data`, `phase`,
`revision`). `state_reset` clears the current observer view; `state_node` replaces one node and
`state_delta` appends text to an existing node at the supplied revision. Worker updates use bounded
replacement snapshots. Legacy events remain available; clients should choose one projection to
avoid displaying a token twice. Tool-result events include original structured `data` when supplied.
Snapshots are written at boundaries, not for every token. Resuming never restarts unfinished workers.
Unassigned child roles inherit the parent model, profile and request controls; the model picker can
still assign an explicit child model.
Ctrl-C interrupts silent HTTP response reads without replacing the configured inference deadline with
a short timeout. Connection establishment and partial writes still use their configured deadlines.

RPC `state` replies come from one worker-owned projection, the same one the worker uses, and never include message history; read `messages` or `messages_page` for history. Typed RPC `notice` events keep their plain `message` and add `kind`, `lane` (`context`, `divider` or `alert`) and `tokens` as additive fields. Plain notices remain valid without these fields.

## Persistent RPC and ACP transports

`dial rpc` is a persistent version-1 JSON-lines control endpoint. Requests have `id`, `method` and `params`;
responses correlate by `id`, while unsolicited events have `event`. Methods include `send`, `state`,
`messages`, `compact`, `seed_context`, `new_session`, `resume`, `rename`, `plan`, `steer`, `abort`,
`task_start`, `task_status`, `task_message`, `task_cancel`, `approve` and `shutdown`.
Live voice (`docs/voice.md`): `live_start` (optional `voice`, `instructions`), `live_mute` (`muted`), `live_stop`, and `live_listen` (records one utterance and runs it as a turn; not available over ACP). While a live session is active, `send` text gets its instructions, and each finished answer is read aloud.
Approval responses must name the currently pending `approval_id`; future/stale/replayed decisions fail.
Abort and task messaging/cancellation remain usable while inference or an approval wait is active.
Consumers must handle out-of-order response IDs and asynchronous events.

`dial acp` exposes the ACP initialize/session/prompt/cancel/permission/mode flow over bounded JSON-RPC
stdio with streamed assistant/tool updates. Loaded sessions retain exclusive writer ownership; parked
sessions are not concurrently writable. ACP does not advertise unsupported filesystem/terminal
capabilities. Both automation transports use bounded serialized stdout and stop honestly on stalled
or broken consumers instead of reporting partially delivered success.

ACP renders `notice`, `background` and `fatal` events as unchanged `agent_message_chunk` text. Each chunk also carries standard `_meta.dial` with `event` and, only when present, `kind`, `lane`, `tokens`, `name` and `replace`, so clients can tell sources apart. This is metadata preservation only; ACP does not implement client-side replacement of background output.
