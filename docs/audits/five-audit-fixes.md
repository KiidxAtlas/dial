# Five implementation audits: applied fixes and verification

## Scope

These five passes cover configuration/setup, persistence/recovery, isolation/filesystem boundaries,
request accounting, and RPC/ACP. Each pass produced source changes and regression tests; this is an
implementation record, not a claim that every branch or supported platform has been certified.
Pre-existing work and the separate `architecture-implementation-audits.md` report were preserved.
No live-model calls, paid benchmarks, dependency installations, or model downloads were used for
verification. Network fixtures used local scripted servers.

## 1. Configuration, routing and setup

**Problem:** embedding setup could pull from the built-in local Ollama server while saving a role
that resolved to a different configured endpoint. The installed-model picker had the same mismatch.
Setup could also claim that semantic embedding was enabled despite a project-level override.

**Applied:** `src/setup/embeddings.rs` resolves the configured Ollama endpoint and credentials before
listing or pulling. Native API paths handle a trailing OpenAI-compatible `/v1`. Only the exact local
default is eligible for local startup/installation; an unavailable custom endpoint produces an
explicit error instead. Configuration is saved only after successful setup. The completion summary
uses the effective saved configuration. `src/ui/overlay/model_download.rs` lists models through the
same configured route. Bearer keys and environment-backed header values are redacted from errors
and progress text; literal header values are not treated as secrets.

**Regression coverage:** configured URL and headers, pull/role agreement, unavailable remote endpoint
without saved changes, project override reporting, bearer-key error and progress redaction, native
API suffix handling. Existing setup and picker tests also passed in the full library run.

**Preserved:** canonical selection changes, explicit embedding overrides, credential validation,
and the existing setup approval boundary. Stalled pull reads are not made immediately cancellable;
cancellation remains checked between progress frames. Environment-header redaction was not separately
exercised, and the picker refresh path has no new end-to-end rendered-UI test.

## 2. Persistence, recovery and ownership

**Problem:** copying a session could append to or adopt an unrelated destination stream journal,
including when the source journal was empty. Failure cleanup could remove a journal it did not own.
A failed new-session header write could leave a partial main file. Deletion and archive rollback
could unnecessarily separate the main file from its stream journal.

**Applied:** `src/runtime/stream_journal.rs` provides exclusive creation and a durability sync for
copied frames. Empty copies also reserve a fresh journal. `src/storage/session.rs` cleans up only
files created by the operation, retains source files when relocation fails, removes a failed newly
created main file, and deletes the main session before attempting stream cleanup. `src/cli/gc.rs`
restores the main file before moving its stream journal back; a failed main restore leaves the
archived pair together and reports the failure.

**Regression coverage:** pre-existing destination journals remain byte-for-byte unchanged; empty
sources cannot adopt orphan journals; failed reopen preserves the source; owned/unowned cleanup;
exclusive copy; archive restore success and failed main rename. These passed in the full library run.

**Limits:** two-file archive restoration is not atomic across both renames. No power-loss test or
end-to-end archive-record failure injection was performed. File sync is not a claim of complete
crash consistency for every surrounding directory operation.

## 3. Isolation and filesystem boundaries

**Problem:** a global visited-directory set treated a directory alias like a cycle. Depending on
traversal order, either an alias or the real directory could be copied without its contents.

**Applied:** `src/workspace_copy.rs` uses a path-local ancestor set. Genuine cycles terminate, while
separate in-root aliases each retain their contents. The shared owner continues to serve copy and
clone callers without changing the public benchmark clone API or dependency materialization policy.

**Regression coverage:** both copying strategies preserve real-directory and alias contents, stop
cycles, refuse external targets, leave external files untouched, and preserve exclusion behavior.
All six workspace-copy tests passed in the full library run.

**Limits:** alias materialization can expand copied bytes beyond source-preflight estimates. Generic
dependency names such as `build` and `dist` remain excluded at any depth, including tracked paths.
That policy was documented, not silently changed. Native Windows ProjFS and Linux overlay mounts
were not runtime-tested in this macOS run.

## 4. Request lifecycle and accounting

**Problem:** newly constructed fallback/cooldown backends could bypass the current turn's usage
ledger. Replies received but rejected by the controller were not consistently counted as discarded.

**Applied:** `src/agent/turn.rs` binds replacement backends to the existing usage scope before their
first request. Full-generation guidance and completed-edit-preview rejections record discards under
the requesting purpose, including repair. `src/agent/roles.rs` records rejected auxiliary replies
before restoring the backend and rejects dropped tool calls as well as explicit tool calls and
truncated responses. Physical token and cost usage is retained rather than subtracted on discard.

**Regression coverage:** replacement-backend ledger binding and repair-purpose discard attribution
passed, alongside the complete library suite. Existing operation-purpose partitions and shared worker
budgets remain unchanged.

**Limits:** the new focused tests exercise ledger binding and attribution helpers, not a complete
scripted primary-fallback-cooldown sequence. This pass does not establish ledger coverage for Laya
judgment or every repair parse-rejection path, nor solve fallback context-capacity/lifetime policy.
No measured latency, allocation, cost reduction, or model-task-success improvement is claimed.

## 5. RPC/ACP transport and event handling

**Problem:** same-name overlapping ACP tools could share correlation state. Reused in-flight request
IDs could replace pending replies or apply cancellation side effects before rejection. Malformed
history-pagination input could silently restart at the beginning.

**Applied:** `src/acp.rs` tracks active tool identities and correlates explicit `call_id` values;
legacy name-only output attaches to the latest active call of that name. In-flight request IDs are
rejected before preparation. Cancellation requests receive their own replies; notifications do not
consume prompt responses, and cancelling a parked session does not abort the active one.
`src/rpc.rs` validates pagination types and cursor parsing, preserves offset precedence, clamps limits,
and returns an empty page at the snapshot end without cloning unrelated history.

**Regression coverage:** overlapping calls and out-of-order results, partial output, unknown IDs,
session changes, permission/call identity, duplicate request IDs, cancellation and notification
semantics, pagination boundaries and errors. Ten RPC/ACP library tests passed. The new real-process
RPC pagination integration test also passed, including reconstruction of the complete snapshot.
Existing typed notice metadata and plain-text compatibility remain intact.

## Final shared verification

| Check | Observed result |
| --- | --- |
| `cargo build --locked --all-targets` | Passed |
| `cargo test --locked --no-fail-fast` — library | 4,108 passed, 0 failed, 16 ignored |
| Same command — CLI binary tests | 16 passed, 0 failed |
| Same command — integration | 268 passed, 28 failed |
| Same command — doc tests | 0 tests, no failures |
| `cargo clippy --locked --all-targets -- -D clippy::correctness -D clippy::suspicious` | Passed; other warnings remain |
| `cargo fmt --check` | Failed; workspace formatting drift remains |

The completion review inspected the audit diff and retained unrelated formatting and user edits
rather than whole-file formatting shared agent/session files. The affected behavior documentation
was updated in `model-embeddings.md`, `subagents.md`, `llm-transport.md`, and `rpc-and-acp.md`.

### Clean-HEAD comparison and release limits

A disposable `git archive` of **`6c795351a004764fcaa3bd7728db78d0541721d5`** was tested with
`cargo test --offline --locked --manifest-path <snapshot>/Cargo.toml --test integration --no-fail-fast`.
It produced **267 passed and the same 28 failing test names**. The current audit tree adds one passing
pagination integration regression and no additional failing integration test names in these runs.
This baseline already includes earlier work; it does not attribute the origin of every failure or
prove that every current change is regression-free. The source checkout/index was not reset or
stashed to obtain the baseline.

The unresolved failures cover recovery/Incomplete outcomes, silence and late-tool cancellation,
worker concurrency, dynamic tool exposure and request prefixes, compaction thresholds, learning/replay,
and CLI error exit status. In the request-prefix failure, the recorded environment message changes
`retain` from loadable to offered between requests; this is a visible tool-exposure change, not proof
that copy-on-write history projection corrupted the conversation. No default discovery/recovery policy
or unrelated assertion was changed just to make these tests pass.

**The full suite is not green and release readiness is not established.** In particular,
`silent_request_retries_only_inference_and_preserves_completed_work` still fails its late-tool safety
assertion in both trees. These results close the five scoped audit implementations, not those broader
integration defects or all findings in separate review reports.
