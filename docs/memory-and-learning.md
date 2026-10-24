# Memory and background learning

## Memory backends

Dial supports project-scoped local rollout learning, managed skills, and provenance memory. Memory is advisory evidence, not authority: current user intent and repository state always take precedence. Captured transcript material is screened/redacted before persistence; credentials and prompt-control material must not be retained. The local rollout backend indexes prior session files incrementally using file size and modification time, skips unreadable sessions, and bounds scans/extraction work per maintenance cycle. It extracts durable knowledge into a private SQLite database and publishes generated projections only through the managed-memory publisher.

Use `/memory enqueue` to request immediate retention/consolidation, or normal maintenance for scheduled work. Provenance lookup is best-effort: unavailable memory must not stop an otherwise valid turn.

Saved codebase understanding checks cited-source identities and its indexed dependency
neighborhood. After index refresh, an existing previously unrelated file becoming a
consumer invalidates the old scope; genuinely unrelated edits do not. Dependency
discovery remains lexical and bounded. Legacy scopes without a dependency-edge witness
are conservatively unchecked until refreshed/re-recorded, not silently treated as fresh.
Reusable findings remain advisory, never proof that current behavior was verified.

### Semantic facts backend

`memory.backend = "semantic"` keeps facts in private SQLite stores outside the repository: one per project, plus a user-wide bank shared by every project. It offers the `retain`, `recall`, `memory_update` and `forget` tools, with the same names and arguments as provenance memory; `retain` takes `scope: "project" | "global"` to choose the bank. It needs no configuration: recall is keyword search (FTS5 BM25). Assigning an embedding-capable model to the `embedding` model role (Alt+M, `/setup embeddings`, or `[model_roles.embedding]`) adds meaning-based search through that role's endpoint, model and key. The two rankings are fused with reciprocal-rank fusion, so recall degrades to keyword search when the service is unavailable or slower than the recall deadline, and `/memory` reports why. Alt+M lists embedding-capable models for this role, and also models whose kind the server does not declare, labelled "unclassified". Assigning the role or finishing `/setup embeddings` while another backend is selected offers to switch to semantic; it never switches silently.

| Setting | Default | Meaning |
| --- | --- | --- |
| `memory.semantic.embedding` | `auto` | `off` never embeds; `auto` embeds when the `embedding` role is assigned or an explicit URL and model are set; `on` requires an embedding route and reports an error instead of falling back to keyword search. |
| `memory.semantic.embedding_base_url` | empty | Advanced override of the role's endpoint. Set together with `embedding_model`. HTTPS, or HTTP to a loopback address only. No credentials, query or fragment. |
| `memory.semantic.embedding_model` | empty | Advanced override of the role's model name sent to `/embeddings`. Set together with `embedding_base_url`. |
| `memory.semantic.embedding_api_key_env` | empty | Name of the environment variable holding the key for the explicit override. Empty for a local server without authentication. Role routes use the role's own key. |
| `memory.semantic.embedding_timeout_secs` | 20 | Per-request timeout, 1–120. |
| `memory.semantic.recall_limit` | 8 | Most facts per query, 1–50. |
| `memory.semantic.token_budget` | 1500 | Recall text budget in tokens, 64–16000. |
| `memory.semantic.auto_retain` | `true` | Queue each completed turn and extract durable facts with the `memory` role between turns. It pauses while the `memory` role is unassigned (it never falls back to the main model); recall and the `retain` tool keep working. Assign the role with Alt+M to save facts automatically. Malformed model output drops the turn; an unreachable model keeps it queued. A failing extraction does not block duplicate merging. |
| `memory.semantic.consolidate_interval_minutes` | 60 | How often near-duplicate facts are merged (token-overlap ≥ 0.8; the newer copy is forgotten). Facts whose numbers differ are never merged. `0` disables consolidation. Range 0–10080. |

Privacy: with no endpoint configured, nothing leaves the machine. With an endpoint, fact content and recall queries are sent to that service for embedding. Embedding requests send only the URL, model and bearer key; custom provider headers and `allow_insecure` are not applied to them. Facts are re-embedded only when their content changes or after `/memory rebuild`. Recalled facts are advisory, untrusted text.
Auto-retain sends the text of each queued turn to the model assigned to the `memory` role.

Automatic recall runs on user messages for the semantic and Hindsight backends. Shown-fact dedupe is scoped to the selected runtime's session/history generation. Every committed change to the provider-visible history advances that generation: compaction, a context reset, an investigation rewind, and a real `rewind` (one that removes at least one visible message). Facts removed from the visible transcript therefore attach again on the next matching prompt. Identical raw prompts are deduped by the effective recall query, including the selected history; a changed query gets a new search, while a redundant call does not cancel an in-flight one. Compaction also consults the selected backend's recall. A no-op rewind (no user message in view) commits nothing and keeps shown-fact suppression.

Fact references: a project fact is `<n>` and a user-wide fact is `user:<n>`. `/memory forget <ref> <revision>`, `/memory update <ref> <revision> <text>` and `/memory history <ref>` take either form and require the current revision. `/memory rebuild` recomputes the index and missing embeddings in both banks. `/memory clear` clears only the project bank; `/memory clear user` clears the user-wide bank. `/memory help` lists the commands of the selected backend; text that is not a command searches memory. With the backend off, `/memory` says how to choose one. `/setup memory` lists each backend with its prerequisite and sets `memory.backend`.

`dial -p` runs one bounded memory maintenance pass before it exits, so a one-shot turn is retained. Hindsight flushes every queued write for its endpoint, including writes from earlier sessions.

### Session journal semantics

The session file is append-only. Each entry is one JSON line below an 8 MiB limit.

- **History generation.** `provider_identity.history_generation` is session-monotonic and never reused. The stored value is also floored by the largest generation recorded anywhere in the file, so a rewind to an earlier branch, or a reopen, cannot make it regress. The generation is bumped in the same append batch as the change it covers and before the change is visible.
- **Oversized records.** A message or compaction record (summary or prefix) at or above the line limit keeps its complete original string in the artifact store. The journal stores the first and last 256 KiB of each oversized string on a UTF-8 boundary, with a `SESSION MESSAGE TRUNCATED` marker that names the artifact URI and the original byte length. Only the journal copy is truncated: the marker-bearing text is what replay and later compactions see; the complete text is recovered by retrieving that URI from the artifact store. If the artifact cannot be stored, the append fails before anything is written, so no record is silently unreadable. Records of other kinds that exceed the limit are rejected at append time.
- **Artifact unavailability.** Retrieval is by URI at read time. If the artifact file was removed, retrieval returns an error for that URI; the journal is not rewritten or repaired, and nothing is reconstructed from the truncated view.

### Session recovery diagnostics

Loading a journal never repairs or truncates its source. Storage reports tolerated torn tails,
malformed/oversized complete lines, and absent parents through the drain-once
`Session::take_recovery_diagnostics()` API. Unknown future record types remain compatible.
On the next accepted user turn, Dial surfaces an issue count, records a bounded classification
sample and gives the model a generated warning that resumed history may be incomplete. Damaged
record bytes and identifiers are not inserted into model context as instructions. A later append
separates a torn tail from new records rather than overwriting the original. Recovery is a warning
about available evidence, not a claim that missing history was reconstructed.

Duplicate content is detected by a content hash and returns the existing fact.

## Learning loop

The improvement loop uses verified trajectories, sealed evaluation gates, rollback monitoring, and compute leases. Foreground/local learning settings are independent from background scheduling. Background cycles are on by default (`improve.background = true`) and still wait for idle compute, AC power, the daily budget, and any `/compute` pause. Set `improve.background = false` to run cycles only through `dial improve cycle`. Endpoint-model background learning additionally requires a positive `improve.max_daily_usd` budget. A zero dollar budget is a hard stop for background cloud cycles, not an unlimited allowance. `improve.daily_compute_minutes` governs local background compute separately (zero means unlimited local compute).

Local background operations check that compute budget while running, not just between operations.
Training requests a cooperative checkpoint/pause on budget exhaustion and uses the existing bounded
termination path if the worker does not settle. Exhaustion ends the cycle rather than retrying as
ordinary foreground preemption; recorded usage and active lease time both count toward the limit.

Repository-derived practice tasks are sourced from local git history and tests when available; generated curriculum tasks remain a separate source. Evaluation snapshots stay isolated from training trajectories. Manually registered repository tasks remain available for curated cases.

Review usage and cycle reports after enabling background work. API-equivalent subscription usage is not invoice cost; only observed priced API usage can enforce a dollar ceiling reliably. Unpriced requests must not be treated as free.

The full optional-learning workflow (improvement cycles, LoRA training, steering, repository exercises and portable packs) is documented in [improvement.md](improvement.md).

## Project architecture and reuse

Dial's project memory indexes source outlines and source-backed findings for reuse across
sessions and subagents. It also prioritizes Markdown design documents named `ARCHITECTURE.md`,
`DESIGN.md`, `CONTRIBUTING.md`, or `PRINCIPLES.md` (case-insensitive names), and Markdown files
under `architecture`, `adr`, or `decisions` directories. This uses the existing bounded,
ignore-aware index; ignored or unindexed documents are not guaranteed to appear.

Keep project goals, module responsibilities, dependency boundaries, canonical helpers and
architectural check commands in these documents. Record important decisions and their rationale
in ADRs. Outlines include section headings so the agent can read the relevant current sections;
they are not a substitute for reading the source.

Up to three design records receive retrieval priority, using at most a third of the context
budget for that priority lane. Source hashes are checked before reuse; changed sources and
their dependent findings must be refreshed. Agents are guided to identify the owner, search
for reusable implementations, justify new abstractions and review the diff for architectural
fit. These are evidence and workflow aids, not a guarantee of compliance. Enforce important
boundaries with executable checks configured through `[verification].commands`.

## Selected memory backends

`memory.backend` selects exactly one backend. The default `provenance` preserves Dial's
lexical facts, correction history and explicit legacy import; selecting another backend does
not relabel those rows as semantic memory. `off` disables memory tools and `/memory` operations.

| Backend | Implementation | Prerequisite |
| --- | --- | --- |
| `provenance` | SQLite facts, revisions, expiry and BM25 retrieval | None |
| `local` | Eligible archived-rollout extraction and model consolidation | `memory` role |
| `hindsight` | Bank-scoped retain, recall, reflect and mental models | Explicit Hindsight endpoint and credentials |
| `sharpshooter` | Model-maintained project decision files | `memory` role |

```toml
[memory]
backend = "provenance"
autolearn = false
autolearn_auto_continue = false
autolearn_min_tool_calls = 5
request_max_bytes = 1048576
context_limit = 0
context_max_bytes = 0

[memory.local]
extraction_role = "memory"
consolidation_role = "memory"

[memory.sharpshooter]
model_role = "memory"
model = "" # role selection; known provider/model override or a bare model/repository name
interval_minutes = 5
```

Backend-specific controls are exposed in `/settings`. External backends require explicit
operator provisioning; missing dependencies produce an inactive/error status, never a lexical
fallback advertised as semantic recall. Model overrides changing provider discard the previous
provider's endpoint, credentials and custom headers.

`/memory` routes to the selected backend. Hindsight supports `status`, `search`, `save`,
`enqueue`, `stats`, `diagnose` and backend-specific commands; unsupported operations fail
explicitly. RPC `memory` accepts `{ "command": "status" }` through the same owner.
Provenance retains `/memory QUERY` and `/memory clear [project|global]`.

Hindsight automatic recall is staged before the first eligible prompt per transcript;
only delivery of the winning user message consumes it. Setup has a five-second deadline.
Authenticated branch replacement invalidates stale work. Authentic archived user/assistant
messages feed background retention; generated compaction prefixes do not become source evidence.
Persistent actors service memory maintenance while idle; local inference follows their compute
lease. Foreground calls stop competing maintenance rather than waiting indefinitely.

Compaction recall is separate from first-prompt recall: Hindsight queries authentic archived
user/assistant context with a five-second deadline, even when automatic prompt recall is disabled.
The bounded result is untrusted summarization evidence. Notes-backed rollover includes it in the
existing generated continuation marker and proves the complete replacement request before commit;
it does not append a fabricated human turn or consume first-prompt recall.

With `autolearn` and `autolearn_auto_continue` enabled, a genuine completed turn meeting the
paired-tool threshold can run a private, learn-only native continuation. It uses the original
worker usage scope, normal hooks and permissions, and complete input/output window checks.
Root capture permits at most two model requests, or the lower saved harness cap; an unlimited
public harness still gets a finite capture budget.
Interrupted, plan/read-only and investigation phases are excluded. Pending captures coalesce;
foreground input, clear or history replacement retires stale work. Root capture waits for live
owned work to drain. Accepted tool results are recorded, not represented as guaranteed durable
learning or successful skill publication.

Hindsight transport waits pulse the owning UI for cancellation.

Model requests use the actual role backend, pinned account, output cap and complete-request
headroom check. `request_max_bytes` bounds the canonical serialized messages/tools input envelope,
without copying that envelope solely to count bytes. External requests reserve distinct usage
receipts and must start before settlement. Known usage from a rejected or stale received response
remains accounted separately from discarded-result status. An abandoned started request remains
failed/unreported rather than disappearing; this is not evidence of server rollback or an invoice cap.
Memory and generated managed skills remain untrusted advisory data. Authored skills win;
generated assets publish only under the fixed private managed root and current owner policy.

## Inspect Dial's saved understanding

Use `/understanding` to inspect saved project findings and source outlines, or
`/understanding routing` to filter by a topic, path or symbol. Entries show their source references
and freshness: **fresh**, **stale**, or **unchecked** when the freshness read budget is exhausted.
Findings include their originating session and revision. Stale summaries remain visible for auditing,
clearly marked stale; they are not restored to the agent's current context.

The interactive view shows up to 40 matching entries and reports truncation. To see or export every
saved entry without loading a model:

```sh
dial understanding
dial understanding routing
dial understanding --json > understanding.json
```

This inspects existing saved knowledge without refreshing the index or generating new claims.
Coverage is limited to supported, permitted indexed files; it does not imply the model knows every
file or that agent findings have been independently verified. `/todo` shows current session tasks,
`/memory` shows other project memory, and `/context` shows context usage.

`/understanding architecture` shows saved architectural findings, design documents and manifests,
indexed directories, and observed import/module declarations with source locations and freshness.
The view identifies missing architectural knowledge explicitly. Directory groups do not establish
responsibilities, and imports are not a resolved dependency graph. No extra model inference is used.
The same view is available as `dial understanding architecture`, with `--json` for all records.

## Architecture-guided coding

Laya decision advice is available with `/laya on`, or `[laya] enabled = true`. It uses the
Dial-owned Laya service's native typed prediction API for retrieval scope before each prompt.
`/laya status` shows the latest session decision; `/laya check` probes the live checkpoint
and device. Advice and raw probabilities never certify behavior or replace deterministic checks.
The `small` role writes architecture summaries. Settings → Model → Project understanding → **Architecture model** defaults to local `mlx-community/Qwen2.5-3B-Instruct-4bit`, a compact summarization model. It requires the MLX runtime and model weights; unavailable local inference defers maintenance rather than sending it to the coding provider. Clear this setting to follow the main coding model. An explicit `small` assignment in the model picker (/roles) takes precedence; clearing that role also clears the local fallback. Laya supplies typed decisions rather than summaries.

Install the independent runtime with `dial laya install --checkpoint /path/to/checkpoint`.
Dial copies the checkpoint assets into `$DIAL_HOME/laya/model` (default `~/.dial/laya/model`),
creates its own Python environment, and bundles its own server. No other coding-agent installation
is required. Reinstallation without `--checkpoint` updates the runtime and preserves the saved model.

With `laya.auto_start = true` (default), a refused connection starts Dial's server using its private
runtime. Default port 8002 is separate from other applications. Health responses must identify the
service as `dial-laya` with the supported protocol; foreign services are rejected rather than reused.
Cold startup waits up to the configured Laya request timeout (30 seconds when `timeout_secs` is 0) before coding continues with a **loading**
notice. `/laya start` explicitly starts/waits for up to 30 seconds, with responsive cancellation.
`dial laya start` and `dial laya status` also work from a terminal. The managed server logs to
`~/.dial/laya/server.log` and exits after 15 minutes without prediction requests. Disable advice
with `/laya off`; existing managed requests finish and the service can shut down when idle.

With `[codebase].synthesize_architecture = true` (the default), a successful turn can maintain a
persistent architecture synthesis using the `small` role. Maintenance response text, reasoning, and tool argument streams stay out of the transcript; progress and completion notices remain visible. The task's first action does
not wait for this maintenance call. One call per indexed source state is attempted, with a 180-second
deadline, bounded source excerpts and output, no tools, and visible cancellation. An invalid response
leaves the previous record intact; `/understanding build` explicitly retries. Sealed evaluations do
not load or synthesize project memory.
Queued foreground work takes priority: automatic maintenance skips or cancels without consuming the
queued message, and an interrupted maintenance attempt may be retried on a later turn.

The record explains project purpose, component responsibilities and state ownership, supported
execution paths, constraints, and validation evidence. Every section must cite sources actually
supplied to the model and still unchanged when saved. It records partial coverage and unknowns;
source citations establish provenance, not semantic correctness. Fresh sections survive incremental
updates, while changed or excluded supporting sources make affected sections unavailable for reuse.
Unchanged projects reuse the saved record across sessions without another synthesis call.

Freshness also covers dependency scopes. Component observations watch their cited module, lexical
consumer candidates, and project documents/configuration; broader architectural observations and
saved investigation findings watch the supported project sources. File additions/removals/renames,
branch identity, manifests, lockfiles and toolchain configuration invalidate saved assumptions.
Checks are bounded (2048 supported files, 16 MiB, three seconds per snapshot); incomplete scopes are
**unchecked**, never silently current. Old records without dependency provenance require rebuilding.
Before the first edit to a path, outdated saved assumptions cause an investigation checkpoint: inspect
current sources and dependencies before retrying. This is a review prompt, not a semantic proof.

`codebase_memory` action `invalidate` records a contradiction using `section_ids` for architecture
or `topic` for a finding, plus `reason`. Contradicted prose is withheld until explicitly replaced;
concurrent synthesis cannot erase a newly reported contradiction. The architecture view distinguishes
source status from behavioral status. The latest configured behavioral check records command digest,
source/environment fingerprints and time; it becomes stale when its provenance changes. A failure
requires review, and a pass does not certify an architectural claim. Runtime services, environment
variables and dynamically resolved dependencies still need task-specific checks; lexical consumers
do not establish a complete call graph.

`/understanding architecture` leads with this synthesis. Raw imports are labeled unresolved references.
Each coding turn receives a bounded purpose statement and task-relevant architecture sections.
After edits, the harness supplies affected-area review guidance and lexical consumer/test candidates;
the agent uses configured `lsp` definitions/references to resolve symbols where available. Existing
language-server startup approvals still apply. No architecture-generated shell command runs automatically.

Before editing/debugging tasks, up to eight operator-configured `behavioral_checks` run to record the
pre-edit baseline, subject to normal tool approval. Relevant configured checks run again after edits.
This distinguishes pre-existing failures from regressions and supplies failure/change/pass repair
evidence without labeling the whole task successful. Syntax checks remain separate. Changed-file
review prompts cover callers, interfaces, configuration/serialization, state ownership, and reuse.

There is no fixed inspection allowance: the model reads, searches, and plans at its own pace. The
repeated-tool-call redirect and repeated-evidence stop still apply.
Workflow records separately count architecture output tokens, failed verification checks, repair
rounds, and change-review prompts. Compare actual task correctness and complete-run cost with the
existing repository benchmarks before attributing performance gains to these features.

## Coding assistance without local training

Dial reuses completed work through project-scoped experience, separate from model weights and
self-improvement admission. `/experience` shows saved recipes and source-scoped failures;
`/experience ledger` shows this session's observations and hypotheses. `dial experience --json`
exports saved project records without starting a model. `/understanding architecture` remains the
view of the overall architecture.

The investigation ledger stores questions, source hashes and output digests, and survives session
resume. Unchanged evidence supplies a reminder before further investigation; changed source stops
that evidence from being reused. Hypotheses remain agent interpretations linked to an observation,
not independently verified facts. Actual file contents remain available through the ordinary read tool.

Successful file edits can become executable recipes only after configured behavioral checks pass
following the last edit. Arbitrary shell changes, missing source preconditions, failed checks and
oversized or sensitive records prevent capture. Replaying `experience(action="run", id=...)`
rechecks each step's source hashes, requires the checks still be configured as behavioral, and runs
through normal tool approvals, hooks, sandbox and checkpoints. A stale recipe remains advisory;
it cannot overwrite changed source. Failed replay retains edits for diagnosis, rather than undoing
unrelated work. These observations do not certify a model, provide training rewards or admit adapters.

Local models receive a smaller initial tool set, with core tools and explicitly loaded tools retained.
Observed tool outcomes rank optional tools per model; `tool_search` and `tool_load` retain access to
allowed specialist tools. Compact descriptions preserve parameter schemas and the complete edit
contract. Recalled recipes accumulate display, source-overlap reuse and subsequent configured-check
counts. These are observed-use proxies, not causal accuracy estimates.

The syntax guard provides advisory parser diagnostics on write, replace/hashline edit and AST commits.
On the full tier parser errors never reject changes; setting `assistance.syntax_guard = false`
disables these advisories. On the small and lean local tiers, an `edit` that would raise the parser
error count of a Rust, Python, TypeScript, TSX, JavaScript or Go file is refused before anything is
written: the error names the first new error with a short window of the would-be result, earlier reads
and tags stay valid for a corrected retry, and `allow_syntax_errors: true` applies the edit anyway (with
the advisory). Files that already had errors are refused only when the count rises; `write` and
`ast_edit` stay advisory. Unsupported languages (including zsh, which has no grammar here) still need their native checks. `edit_preview` parses
proposed source without changing files; its
optional `check="type"` uses an offline Cargo check in a disposable project copy. That copy is limited
to 2,048 files, 32 MiB, a three-second preparation budget and a 30-second compiler timeout. Compiler
previews may execute build scripts and require normal approval. Rust/Cargo is currently the supported
type-preview backend; syntax parsing supports the installed tree-sitter languages. Preview success
is not verification of the live workspace. Directly matched Python tests and Rust module tests
supplement, rather than replace, configured verification. Empty suites do not earn recipe verification
credit. This does not implement token-level compiler steering during generation.

An optional `coach` role can annotate a verified workflow with a short lesson. It requires
both an assigned hosted role and explicit enablement, makes at most one bounded call per eligible
completed turn, and yields when queued coding arrives. It receives bounded workflow metadata, never
creates executable steps or checks, and never loads another local model. Local teacher roles are
skipped to preserve coding compute. No teacher call occurs by default.

All controls are available in `/improve settings` under Local model assistance, and in `/settings` under Tools →
Verification. Assign the coach role in `/roles`.

```toml
[assistance]
enabled = true
syntax_guard = true
selective_verification = true
compact_tools = true
initial_tool_limit = 0
context_max_bytes = 0
teacher_enabled = false
teacher_timeout_secs = 0
```
