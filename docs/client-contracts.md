# Client compatibility and conformance

Use this guide with [RPC and ACP](rpc-and-acp.md) and the authoritative runtime owners
`crates/dial-app/src/{rpc,acp}.rs`. No network listener, remote authority or new transport is
introduced. Launch one local stdio process per controller; stdout is exclusively protocol data.

## Compatibility policy

- Plain RPC is version 1. Check `ready.supported_protocol_versions` for an intersection with
  versions your client implements, then check `methods`, `capabilities` and `limits`. Do not infer
  capability from a build version or from an unrelated transport. Fail closed on no intersection.
- Existing v1 method meanings and required fields must remain compatible. Additive optional
  response/event metadata may appear: ignore unknown fields, but do not interpret them as grants.
  Breaking meanings or required inputs need an explicitly negotiated protocol version; this guide
  does not introduce one. The checked-in schema is a producer contract, not a requirement that
  clients reject harmless extra response fields.
- ACP uses JSON-RPC 2.0 and ACP v1 initialization, not the plain RPC framing schema. Respect
  advertised capabilities. `_meta.dial` carries optional richer outcomes; generic clients may
  render standard chunks, but must not equate `end_turn` with verified success.
- Rust facade re-exports are not a separately versioned external SDK. Pin the crate revision when
  embedding; prefer the versioned transports for independently released clients.
- Session IDs identify durable state, not request idempotency keys. There is no exactly-once retry
  or event replay cursor. Never automatically repeat a mutating request after an ambiguous failure.

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

Run `cargo test --locked -p dial-app --lib rpc::tests` and the corresponding `acp::tests` filter.
These unit contracts test projection/adapter behavior, not a live IDE, process-crash persistence or
provider delivery. Transport and session persistence suites cover their own owners; do not report
these fixtures as a live reconnect certification.

### Policy explanation events

RPC `policy_explanation` is an additive observation correlated by `call_id` and `tool`. It carries
human-readable `message` and typed `explanation.reason`, with optional `explanation.rule_scope`.
Render it as a reason, not an approval request or an authority token. Only the actual pending
`approval_id` can be answered through `approve`; explanation events never replace that lifecycle.
Clients should tolerate unknown event kinds and reason values without granting permission or
discarding the original turn result. See the schema for producer fields and supported reason values.
