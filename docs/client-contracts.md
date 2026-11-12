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

## Client parity

This matrix describes implemented paths, not proof that a particular editor, model, device or
server supports them. Runtime offers and normal approvals remain authoritative.

| Capability | Terminal | Plain RPC | ACP v1 |
| --- | --- | --- | --- |
| Human questions | Native dialogs | Correlated question events, presentation acknowledgment and answers | Not negotiated; `questionInput.supported=false` |
| Tool permissions | Native approval UI | Pending `approval_id` only (`allow`, `always`, `project`, `deny`) | `session/request_permission` with once, always, project and deny options; permission choices are not question answers |
| Editor-buffer filesystem | Local workspace files | Local workspace files and separately declared host integrations | Negotiated `fs.writeTextFile`; reads use client buffers when `fs.readTextFile` is also offered |
| Image prompts | Supported for image-capable models | Check `ready.image_input` | Check `promptCapabilities.image` |
| Live voice | Device/provider-dependent | `live_*` methods when advertised | Not exposed; audio prompt blocks also unsupported |
| Host tools and URI schemes | Configured extensions | Explicit session-scoped declarations | No host-tool, URI or client MCP bridge |
| Provider sign-in | Native setup | `auth` methods after protocol 2 negotiation | Existing external configuration only; no terminal/OAuth bridge |
| Session administration | Session picker | `list_sessions`, `pin_session`, `delete_session`, `fork_session` | `session/list`, `session/fork`, `session/close`; no pin or delete |
| Models and thinking | `/model`, `/thinking` | `list_models`, `set_model`, `set_thinking`, `state.current_model` | `model` and `thought_level` config options |
| Settings | `/settings` | `settings_schema`, `get_settings`, `set_setting`, `reset_setting` (saved files; applied on restart) | Not exposed |

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
one rendering projection: legacy text/tool events **or** the execution tree, not both.

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
  and send `hello`; fail closed when there is none.
- `request` and `poll` for toolkit-neutral, non-blocking use (drive `poll` from a background thread
  and forward each `Message` to the UI loop), and `call` for short queries. `call` buffers every
  other message, so no event is lost.
- `supports(method)` against `ready.methods`; an unadvertised method is refused locally.
- Typed engine errors (`RpcError.code`, see [protocol 2 codes](rpc-and-acp.md#ready-frame)) and
  helpers for `approval` and `text` events.
- `Message::Disconnected { outstanding }`, which lists each request left without a response as
  `ReadOnly` (safe to repeat on a new connection) or `MayHaveApplied` (outcome unknown, never
  retried automatically). Reconnect is `Client::connect` again followed by `resume`.
- A bounded frame size, graceful shutdown (`shutdown`, or EOF on drop) and `kill`.
- `RpcError::kind()` as a typed `ErrorCode`, and envelope accessors on `Event` (`seq`,
  `session_id`, `request_id`); `lost_events()` counts sequence gaps.
- Typed helpers for common requests (`send`, `steer`, `resume`, `set_setting`, `list_checkpoints`,
  `checkpoint_diff`, `undo`) and `restart_and_resume`, which applies saved settings by replacing
  the engine process and resuming the same session.

`cargo run -p dial-client --example headless -- "prompt"` drives one turn without a terminal UI
(set `DIAL_BIN` and `DIAL_RPC_ARGS` to choose the engine); it denies every approval request.

The client renders nothing and keeps no conversation model: execution-tree reduction and display
remain the frontend's job, following the state rules above.

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
