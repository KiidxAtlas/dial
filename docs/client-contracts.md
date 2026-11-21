# Client compatibility and conformance

Use this guide with [RPC and ACP](rpc-and-acp.md) and the authoritative runtime owners
`crates/dial-app/src/{rpc,acp}.rs`. No network listener, remote authority or new transport is
introduced. Launch one local stdio process per controller; stdout is exclusively protocol data.

## Compatibility policy

- Plain RPC starts at version 1 and supports negotiated version 2. Check
  `ready.supported_protocol_versions`, then `methods`, `capabilities` and `limits`;
  send `hello` before other requests to negotiate. Do not infer capability from a build
  version or unrelated transport. Fail closed on no compatible version.
- Existing v1 method meanings and required fields must remain compatible. Additive optional
  response/event metadata may appear: ignore unknown fields, but do not interpret them as grants.
  Breaking meanings or required inputs need an explicitly negotiated protocol version; this guide
  does not introduce one. The checked-in schema is a producer contract, not a requirement that
  clients reject harmless extra response fields.
- ACP uses JSON-RPC 2.0 and ACP v1 initialization, not the plain RPC framing schema. Respect
  advertised capabilities. `_meta.dial` carries optional richer outcomes; generic clients may
  render standard chunks, but must not equate `end_turn` with verified success.
- Rust facade re-exports are not a separately versioned external SDK. Pin the crate revision when
  embedding; prefer the versioned transports for independently released clients. Rust frontends
  should use the `dial-client` crate ([Rust client](#rust-client)), which speaks the transport.
- Session IDs identify durable state, not request idempotency keys. There is no exactly-once retry
  or event replay cursor. Never automatically repeat a mutating request after an ambiguous failure.

To check an independently built consumer against a local executable, run
`cargo run --locked --manifest-path sdk/client-smoke/Cargo.toml -- /absolute/path/to/dial`.
The [consumer](../sdk/client-smoke/src/main.rs) uses its own Cargo workspace and lockfile, a temporary
project and a temporary Dial home. It expects negotiated v2, an empty history page, and the coded
`invalid_params` refusal. It sends no prompt. This checks the supplied binary, not historical releases.

## Client parity

This matrix describes implemented paths, not proof that a particular editor, model, device or
server supports them. Runtime offers and normal approvals remain authoritative.

| Capability | Terminal | Plain RPC | ACP v1 |
| --- | --- | --- | --- |
| Human questions | Native dialogs | Correlated question events, presentation acknowledgment and answers | Not negotiated; `questionInput.supported=false` |
| Tool permissions | Native approval UI | Pending `approval_id` only (`allow`, `always`, `project`, `deny`) | `session/request_permission` with once, always, project and deny options; permission choices are not question answers |
| Editor-buffer filesystem | Local workspace files | Local workspace files and separately declared host integrations | Negotiated `fs.writeTextFile`; reads use client buffers when `fs.readTextFile` is also offered |
| Image prompts | Supported for image-capable models | `send` `images`/`inline_images`; check `ready.image_input` | Check `promptCapabilities.image` |
| Live voice | Device/provider-dependent | `live_*` methods when advertised | Not exposed; audio prompt blocks also unsupported |
| Host tools and URI schemes | Configured extensions | Explicit session-scoped declarations | No host-tool, URI or client MCP bridge |
| Provider sign-in | Native setup | `auth` methods after protocol 2 negotiation, for every provider (typed values arrive as `auth_prompt`, masked when `secret`) | Existing external configuration only; no terminal/OAuth bridge |
| Extensions and MCP sign-in | `/extensions`, `/mcp login` | `list_extensions`, `extension_action` (with `confirm` for risky actions), `mcp_login`/`mcp_logout`, then `apply_settings` (`reconnect: true` after a sign-in) | Not exposed |
| Session administration | Session picker | `list_sessions`, `pin_session`, `delete_session`, `fork_session` | `session/list`, `session/fork`, `session/close`; no pin or delete |
| Models and thinking | `/model`, `/thinking` | `list_models`, `delete_local_model`, `set_model`, `set_thinking`, `state.current_model` | `model` and `thought_level` config options |
| Settings | `/settings` | `settings_schema`, `get_settings`, `set_setting`, `reset_setting`, then `apply_settings` to apply in the running session | Not exposed |
| Approval mode, retry, managed model | `/approval`, `/retry`, `/model restart` | `set_approval`, `retry`, `restart_model`; `state.approval` | Not exposed |
| Workspace directories | `/add-dir`, `/remove-dir`, `/dirs`, directory-access prompts | `add_dir`, `remove_dir`, `list_dirs`; directory-access prompts after `hello {interactive: true}` | Not exposed |
| Background jobs | `/processes` | `list_jobs`, `stop_job` (no terminal attach) | Not exposed |
| Provider usage and plan | `/usage`, status line | `usage` (`plan`, limits and notes per signed-in provider; `/usage reset` stays terminal-only) | Not exposed |
| Slash-command parity | Every command | `get_available_commands` rows carry `rpc`: the method, client presentation, or terminal-only with a reason | ACP `available_commands_update` |
| Client code generation | Not applicable | `get_schema` (same document as `docs/rpc-schema.json`) | ACP's own schema |

ACP's filesystem bridge does not grant new directory access or turn permission responses into
free-form user input. Clients needing questions or live voice should use plain RPC; there is no
implicit fallback from an ACP refusal to local UI or an unnegotiated custom method. Any future ACP
question extension must negotiate support, correlate presentation and answers, and retire pending
questions on cancellation/disconnect before it can be advertised.

Configured MCP servers belong to Dial, not to ACP's client bridge. The `mcp_servers` inventory
separately reports connected server capabilities, Dial's `client_capabilities`, and unsupported
client requests. A successful connection does not imply sampling or elicitation support.

## Schema-backed request example

[examples/clients/rpc-v1.jsonl](../examples/clients/rpc-v1.jsonl) is validated by the existing
runtime JSON Schema validator against [rpc-schema.json](rpc-schema.json), then round-tripped
through the actual bounded request reader. It contains state/history queries, a prompt, cancellation
and a fresh state query. It is a set of client frames, **not a script to pipe and immediately close
stdin**: EOF disconnects the controller. Keep stdin open, consume `ready` first and send each frame
when appropriate; send `cancel-1` only if intentionally cancelling the outstanding `turn-1`.
A model must be configured before running prompts; these fixtures contain no credentials.

```text
launch dial rpc -> receive ready -> negotiate v1 and methods
state-1 -> correlate its response (events may arrive in between)
history-1 -> render the returned snapshot page
turn-1 -> consume ordered events while retaining the outstanding request
cancel-1 -> cancellation acknowledgment is not the turn result
turn-1 response -> report the authoritative outcome, including interruption
state-2 -> replace the view with the latest available snapshot
```

Allocate distinct outstanding IDs, correlate responses by ID rather than arrival order, and choose
one rendering projection: legacy text/tool events **or** the execution tree, not both. Legacy
renderers send `hello {tree_events: false}` so the engine does not stream the tree at all.

## State, revisions and partial output

`state_reset` invalidates observer nodes. `state_node` replaces a whole node; `state_delta` appends
text and supplies the resulting revision. Revisions are per node, not a global sequence or a
compare-and-swap token. They may skip because updates are coalesced; do not require increments of
exactly one. Ignore duplicate/older revisions. A delta for an unknown node cannot reconstruct it:
request `state`, and reestablish a snapshot before rendering further dependent deltas.

A terminal phase or metadata change is a replacement, not a text append. Keep partial text visible
but label it interrupted/failed/incomplete as appropriate; text alone is not evidence of success.
Tool-result event text is capped at the advertised limit. Retained artifacts and structured results
have separate completeness semantics; a clipped display is not the complete process output.

## Cancellation, disconnect and reconnect

Cancellation acknowledges a request to stop, not rollback or confirmed settlement. Continue draining
until the original prompt response or transport failure. ACP cancellation notifications produce no
response; cancellation requests use their own IDs. Reusing an in-flight ID is rejected before side
effects. Cancelling a parked session cannot cancel the active session.

On EOF/broken stdout, mark outstanding operations **outcome unknown** unless a terminal result was
observed. Reconnect means launching a fresh process, negotiating again and explicitly resuming the
persisted RPC session (ACP: initialize, then session/load). Drop old request IDs, pending approval
cards and partial tool correlations. Rebuild from state/history, not from the old stream. Unfinished
workers are not restarted by resume. A fresh adapter does not consume a late response from the old
connection. Session writer locks can refuse a second writer: surface that error rather than claiming
shared multi-client mutation support. Failed/unavailable snapshots are errors, not empty success.

History cursors are decimal offsets into a snapshot, not stable cross-branch bookmarks. An offset at
the end yields an empty page; beyond the end or malformed offsets are errors. Refresh history after
resume, branch changes or compaction rather than applying old offsets to a new history.

## Rust client

`crates/dial-client` is the reference implementation of this guide for Rust frontends (a TUI, a
desktop GUI or a test harness). Its only workspace dependency is `dial-protocol`, the shared wire
vocabulary (method table, capability groups, error codes, event names) that the engine also uses,
so a frontend built on it couples only to the versioned wire protocol, never to engine internals.
It provides:

- `Client::connect`: launch `dial rpc` with `Launch`, read `ready`, pick the highest common version
  and send `hello`; fail closed when there is none. Failed handshakes kill and reap the child
  immediately rather than adding the normal graceful-shutdown interval to the timeout.
- `request` and `poll` for toolkit-neutral, non-blocking use (drive `poll` from a background thread
  and forward each `Message` to the UI loop), and `call` for short queries. `call` buffers the other
  messages it sees, so no event is lost, up to 1024 messages and about 16 MiB per call (a single
  frame may overshoot the byte bound). Past either bound it returns `ClientError::Backlog` with its
  request still outstanding: poll to drain the buffered
  messages and the reply arrives through `poll`. The bound keeps memory finite without dropping any
  response or approval.
  Reader and writer FIFO queues each enforce a 16 MiB wire-payload budget plus a frame-count limit.
  A legal frame above the budget (up to 64 MiB) occupies its queue alone; each worker can also
  hold one in-flight frame. These are wire-byte budgets, not a total JSON heap-memory bound.
  Request writes use a bounded FIFO worker queue, so pipe backpressure cannot block `request`,
  `call` deadlines or shutdown. A full queue returns `ClientError::Io` (`WouldBlock`) without
  admitting the request; frames larger than `MAX_FRAME_BYTES` return `InvalidInput`.
  Asynchronous write failures surface through disconnect accounting. `call` timeouts include
  queue/write waiting: an admitted request remains outstanding and may still reach the engine
  after timeout. Do not retry mutations automatically; `kill` or bounded `shutdown` terminates
  an unresponsive engine.
- `supports(method)` against `ready.methods`; an unadvertised method is refused locally.
- Typed engine errors (`RpcError.code`, see [protocol 2 codes](rpc-and-acp.md#ready-frame)) and
  helpers for `approval` and `text` events.
- `Message::Disconnected { outstanding }`, which lists each request left without a response as
  `ReadOnly` (safe to repeat on a new connection) or `MayHaveApplied` (outcome unknown, never
  retried automatically). Reconnect is `Client::connect` again followed by `resume`.
- A bounded frame size, graceful explicit `shutdown`, and `kill`. `Drop` closes input and transfers
  child ownership to a background reaper, which waits briefly before killing and reaping an
  unresponsive engine; destruction does not wait for that grace interval on the caller thread.
- `RpcError::kind()` as a typed `ErrorCode`, and envelope accessors on `Event` (`seq`,
  `session_id`, `request_id`); `lost_events()` counts sequence gaps.
- Typed helpers for common requests (`send`, `steer`, `resume`, `set_setting`, `list_checkpoints`,
  `checkpoint_diff`, `undo`) and `restart_and_resume`, which applies saved settings by replacing
  the engine process and resuming the same session. It takes the client by value and returns
  `Restarted { client, handoff }`. This replaces the earlier `Result<Client, _>` return: the old
  connection's unread output is no longer copied into a list. The restart first sends `shutdown`
  and closes the engine's stdin, then reads its output into the bounded queue `call` uses until the
  engine exits. An engine that has already exited hands its buffered messages over; it is never
  refused for them. If the queue fills while the engine still runs, the restart is refused with
  `ClientError::Backlog`. That refusal is not a rollback: the engine is winding down, and
  `RestartError::client` takes no requests (they fail as `Disconnected`). Recover by polling it to
  drain, then `shutdown` to reap it, then `connect` and `resume` the session you read from `state`
  before the restart. Retrying `restart_and_resume` on that connection cannot work, because its
  `state` call fails. After the old engine exits, `handoff` owns the messages it left unread.
  `Handoff::poll(timeout)` returns `HandoffPoll::Message` (the end of the connection arrives last
  as `Message::Disconnected`), `TimedOut` (incomplete; poll again, nothing was lost) or `Finished`.
  Drain it before the new client's messages, then call `Conversation::new_connection`. Dropping an
  unfinished handoff discards its messages, which the type marks `must_use`. Output held open by an
  inherited stdout does not stall the restart. Other failures return `RestartError`: `client` is the
  old connection while it may still run (a failed `state`, or an I/O error while waiting for the
  process), or the replacement when it started but did not resume the session, so `resume` can be
  retried. When the old engine has exited and no replacement runs, `client` is `None` and `handoff`
  holds its messages; reconnect with `connect`.
- Interactive authentication uses `mcp_login(server) -> Result<RequestId, ClientError>`. This
  replaces the blocking `mcp_login(server, timeout)` helper: poll while its id is outstanding,
  display `auth_url`, answer `auth_prompt` with `auth_code`, and match the eventual login response
  by id. Do not use blocking `call("mcp_login", ...)` for interactive login: it buffers the very
  events needed to complete authentication. A caller's UI controls cancellation and deadlines.

`cargo run -p dial-client --example headless -- "prompt"` drives one turn without a terminal UI
(set `DIAL_BIN` and `DIAL_RPC_ARGS` to choose the engine); it denies every approval request.

The `Client` renders nothing. `dial_client::Conversation` is the shared view model: feed it
`messages` history (`load_history`), the prompts you send (`push_turn(text, images, request_id)`,
with the `RequestId` that `send` returned, so only that response settles the turn; `push_user`
does not correlate and is kept for compatibility), and every received
message (`apply`), then draw `entries()` (user, assistant with reasoning, tool cards with their
results, notices) next to `pending_approvals()`, `question()` and `todo()`.
`needs_reload()` reports an event-sequence gap. `load_history` refreshes transcript rows but cannot
recover pending approvals, questions or todo, and therefore does not clear that flag. Request
`recovery_snapshot` once the actor and its workers are idle, process all messages preceding its
response, then pass the response to `Conversation::load_snapshot`. It validates the same-session,
same-connection sequence and replaces transcript and live interaction state together. Incomplete,
stale or wrong-session snapshots are refused without changing the view. Older engines without
this method cannot certify complete recovery from a history query alone. `state.live` is an
interaction-only snapshot, not a full transcript recovery fence.
Each connection has its own `seq` space (`ready` is `seq` 1 and the first event is 2). After
a reconnect or restart, feed the old connection's handoff messages, then call
`new_connection(client.ready())` before the new connection's messages. That voids the old
connection's ids: its pending approvals, open question and turn tracking are dropped, and a
streaming answer is settled, so text from the new connection starts a new row. A turn still
running gets an error row because its outcome is unknown. Rows already shown, tool results and
history are kept. The `ready` frame's `seq` becomes the baseline, so a reconnect to the same
session continues without a reload, a different session replaces the rows as `active_session`
does, and a gap on either connection still raises `needs_reload()`. Without this call, the first
event of the new connection would be compared with the old connection's last `seq` and look like
a gap.
Execution-tree reduction (`state_*` events) remains the frontend's job, following the state rules
above.

`expect_turn(request_id)` tracks any turn-starting request, including `retry` and `goal`; its
correlated response settles that turn independently of method naming. Remote collaboration results
and errors remain visible without settling a separately tracked local turn. The wire cannot always
attribute interleaved text fragments to a remote turn, so clients must not guess that attribution.
Each tool row carries `effects` (`none`, `known`, `unknown`, when the engine says) and structured
`data` next to `status` and `changed_paths`. Output and progress route by `call_id`; settlement clears
progress. Disconnect/reconnect marks unresolved tool outcomes and effects unknown, not failed.

An automatic retry notice (`kind: "retry"`) marks every assistant fragment from that failed model
attempt `abandoned: true`. Argument-only tool drafts retire as skipped with no effects; dispatched
or completed tools are preserved. Later text and reused draft call IDs create fresh rows. Successful
foreground inference `state_node` events (or `inference_done` after `hello {tree_events: false}`)
delimit model rounds, preserving earlier successful answers;
older engines lacking those boundaries cannot provide that distinction. Response text remains the
fallback when the successful turn has no streamed answer.
Historical tool rows preserve explicit status, effects and structured data. Use
`ToolOutcome::succeeded() -> Option<bool>` or `outcome_known` to distinguish unknown legacy
outcomes from known failures. The compatibility field `is_error` remains conservatively true
when success is not established. Do not infer outcomes from output prose.

## Executable conformance map

All listed checks run offline without a model or credentials:

| Contract | Existing owner/test coverage |
| --- | --- |
| Example schema and bounded framing | `rpc/tests.rs::client_request_examples_validate_against_the_published_schema`, request-limit tests |
| Negotiation and capability differences | `ready_frame_announces_versions_limits_and_transport_capabilities` |
| Cursor end/error boundaries and unavailable snapshot | `history_pages_preserve_boundaries_and_reject_invalid_cursors`, `state_reply_reads_only_the_state_projection` |
| Revision, partial output, interrupted settlement | `revisioned_snapshot_retains_partial_output_and_interrupted_outcome`, `node_metadata_ignores_text_growth_and_tracks_every_other_field` |
| Cancel notification/request/duplicate-ID and parked session | `acp/tests.rs::cancellation_cannot_steal_a_prompt_response_and_notifications_have_no_reply` |
| Fresh-connection isolation and late results | `reconnect_does_not_replay_pending_replies_permissions_or_partial_tools` |
| Overlapping tool calls and partial output | `overlapping_same_name_calls_keep_their_own_results` |
| Outcome and approval lifetime | `prompt_stop_reasons_distinguish_outcomes`, `approvals_attach_by_call_id_and_expire_when_decided` |
| Client framing, correlation, negotiation choice and retry classification | `cargo test --locked -p dial-client` |
| Real-process client lifecycle: v2 negotiation, approval round trip, `busy`/`invalid_params`/`not_pending` codes, killed engine reported as outcome unknown, reconnect by `resume` | `tests/integration/cli_print/rpc_client.rs` |

Run `cargo test --locked -p dial-app --lib rpc::tests` and the corresponding `acp::tests` filter.
These unit contracts test projection/adapter behavior, not a live IDE or provider delivery. The
`rpc_client` integration tests (`cargo test --locked --test integration -- cli_print::rpc_client`)
run the real executable against a local scripted provider; they cover a killed engine process and
reconnect-by-resume, but not a live model, a real GUI toolkit or every crash point. Do not report
them as a live reconnect certification.

### Policy explanation events

RPC `policy_explanation` is an additive observation correlated by `call_id` and `tool`. It carries
human-readable `message` and typed `explanation.reason`, with optional `explanation.rule_scope`.
Render it as a reason, not an approval request or an authority token. Only the actual pending
`approval_id` can be answered through `approve`; explanation events never replace that lifecycle.
Clients should tolerate unknown event kinds and reason values without granting permission or
discarding the original turn result. See the schema for producer fields and supported reason values.
