# Tools, safety and context

For programmatic reads, `read` accepts `format: "text"` or `"base64"`: one literal local file,
at most 1 MiB, exact payload in `data.content`, and a compact SHA-256/byte-count receipt in text.
`retain: true` additionally preserves the exact source in the artifact store. Omitting these
fields retains normal document conversion, selectors and line reads. `write` with
`encoding: "base64"` creates a new binary file through native transaction admission; it never
replaces an existing output and its parent must exist. See the
[portable eval helpers](execution-and-web.md#persistent-kernels-and-evaluation).

The built-in tool roster, specialist integrations, safety model, context sources, editing grammars and tool-reliability measurement.

## Repository and execution trust boundaries

Native file mutations refuse Git metadata, including linked-worktree metadata and resolved
aliases. Host-side Git inspection fails closed when effective repository/global configuration
contains executable clean, smudge, process or text-conversion drivers; configuration-query failure
is not treated as absence of drivers. History reads disable signature-verifier execution and
patch reads disable external diff/text conversion. These guards also cover Git-panel AI and
CLI commit-generation reads; approved staging, hooks and signing retain their explicit mutation
policy. Ordinary shell inspection exemptions reject unquoted glob/tilde expansion instead of
classifying the unexpanded command as safe.

The `computer` worker receives a filtered environment, but its JavaScript runs still have Node
APIs. `read_only: true` restricts the desktop-helper facade; it is **not** a security boundary
around arbitrary JavaScript or `require`. Every run still requires the normal approval policy.
Do not treat that option as permission to execute untrusted code.

## Compact file references and tool context

Dial automatically gives the model short file references such as `@f7`. Native snapshot
headers become `[@f7#TAG]` in the model-facing view. Reads in replace mode introduce
references separately without changing source rows. Discovery rows retain labels such as
`@f7(path)` so the model can still choose files; repeated locations use the bare identity.
The model passes only `@f7:120-160` for a read or `[@f7#TAG]` for an edit. This is a request
projection, not merely advice to abbreviate: later native calls also use already-issued
references where safe to project, frozen when recorded to keep cached history stable.
User-facing tool calls/results and retained source evidence keep the actual paths. User instructions,
source lines, raw reads, search patterns, shell scripts and signed provider replay are
not rewritten. Full paths remain valid, including for new files. A path copied with its edit-header
tag (`@f7#TAG` or `@f7#TAG:1-20`) names the same file; the tag is ignored outside edit headers.

Handles expand in native structured path arguments and hashline headers/move targets
before normal schema, scoped-guidance, hook, approval and filesystem checks. They do not
grant access, restore sensitive-directory grants, change edit witnesses or bypass snapshot
validation. Unknown/retired handles fail without guessing; a literal filename named `@f7`
can be addressed as `./@f7`. Mapping count and path size are bounded; exhausted registries
keep existing identities and fall back to full paths rather than reassigning handles.
Clones share identities; isolated workspaces do not. Resume preserves mappings, while
root changes retire previous identities and ordinary session switches use a separate scope.

Typed `@directory` attachments admit only complete listings of at most 200 names and 16 KiB.
Oversized listings are omitted wholesale with narrowing guidance, rather than enumerating and
sorting the whole directory or choosing an arbitrary filesystem-order subset. Directory/entry/type
errors are explicit. Cancellation is checked between references, entries and image attachments;
it cannot preempt an individual operating-system filesystem call. The original typed text is retained.

When an edit reports a missing or incomplete source witness, read the affected lines again and
retry against that current read. Hashline edits should use the fresh `[path#TAG]` header; other
edit formats use their own normal request syntax. Reads and edits still enforce the same witness
and snapshot checks.

Native edit-retry readiness is bound to the confined physical source, raw snapshot digest and
required line ranges (or whole-file coverage) in the existing witness store. Reading another
range, a clipped row, a stale snapshot or only one file of a multi-file requirement does not
release that witness prerequisite. Diagnostic prose and tool payloads cannot manufacture it.

For controller-confirmed successful, completely captured `bash`/`bg` results with retained
evidence, Dial can remove recognized Cargo progress and individual Rust test-success rows.
Test totals, zero-test messages, warnings and diagnostic continuations remain visible;
failures, cancellation, uncertain effects, partial output and unrecognized output do not
qualify. The model receives an `artifact://` link to the original retained result, readable with `read`.

Context filtering uses existing compaction controls and cache-pressure/prefix protections:
large, exact repeated successful read-only results can become stable retrievable pointers.
Failed/uncertain results, mutations (including LSP mutations), current instructions and
tool-call pairing remain intact. This affects model context, not append-only history or
artifact retention. No fixed percentage of token savings is promised; provider token/cache
usage and task success must be measured on real workloads.

## Tool roster, safety and context sources

**Tools** (file edits confined to the project; shell access follows the chosen sandbox)
- The default core is stable across ordinary requests. Hosted routes get 24 KiB of schemas;
  lean and local routes keep 12 KiB, and lean local routes also cap the count at eight. Core
  priority is `tool_search`, `tool_load`, `finish`, `read`, `edit`, `write`, `bash`, `grep`,
  `glob`, `todo`, `verify`, `eval` (when containment is available and a cell runtime resolves,
  including the embedded JavaScript engine), `bg`, then `task` with `message` (hosted routes
  only, and not in unattended runs unless `task.prefer_subagents`), then `ask` when a human can
  answer; each joins while it fits the route's budget.
  Specialists, MCP and memory tools are deferred. `tool_search` describes matches, with a compact
  parameter summary (`name*` marks a required parameter), without loading them; `select:a,b`
  looks up exact names, and `load: true` also loads the exact or top matches for the next request
  through the same owner and checks as `tool_load`, so a specialist costs one round instead of
  two. Use `tool_load` with exact names to load or unload explicitly. Pins survive resume. An
  unoffered call is refused rather than implicitly changing the offer. Where `bg` is not in the
  core, a `bash` command that becomes a background job loads `bg` for the next request. On
  first-party Anthropic and OpenAI Responses routes a load or a request-scoped narrowing does not
  edit the request's `tools` array: deferred tools are made visible by `tool_reference` results,
  and narrowing uses `allowed_tools` or a refusal by state
  ([llm-transport.md](llm-transport.md#cache-stable-tool-surfaces)).
  Offered built-in schemas omit parameter explanations except for `read`, `edit`, `bash`, `grep`,
  `glob`, `todo`, `verify`, `bg` and `lsp`, which keep one short explanation (at most 15 words)
  per top-level parameter; argument errors return the complete explanations. Lean and local
  routes use shorter `bash`, `grep`, `glob` and `write` descriptions that keep the regex dialect,
  output format, scopes, paging and sandbox facts; hosted routes keep the tools' own texts. `assistance.compact_tools=false`
  retains legacy automatic discovery/routing; profile allowlists and explicit harnesses remain
  authoritative. Built-in loading leaves cached policy stable; explicit MCP loads can add bounded server instructions.
  Catalog and `tool_search` entries expose advisory `readiness` plus `operation_readiness` for
  known action-specific prerequisites. States are `ready`, `missing_prerequisite`,
  `approval_required`, `unsupported` and `unknown`; metadata is recomputed from configuration and
  owner-held facts without starting a process, probing a service or installing anything. Automatic
  routing defers known blockers and prefers ready tools only when relevance is tied. Unknown is
  not unavailable, and eligible tools remain explicitly discoverable/loadable. A missing grounding
  role for `computer` locate does not disable unrelated desktop operations. Ready is neither
  authorization nor a guarantee of live availability; ordinary approval and execution checks remain.
  Explicit search uses readiness only after relevance ties and keeps blocked exact names visible.
  `tool_load` rejects overlapping load/unload names before changing selections and reports each
  tool's effective next-request offering. Unpinning a core schema may leave it offered; persistence
  errors are not successful loading receipts.
- **Directory access** (`request_directory`): the agent requests an existing directory with a reason; Dial shows a mandatory Yes/No popup, including in auto modes. Normal grants preserve tool approvals and sandbox/credential restrictions. Protected Dial storage requires a second separate, exact-directory approval for **native file tools only**; shell commands remain protected. Sensitive grants are process-local and never unlock credentials. See [Directory access and sensitive approvals](directory-access.md).
- **Policy explanations:** approval decisions expose a typed observational reason and, for saved rules, the winning rule scope. Explicit denies, readonly/plan restrictions, automatic allowances, safe calls, operator responses, cancellation and hook refusals retain existing authority. The TUI approval card includes the reason; plain output and RPC/stream JSON use the same formatter (`policy_explanation`). These events are not approvals, grants or proof that a tool executed; only an actual pending approval request can be answered. See [client contracts](client-contracts.md).
- **Independent sessions** (`peers`): list live sessions in the same checkout, send bounded messages and declare advisory relative-path claims. Peer data never grants permissions or substitutes for verification. This is distinct from delegated subagents; see [Session coordination](session-coordination.md).
- **Subagents** (`task`): `scout`, `reviewer`, `planner` and `worker` retain independent contexts, role routing, approval boundaries and structured-output validation. `action="run"` is blocking; `start` returns live IDs, then `status`, `wait`, `message` and `cancel` operate on those jobs. Peers use `task_message` for coordination and artifacts for shared evidence. `/jobs`, `/message <id> <text>` and `/cancel <id>` expose live controls to the user. Inbox/history/concurrency are bounded; children cannot spawn children, and workspace mutations remain serial.
  Blocking task calls still keep the UI responsive: both serial children (including workers/local models)
  and parallel batches run off the UI thread. Timed event pumping updates the timer and handles input
  even while a child produces no output; cancellation prevents pending approvals from authorizing work.
  `subagent_max_live_jobs` sets the live-job ceiling across `start` batches (1–32, default 32).
  Change it globally or per project in `/settings` → Tools → Subagents → Maximum live jobs,
  then start a new session. Excess batches are refused before any worker launches; they are not queued.
  Blocking `run` batches retain their existing concurrency/mutation policy. Shared subagent role/profile
  controls resolve once per live batch, before runtime nodes or history are changed.
  Worker reports expose a separate priority envelope (summary, blockers, `acceptance_N` statuses,
  evidence references, changed paths and integration risks) apart from narrative previews. Prose
  acceptance claims remain `not_verified`; completed execution is never acceptance proof.
  Optional `acceptance_checks` selects configured check IDs for controller-owned `accepted`,
  `needs_verification` or `rejected` acceptance, separate from execution status. Mutating contracted
  workers run isolated; only passing checks on stable declared inputs permit promotion. Whole-patch
  preflight blocks partial conflict merges; failed installation triggers rollback attempts without
  replacing concurrent edits, retaining recovery data when restoration fails. This is not filesystem-wide
  atomicity. See [isolation and results](subagents.md#isolation-and-results) for the limits.
  Complete reports are retained when artifact storage is available; synchronous plain report text
  and custom output-schema JSON stay unchanged, with a separate model-visible metadata note. Child working state is
  packed as whole JSON fields, prioritizing constraints and current work rather than prefix-clipping.
  Oversized critical request/state snapshots explicitly require clarification from the parent;
  an incomplete handoff never claims to contain the full contract.
- `read` takes line selectors in every edit mode: `path:50-200`, `path:50+150`, `path:-60` (last 60 lines), `path:5-16,960-973`, or `offset`/`limit`; the footer identifies the returned range. `path:outline` lists a source file's syntax declarations (functions, types, impls/classes, methods, nested) with 1-based line ranges, using the same tree-sitter declarations as `find`; outline rows are summaries, not edit witnesses, so read a line range before editing, and unsupported file types return an error suggesting a line range. Pass `paths` (up to 16 targets, each with its own selector) to read several files or ranges in one call. In hashline mode, a whole-file read (no selector, `offset` or `limit`) of a supported-language source file that would exceed the read cap returns a **summarized view** instead of a truncated prefix: imports, type definitions, signatures, closing lines and short bodies keep their real line numbers and anchors, while each long function body is one marker row (`  ⋮ lines 120-188 elided (read path:120-188)`). Only the rows shown are recorded as seen; an edit anchored inside an elided range is refused until that range is read. The footer states the view is summarized and how to read the full file. Small files, explicit ranges, `:raw`, unsupported file types and files with nothing worth eliding read exactly as before. `grep` scans large file sets on up to 8 worker threads (reading and matching only); budgets, skips, caps, cursors, ordering and coverage are applied sequentially in walk order, so a page is byte-identical to a serial scan. `grep` supports Rust regexes, literal text (`literal`), `ignore_case`, `glob`, context rows and a bounded `limit`. Its `output` selects the page shape: `lines` (default) shows matching rows; `files` lists each matching file with its match count in the same sorted order, with `limit`/`skip`/`cursor` counting files; `count` reports total matches and files. Summary pages use the same scopes, ignore rules, exclusions, read limits and coverage/continuation footer as `lines` and record no edit evidence; a capped `lines` page suggests `output="files"`. When a workspace path does not exist, `read`, `edit`, `grep` and `glob` keep their error text and add a `Did you mean:` line with up to five existing paths (case or typo matches, the same name elsewhere, close names in the nearest existing directory); the search reuses the ignore-aware walker, never names private, secret, ignored or dependency paths, and is limited to 20,000 files and 300 ms.
- Reading a directory lists directories first, then files, by name. `.git`, build/dependency directories, the scratch directory, gitignored directories and nested linked git worktrees are named but not expanded. `glob`, `grep`, AST search and the codebase index skip nested linked worktrees (a `.git` file pointing into another repository's `worktrees/`); submodules are still searched. When a search walk covered its whole scope, its footer is one short line (`walk complete; …`, `next_cursor: null`); full counters, omitted paths and the continuation order appear only when something limited the walk. When `grep` stops a page at a cap (per-file matches, files with matches, the match limit or the 48 KiB output budget), the coverage line names it as `page_capped_by=…` with `page_files_with_matches=…`. Matches and files past the cap are not counted; repeat the call with the page's `next_cursor` and `remaining_skip` to read them.
  `glob` treats whitespace as part of one pattern, not as a separator. An empty result for a pattern containing whitespace includes a hint; use brace alternatives or separate calls when multiple patterns were intended. Ignore and access rules remain unchanged.
  An empty glob with a literal directory prefix also explains that the pattern does not override ignore rules. Inspect an explicitly requested directory with `read`, or set the search `path` to that directory; use `gitignore=false` only when ignored descendants are intentionally in scope. A similarly named example elsewhere is not a substitute for the user's target. Search defaults, symlink handling and access boundaries are unchanged.
  Search scope checks stop at the canonical authorized read root. Symlinked ancestors above
  that root (such as macOS `/var`) do not cause a refusal; symlinks in the scope itself or
  below the authorized root remain refused.
  `glob` also accepts `preview=true` when file discovery would otherwise be followed by a source
  read. Paths and pagination are unchanged; after the page footer it appends up to three local code
  previews from the first 16 returned paths, at most 40 lines per file and 6 KiB total, within the
  remaining 48 KiB page budget. Among those candidates, a conventionally named test is previewed
  first when present; the remaining candidates keep their order. This helps inspect test ownership
  before choosing verification without adding excerpts or changing path order. It reuses `find`'s
  confined, secret-screened source reader: sensitive
  paths, unsupported types and credential-bearing sources are not previewed. Complete numbered rows
  are edit witnesses and can be restored on resume only when their source remains unchanged;
  omitted or truncated rows are not editable. The default remains path-only. A preview is source
  evidence, not a reason to skip behavioral verification or assume the file owns the requested behavior.
  When `glob` is offered, built-in guidance recommends previews for discovering a few code files
  that will be inspected, not for path-only lookup. This recommendation does not enable previews
  by default; missing source ranges still require a read.
- `grep` also accepts `preview=true` for local `output="lines"` searches. After the normal
  match page it appends enclosing declarations from up to three code files among the first 16
  matching files, at most 40 lines/file and 6 KiB total, within the existing 48 KiB output cap.
  Complete fresh numbered rows are edit witnesses; long or unavailable declarations are bounded
  or omitted, and undisplayed ranges still need a read. Search coverage and cursors retain their
  ordinary meaning. Summary modes and remote searches report previews unavailable.
- Parallel grep reserves its 64 MiB search-content budget **before** dispatching reads, in microbatches of at most four files (16 MiB of speculative source buffers). Matching rows reuse their exact probe snapshot; edit-witness publication still independently validates freshness. A capped page replays directory coverage only, not file contents or regex matching. Coverage bytes count files processed in page order, not speculative files discarded at the cap; witness-validation and ignore-file reads are separate from the search-content budget. Serial and parallel pages match on an unchanged workspace.
  Anchored grep globs reuse their literal directory prefix for both traversal and continuation
  coverage reconstruction. Line and summary pages distinguish read, walk, match and output limits;
  a footer that evicts rows to fit the output budget reports that output cap explicitly.
- Legacy AST/index traversal now shares search's bounded directory enumeration: at most 50,000 immediate entries are admitted before sorting, with oversized directories omitted wholesale and the index marked incomplete. A separate 2,050,000 raw-entry budget bounds enumeration across directories. Its existing ignore-crate policy (including ancestor rules and Git excludes) remains distinct from paged search policy; explicit scopes, submodules and linked-worktree exclusions retain their semantics.
- Behavior-mode `find` builds syntax indexes only when a fresh file's token/literal evidence can match the query and its quoted-phrase filters. Nonmatching files are not cached as negative results: later edits and different queries are rechecked. Semantic retrieval still admits lexically unrelated candidates for reranking. Fresh source reads, hashes, scope/ignore rules and cache byte limits are unchanged. The related-evidence pass stops building reference spans after its candidate quota is full, but continues searching for requested type definitions and configuration references.
- Saved codebase understanding treats a cited file that no longer exists as stale (`source_status: "missing"`), even when the freshness snapshot is partial.
- **Edit modes.** Hashline is the default. `read` returns fresh `[path#TAG]` snapshots and `N:text` rows; `edit` accepts `PUT`/`CUT`/`MV`/`REM` operations and capture registers. Changed or unseen anchors cannot authorize a write. Syntax-aware block anchors use actual outermost-starting nodes where supported. Replace, apply-patch, structured patch and sloppy are independently selectable globally or by model; see [Editing grammars and safety](#editing-grammars-and-safety).
  Hashline preflights all operation anchors before staging changes. For current snapshots, a rejected patch reports unread ranges across operations and files together and shows a bounded preview, so a retry does not discover just one missing range at a time. Only complete displayed rows gain read coverage; omitted or truncated rows still require a read. Stale snapshots never gain coverage from this preview.
  Native edit results expose `data.edit.outcome` (`applied`, `already_applied`, `source_changed`, or `target_not_found`) when the native owner can establish that classification. `already_applied` is observational: current witnesses and a unique exact desired target are required; old-text-absent retries also need retained prior-source proof that the exact replacement produces the entire current file. It performs no write, transaction commit, changed-path reporting or editor notification, and does not count as progress. Unwitnessed, stale or ambiguous targets and mixed no-op/change batches remain refusals. Deletion, replace-all, fuzzy/sloppy/patch and notebook old-text-absent retries are not certified as already applied.
  Add `verify: [...]` (up to 16 entries) to an `edit` or `write` to run tests once the change succeeds. Entries are test files or `file::test` names (pytest node, unittest `Class::method`, Rust/Go test filter, jest/vitest name) or configured check names; Dial picks the runner (pytest, unittest, cargo, go, jest/vitest) and accepts no arbitrary command. When several mutations in one response name entries, they run once, after the last successful mutation, so a test file written earlier in the same response is included; nothing runs when that mutation fails. The result appends `Verify: Passed N, Failed M` with trimmed failure traces within the per-check diagnostics budget, uses the same command approval, sandbox and evidence ledger as the `verify` tool, and later edits stale the evidence as usual. A plain Python script that is not a test file runs on exit status only and is labelled `(exit status only)`: weaker evidence that earns no behavioral credit. The mutation's success and committed paths stay intact even when verification fails: text and `data.post_edit_verify` or `data.post_write_verify` report the separate outcome; an unresolvable entry is reported, not a pass. Verification must be enabled; child workers refuse a non-empty `verify` before mutation and can use scoped `bash` tests instead.
- **Loop safeguards.** The system prompt is normally stable across requests; plan mode, external thinking and still-connecting MCP servers are restated as a transient request-tail `<dial-notice kind="session_mode">` instead of rewriting it, while a configured model/profile change or a different tool offer can rebuild it; default explicit tool loads change schemas without enumerating them in the system band. Recalled notes and verified repair patterns are decided once per user turn and stored with that message to support prompt caching. A reply that stops at the provider's output-token limit is never taken as the answer. Reasoning-only replies use the reasoning gate below; truncated answer text or tool arguments can retry with a doubled budget that fits the context window, then receive bounded nudges. `harness.max_tokens` defaults to `0` (no operator output cap). Intrinsic tool paging and protocol protections still apply; oversized results report how to fetch the rest.
  Failed tool results receive compact cause-specific feedback in the shared parent/worker engine, including external tools and failures with uncertain effects. Repeated failures do not expand a corrective prompt or append a recovery checkpoint to each request. Unchanged failed file operations are refused until their own source prerequisite changes or the required source is observed; an external change to that same confined source can release the barrier, but unrelated edits cannot. Access refusals are never released by source changes. Each request carries at most one advisory harness note, chosen by priority (safety, failure recovery, repetition/loop, completion review, todo reminder); advisory text that normalizes (case, whitespace, punctuation) to nothing or to generic filler such as "looks good" or "no issues" is dropped, and a non-safety, non-failure note identical to one delivered in the last 3 requests is suppressed; protocol notes, refusals, withheld calls and completion gates are unaffected. Advice preserves permission boundaries, distinguishes verification gaps from code failures, and uses source already returned before asking for another read. Feedback is not proof of a diagnosis or a permission grant; uncertain effects still stop automatic continuation. Numeric-looking malformed read selectors such as `:25- fifty` report selector syntax guidance instead of a misleading missing-file error.
  A sole admitted user request that remains visible is not reissued as a new user-tail instruction after every tool round. Corrections and steering keep their precedence reminders; compaction that removes the original restores the admitted request context. Repeated successful shell observations in Git workspaces become eligible again when the source-tree fingerprint changes, just like failed shell commands. Unknown fingerprints grant neither access nor verification credit.
  Failure recovery preserves the requested target, final state and acceptance conditions: a diagnostic workaround is not automatically the final deliverable. Temporary alternatives must be reconciled before completion. Shared full/compact reporting guidance asks for a short result and essential verification for routine success, rather than a chronology or copied artifact contents; requested detail and mandatory gap disclosures remain.
  Permitted artifact generators may serialize their computed output and verify the delivered file in one invocation. Checks already covering the requirements need not be repeated in another tool call; configured verification gates, source/config editing rules and permission boundaries still apply.
  Native `write` preflight and controller `edit`/`write` target binding preserve the resolver's permission-denied classification for outside roots, parent escapes, symlink escapes and protected storage. These refusals report `blocked` with no effects and select permission-boundary recovery rather than stale-source advice. Malformed paths remain ordinary errors. This changes classification and coaching, not sandbox enforcement; it never authorizes switching tools to bypass a refusal.
  After repeated unsuccessful recovery, the interactive interface asks for direction: continue with
  a corrected approach, supply an instruction, or pause while keeping completed work. Noninteractive
  and sealed runs retain finite loop safeguards; declining the prompt also pauses safely.
  `/todo` opens a read-only plan inspector; selecting a step shows full blocked reasons, findings and recorded evidence. `/todo list` prints the list. The evidence-expansion shortcut expands todo change cards and wraps long reasons.
  Retained artifacts have private lifecycle records. `read` on `artifact://H:info` reports
  availability, retention timestamps and byte size without reading content. A missing blob with
  a record is distinguished from a legacy/foreign reference with no local record; removal or
  expiry cause is not guessed. Lifecycle metadata is storage history, not correctness proof.
- **Silent inference recovery.** Requests managed by the model event pump have a separate
  silence watchdog: by default, 30 seconds without meaningful inference progress cancels only that
  attempt, discards its partial reply/tool arguments, then retries the same context once.
  Hosted reasoning can be silent for minutes, so on remote routes the watchdog never fires sooner
  than the transport's stall limit (`remote_model_idle_timeout_secs`, default 60 seconds, or a
  smaller `worker_timeout_secs`), and is off when that limit is `0`; genuine remote stalls are
  retried by the transport under `retry.max_attempts`.
  Local loading and prompt processing are exempt until generation starts, so a slow cold load or
  prefill is not repeatedly cancelled by the generation-silence limit. Explicit cancellation and
  configured startup/transport limits still apply. Completed tools, files, conversation and todos
  are not rolled back or replayed. After repeated silence, interactive main-model requests ask for
  retry/direction or pause; noninteractive exhaustion reports incomplete while keeping work. The
  old worker must finish before a new attempt can start. Failed/cancelled Dial-managed model servers
  are stopped before retry, ensuring abandoned GPU work cannot block a new request; external servers
  are never killed. Retried inference keeps its request-specific model profile, local hints and
  reasoning controls; they do not leak into a later independent request. A Codex WebSocket stall
  switches recovery to HTTP streaming instead of repeating the same stuck transport. An agent
  watchdog stall keeps that backend on HTTP streaming until it is rebuilt; a transport-level stall
  selects HTTP streaming for that session/account/model. Ordinary user cancellation does not trigger
  this switch. Anthropic lifecycle transitions and nonempty signed-thinking deltas count as activity
  even when no visible text is emitted; pings, empty deltas and usage-only updates do not reset the
  watchdog. This is independent of task/step budgets. Configure it outside the project:

  ```toml
  [request_recovery]
  silence_secs = 30  # 0 disables this watchdog and permits unlimited silent waiting
  retry_once = true  # false requires direction without an automatic retry
  ```
- **Reasoning gate.** Dial-owned MLX-LM requests with recognized thinking markers close the thinking span inside the same generation when token blocks repeat four times, reasoning reaches three quarters of the configured byte/time allowance, or thinking consumes half the output-token budget. Sampling then continues into an answer or tool call with the existing KV cache; this does not end the task or change `harness.max_steps`. Only reasoning is controlled: repeated answer text and tool arguments remain untouched. Speculative/distributed inference, MLX-VLM and external providers retain the client fallback below.
  Main and child agent reasoning time and byte limits default to `0` (unlimited). With positive limits configured, reasoning events and keep-alives do not reset the deadline; hidden reasoning is covered by time, not guessed token counts. Model loading and prefill use their provider timeout rather than the reasoning deadline. A fallback hit cancels only that request and discards all partial calls. One action-focused recovery is allowed per turn; another hit ends the turn as incomplete. Adjust limits in `/settings` or `[reasoning_guard]`. These settings are outside the improvable harness. `enabled = false` disables the time deadline; a positive byte cap remains independent.
  The streamed-reasoning byte cap remains active after answer text or tool arguments start; a short preamble cannot disable it. Main and child agents also compare successful observations, including `bash` results: changed timeout arguments and artifact handles do not disguise unchanged evidence. Failed-call and observation guards share argument normalization: presentation metadata does not disguise a retry, while nested payloads and meaningful arguments remain distinct. Repetition gets explicit investigation feedback; the main agent temporarily withholds the repeating tool until a confirmed editor diff or a successfully completed/blocked todo establishes recovery. Unrelated reads, artifact retrieval, no-op edits, todo polling and plan rewording do not restore withheld tools. Four repeats of the same unchanged observation stop the run; unrelated checks and errors do not erase its history. Only nonzero editor diff summaries reset observation history for fresh verification, and new user steering resets the turn-local guards. A plain, explicit final clarification question uses the shared completion gate: a fresh workspace request with zero observations first gets one investigation nudge, and a necessary question after investigation ends `needs_input`, preserving unfinished todos. This conservative fallback recognizes direct clarification requests, not arbitrary question marks; `finish(needs_input)` remains the preferred explicit protocol. Repeating a substantial explanation three times also sends a loop self-check to the model, even with different calls. Wording alone does not stop genuine progress; result and failure guards enforce those stops.
- **Model I/O robustness.** Remote silence uses `remote_model_idle_timeout_secs`, which defaults to 60 seconds and bounds both the wait for the first event and silence between events; a stalled remote stream fails and is retried under `retry.max_attempts`. Local workers use `worker_timeout_secs`, which defaults to `0` (unlimited). Local model servers (localhost or LAN) are never limited by the remote timeout. Setting `remote_model_idle_timeout_secs = 0` opts out. Positive limits use actual stream progress, not keep-alives or read boundaries. The reasoning gate separately applies any configured pre-action limits. The UI stays editable while waiting; Ctrl-C or Esc cancels. Tool-call arguments that are almost JSON are repaired rather than regenerated; cut-off or trailing-garbage arguments are refused and never executed.
  With a positive timeout configured, native Anthropic streams also bound the first-event wait, including through loopback proxies. Usage-only updates and empty deltas do not reset the idle deadline; actual text, reasoning, signatures, tool arguments, and validated lifecycle events do. `ping` resets it only while a tool call is open, for at most 10 minutes after the last real progress, because Anthropic may send only pings while it buffers a large call's arguments. Timeouts and transient streamed errors use bounded retries, discarding failed partial replies and their tool calls; a retried attempt's streamed text is closed as interrupted, not continued by the next attempt. Cancellation, malformed events, and nontransient errors are not retried.
  OpenAI-compatible chat streams ignore empty/role-only data chunks for idle progress. A nonempty
  `finish_reason` commits the reply; Dial drains optional usage and `[DONE]` for at most 2.5 seconds,
  even with ordinary idle limits disabled, so a finished stream cannot hang on trailing keepalives.
  Optional usage arriving after that grace is unavailable. Empty finish reasons do not commit a
  partial reply; malformed events and cancellation still fail rather than becoming success.
  Common client spellings (`WebFetch`, `WebSearch`, `filePath`, replacement-edit `oldString`/`newString`/`replaceAll`, and shell `workdir`/`working_dir`) are mapped onto catalog names and declared schema properties before validation. Exact tool names and canonical arguments take precedence; conflicts remain validation errors. Compatibility does not load unavailable tools or bypass authorization.
  For built-in `find` only, a single string for `keywords` is normalized to a one-element array, preserving its exact literal text (including commas and spaces). The public schema remains an array, ordinary string-list decoding still applies, and length bounds and authorization are still validated. This does not coerce arbitrary strings into other tools' arrays.
- **Selectable native editors.** Replace and patch grammars support bounded, operator-configurable fuzzy matching; sloppy has its own Unicode/indentation/capture recovery ladder. Ambiguous targets and overlapping edits fail atomically. Canonical replace fields are `old_string` and `new_string`. All modes preserve fresh snapshots, seen-source guards, generated-file protection and complete-target approvals.
- **Forgiving hashline bodies.** A `PUT` body row written without its `+` prefix, or a raw blank line between body rows, is taken as content and the result warns how many rows lacked the prefix, instead of failing the whole patch. Rows that look like unified-diff deletions (`-…`) are still refused. Lines the model was shown are rebuilt from the session's earlier `read`/`edit`/`write` results when the agent restarts or is rebuilt, for files whose content is unchanged since, so a resumed session does not reject edits with "no read of this file is recorded".
- **Compaction is checked, not assumed.** `[compaction]` configures triggers, retained complete tool groups, reserve, method preference, model/agent overrides, speculative and idle maintenance. Available methods are native/remote, image archives, structured handoff, shake and soft summaries. A prepared result is committed only if its branch/model/configuration still match and the full next request, including protected instructions, tools and output reserve, fits. Unsupported native routes, unknown image billing and unreconstructable encrypted history fail explicitly instead of fabricating state. Original journal evidence stays searchable.
- **Stuck-turn detection.** Besides the identical-call guard (warns at the third identical failure, stops at the fourth), dial counts failures whatever the arguments: the same file tool (`read`, `edit`, `write`, `glob`, `grep`, `ast_edit`, `ast_search`) failing five calls in a row gets a warning and ten stops the turn, even with successful reads in between (the read-guess-read-guess loop); `bash` and `eval` failures are task feedback and are never counted. Four rounds in which every call failed add a warning, eight stop the turn. A stopped turn is **reported**, not raised: the reason is recorded in the conversation, shown as a notice, the outcome is `incomplete`, and print mode exits 2 (with `--output-format json` the result has `outcome: "incomplete"` and no `error`). The step limit ends the same way.
- **Step budget.** `harness.max_steps` counts model calls that did work; `0` means unlimited interactive turns, including when an admitted harness has a finite budget. Positive values keep their step limit. Automatic repair and sealed evaluation still have finite caps. A reply that was unusable (truncated answer text or tool arguments, an unreadable call, a tool call written as plain text) is retried without spending a step, up to eight times per turn, and compacting after the server rejected the prompt size is never charged. Sealed evaluation runs get no refunds: their budget is what they measure.
- **Completion and todos.** `todo` supports `set`, `append`, `start`, `done`, `drop` and `block` with a reason; a closed active entry advances automatically. Common shorthand is accepted: `done: true` means `op: "done"` and `task: N` means `index: N`, and the corrected call for an unknown key keeps the given number. A `done` that follows a refused `edit`/`write` in the same turn is accepted with a note that nothing from the refused edit landed; the claim itself is not checked. Lists survive resume and compaction and remain visible in the TUI and protocols. Findings/evidence are optional, and todo updates never trigger verification rounds. Final replies never close execution steps; close them explicitly through `todo`. Open execution steps or deliverables make both plain replies and `finish(answered/completed)` incomplete. Explicit blockers and running workers cannot be silently completed. Plain final replies and `finish` share completion prerequisites, final configured verification and bounded repair. Task outcomes are `completed`, `blocked`, `needs_input`, `incomplete`, `failed` and `interrupted`; unsuccessful print runs exit with code 2. A stop hook that remains blocking after its retry bound reports blocked, and cannot restart a turn waiting for user input. These contracts do not independently prove arbitrary model claims true.

- **Specialist integrations** (available per `integrations` in the config, loaded on demand; never in sealed evaluation/self-play). MCP tools follow `[mcp.*]`; enabling a schema never bypasses approval:
  - `bg`: long-running processes (`start`/`list`/`output`/`wait`/`kill`) under the same sandbox as `bash`, in their own process group, at most 8 at once, 256 KiB of output kept each, all killed when dial exits. Browser tool loading also exposes `bg` when enabled. Start or reuse local preview servers here, wait for readiness, then open the actual HTTP URL; foreground `bash` kills its process group on return/timeout and refuses common dev-server commands before launching them. Stop temporary preview processes you created after inspection, preserving existing user services.
  - `web_fetch` / `web_search`: HTTP(S) fetch with HTML→text conversion and an SSRF guard (loopback, private, link-local and metadata addresses are refused on every connection and redirect), and search through a provider chosen by `[web] search_provider` or the per-call `provider` argument: keyed APIs (`brave`, `tavily`, `exa`, `serper`, `kagi`, `jina`, `perplexity`, `google`, `bing`, `searxng`, `firecrawl`, `parallel`, `zai`, `xai`; each needs its `*_API_KEY` environment variable, and `auto` skips those without one) or keyless HTML scrapers (`duckduckgo`, `startpage`, `mojeek`, `ecosia`; scraping paths that can break, and a bot wall is reported as an error so `auto` falls back to the next provider). Grounded answer providers (`openai`, `anthropic`, `gemini`, `openrouter`, and `codex`) run the provider's own web search and return an answer with its cited sources; they are explicit only (`auto` never picks them). `openai`, `anthropic`, `gemini` and `openrouter` need `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` or `OPENROUTER_API_KEY` from the environment or a key saved for that provider. `codex` needs no key: it runs one search turn on the active ChatGPT subscription session (`/providers` sign-in) and fails when no ChatGPT account is signed in. Grounded providers apply the recency window through the prompt rather than as a hard filter. Gemini's grounding links (`vertexaisearch.cloud.google.com/grounding-api-redirect/…`) are resolved to the page they name through the guarded fetch client, at most four at a time with a 5-second budget each; a link that cannot be resolved keeps its redirect URL. A `[web] search_url` template containing `{query}` overrides all of them. Both need approval and are disabled by `sandbox.mode = "strict"` or `allow_network = false`.
    - Optional `web_search` `fetch_top` (integer 0–3, default 0) fetches the first structured
      result URLs in ranking order through the existing `web_fetch` client and URL/redirect guards.
      Page bodies are capped at 6,000 characters, and combined output at 100,000 characters.
      Individual fetch failures retain the search results and source URL; answer-only providers
      without structured result URLs report that there are no eligible pages rather than parsing
      links from prose. The approved search explicitly includes these fetches; it does not enable
      unrestricted network access or forward search-provider credentials to result pages.
  - `lsp`: definitions, references, hover, implementations, type definitions, symbols and diagnostics, plus transactional rename/code-action edits with document-version, UTF-16, freshness and complete-target permission checks. Server commands/startup remain explicit trusted effects; language servers come from `PATH` or `[lsp.<language>]`. After an edit, if a language server for that file is already running, Dial waits up to 1.5 s and appends `[LSP advisory, not verification]` with up to 8 `path:line:col: message` lines: only error diagnostics the edit introduced, compared with what the server reported for that file before the edit. With no running server nothing waits and no server is started. Turn it off with `[lsp] diagnostics_on_write = false`.
    - `request` (`action="request"`, approval required, may run any server method the session advertises): `query` is one LSP method such as `textDocument/inlayHint`, `payload` is a JSON object of its params, and `path` selects the server. Absent `textDocument` and `position` fields are filled from `path` and the 1-based `line`/`column`/`symbol`. Lifecycle, `$/*`, `window/*`, `client/*`, `*/refresh`, notifications, `workspace/executeCommand`, `workspace/applyEdit` and `codeAction/resolve` are refused with the action that replaces them. Results are bounded to 32 KiB of pretty JSON; a `WorkspaceEdit` result is validated and previewed as one transaction but never applied.
  - `dep_source` (read-only, loadable): the source of a dependency at the version this workspace locked or installed, for cargo (Cargo.lock + cargo registry/git checkouts), npm (`node_modules`), python (project `.venv` site-packages) and go (`go.mod` + `vendor/` or the module cache). Arguments: `name` (optionally `@version`), `ecosystem` (inferred if omitted), `query` (literal search, `path:line` results), `path` (package-relative file with optional `:N-M`, or a directory). It never fetches, installs or runs package code and never leaves the package root. Packages in the resolved dependency caches outside the workspace (`CARGO_HOME/registry/src`, `CARGO_HOME/git/checkouts`, the Go module cache) are readable by this tool under both sandbox policies, never inside a denied path; other paths outside the workspace need the Dev policy or `/add-dir` under Strict. Read-only subagents may use it too.
  - `evidence` (registered by default): artifact-backed immutable citations and bounded `capture`, `snapshot`, `inspect`, `context`, `crop`, `compare`, `freshness`, `derive` and `align` operations. Captures obey current workspace read authority; retaining originals is explicit. Reopening verifies retained bytes, while lineage, coverage and uncertain clock metadata remain untrusted claims, not verification or permission. Media reads and approved monitor matches emit citations; export/relocation and GC follow manifest dependencies. See [evidence.md](evidence.md).
  - `document`, `audio`, `data`, `investigate`, `workbench` (discoverable by default): bounded document structure/revision observations, approved timestamped file audio, reproducible CSV/JSON/XLSX calculations and charts, attributed semantic comparisons/experiment records, and immutable citation inspection/corrections. `/evidence` exposes the workbench in the scrollable terminal transcript. `video` additionally requires `experimental.video_analysis = true` (off by default; frame image tokens can be costly). External converters/backends are operator-installed, never silently installed; no evidence output grants verification or execution authority. See [evidence investigations](evidence-investigation.md).
  - `evidence_search` (discoverable by default): bounded cross-source search over an explicit list of retained evidence citations. Local lexical ranking preserves exact excerpts/values, representation coordinates and source manifests; optional `ranking="semantic"` adds one configured `small`-role model request over bounded candidates, including lexical misses. It is not embedding search, an exhaustive archive index or proof that a result supports a claim. Original sources are not reopened and missing/unsupported inputs and coverage gaps are reported. See [evidence investigations](evidence-investigation.md).
  - `runtime_inspect`, `network_inspect`, `database` (discoverable by default): local trace/profile interpretation, safely projected HAR/OpenAPI comparisons, and isolated read-only SQLite schema/query/plan observations. No live traffic capture, remote connection, application startup or database mutation. Outputs retain source identities and explicit coverage without claiming causality or successful verification. See [system investigations](system-investigation.md).
  - `monitor` (registered by default): explicitly approved, expiring, one-shot error watches on a Dial-owned `bg` job or an existing workspace log file. `start` takes a unique `name`, exactly one of `job`/`path`, optional regex `pattern`, and `duration_secs` (default 3600, maximum 86400). Every start/rearm requires live confirmation even in automatic mode. `list`/`stop` do not read source contents. New matching output can wake the current task in an open idle interactive terminal to diagnose and repair under existing permissions; logs never grant authority. Matched sanitized windows may be retained as evidence citations. No arbitrary PID/stdout attachment, external process control, persisted watch configuration or unattended daemon. See [application-monitoring.md](application-monitoring.md).
  - `image_search` (read-only, offered only while `vision.image_embedding` is configured): ranks workspace images with a multimodal embedding model. `mode=text` matches `query` against images under `path`; `mode=similar` ranks by likeness to a reference `image`; `mode=score` scores up to 32 `images` against the prompt `query`. Results are cosine scores with workspace-relative paths, honouring ignore files and skipping hidden, dependency and protected paths. Vectors are cached per image content outside the workspace, and each call embeds at most 256 new images. While the feature is configured, `image_gen` results also report the new image's prompt similarity. See [model-vision.md](model-vision.md).
  - `image_weights` (discoverable by default; `image.allow_weight_edits = true` and approval required): inspect a local image checkpoint's exact tensor names/shapes/dtypes and source SHA-256, then scale selected F16/BF16/F32/F64 tensors into a new `.safetensors` file. The source, unselected bytes and metadata are preserved; packed quantized tensors, stale source hashes, nonfinite results and overwrites are refused. Source/output paths need ordinary directory authorization. Does not copy the complete model, alter routing or reload MLX-Serve. See [image-generation.md](image-generation.md#actual-layer-weight-edits-local-checkpoints).
  - `model_weights` (off by default; enable with `integrations = ["model_weights"]`; approval required except for `status`): inspection and evaluation-gated weight edits of the Dial-owned local MLX model named by `[weights] model` (default: the main model, so a hosted main model needs `weights.model` set). `operation` is `status` (stored results and edited copies only), `map` (layout and parameter counts from safetensors headers), `stats` (per-tensor/per-layer weight norms, optional `prefix`), `activations` (per-layer contrast for 1–64 matched `pairs`), `trace` (answer-loss change over 1–16 `cases` with each layer skipped, or with each head of `layer` removed in `mode="heads"`), `experiment` (create `experiment` with a `budget` of model runs, or show it), `generate` (1 run: token-limit hits, repetition and checks over 1–32 `cases`), `direction` (1 run: per-layer direction between `negative` and `positive` answers, saved as `name`), `edit` (0 runs: new model copy `name`; by default `direction` projected out of `layers`, or with `method="scale"` exact tensors from `map`/`stats` multiplied by chosen factors in `changes`, optionally on chosen `rows`/`columns`), `attribute` (1 run: ranks attention heads and MLP neurons by estimated loss rise if removed over `cases`, optionally against `wanted` answers; scale `changes` may then name a `layer` with `heads` or `neurons`), `try` (1 run: scores an edit in memory at several `doses` with loss change on `cases` and drift on `neutral` answers, writing no files), or `evaluate` (base vs `edit` on at least 8 held-out `cases`). Quantized edits whose requantization error exceeds half the edit are refused unless `allow_noisy`. Generate, direction, edit, attribute and try require `experiment`. The tool cannot promote or revert edits; the user does that with `/weights promote` or `dial weights promote`. Results are numbers only, with at most 64 tensor rows returned; the full result is stored for `/weights open`. The base model is never written; GGUF and hosted models are refused. See [improvement.md](improvement.md#weight-and-activation-inspection).
  - `image_gen` (automatic when `image.auto = true`, otherwise `integrations = ["image_gen"]`; a configured image service and approval are required to generate): generate one image through a shared image service, independently of the main chat model's image-output capability. Arguments: `prompt`, `output` (a new workspace-relative PNG/JPEG/WebP file), optional `size` (`auto`, `1024x1024`, `1536x1024`, `1024x1536`), and optional `image`, a workspace PNG/JPEG/WebP (at most 32 MiB) to edit instead of generating from scratch. Edits go to MLX-Serve as base64 `image` on its generations route, to OpenAI Images as a multipart `images/edits` upload (gpt-image models and dall-e-2), and to the Responses hosted image tool as an `input_image`. xAI, inline chat-image output and dall-e-3 refuse an edit before any request. The source is decoded under the same limits, read without following a final symlink, and never modified. Without an explicit or configured size, an edit keeps the source's shape. API routes use their provider credential; ChatGPT uses the selected signed-in account. Nothing is sent until an approved call. Destination staging and validation precede generation; the returned image must stay within dimension and allocation limits. When its format differs from the destination extension, Dial converts it locally; matching formats retain the original bytes. JPEG conversion is lossy and drops transparency. Files are never overwritten, staging files are removed on failure, and incomplete/failed Responses turns never publish an image. Provider failures, conversion errors and filesystem races can still fail a paid request. The written file is attached to the result so vision-capable models can inspect it. Image-creation requests offer the tool without a discovery round; availability refreshes after model/account changes.
    `/image generate <prompt>` runs the tool yourself with exactly that prompt, without a model turn: the same approval and plan-mode rules apply, the image is written to a new `image-<first prompt words>.png` in the workspace root (`.jpg` for xAI; numbered when the name is taken), and the result joins the conversation like `!cmd` output. It is not part of turn checkpoints, so `/undo` does not remove it. `/image edit <file> <prompt>` does the same with `<file>` as the source image (a path without spaces); the source is only read, and the result is a new `<source stem>-<first prompt words>.<ext>` beside it (`/image edit kite.png add a sun` writes `kite-add-a-sun.png`), numbered when the name is taken.
    `/image settings` adjusts the current image model's generation settings: default size, and, where the API accepts them, steps, seed, edit strength, LoRAs and per-layer text-encoder weights (MLX-Serve) or quality, background and moderation (OpenAI). Every `image_gen` call for that model applies them; a call's own `size` still wins. See [configuration.md](configuration.md#per-model-image-settings-imagemodelskey).
    - Image generation is a shared tool capability, not a requirement on the main chat model. Dedicated image settings and an assigned image role win; otherwise automatic routing can use main, the active provider, a configured image-capable profile, or keyed OpenAI/xAI defaults. ChatGPT with missing capability metadata offers an approval-gated hosted-image attempt with unverified model/account support; explicit `disabled` is respected. Discovery never probes or spends quota, and a failed request never switches billing routes. See [configuration.md](configuration.md#image-generation-image) for routing order and provider-access limits.
  - `report_tool_issue` (`integrations = ["report_tool_issue"]`, default off, no approval): drafts a bug report about a Dial tool (`tool`, `summary`, optional `expected`, `actual`, `steps`, `category`) with the Dial version and platform. The draft is redacted and bounded to 16 KiB, then saved as an artifact (or returned inline without one). Nothing is posted or sent over the network; the user files the report.
  - `verify`: runs the focused checks the completion gate selects for this turn's changes (build/type checks, related tests, configured `[verification].commands`) and reports each check's status (`passed`, `code_failed`, `environment_unavailable`, `permission_denied`, `cancelled`, `timed_out`, `unverified`), its command and key diagnostics. Commands use the same approval and sandbox path as automatic verification and results go into the same evidence ledger: a pass at the current edits counts toward final verification exactly like a gate run, and any later edit makes it stale unless the check declares `[verification.inputs."name"]` and its declared inputs are provably unchanged (see [configuration.md](configuration.md)). `targets` runs a subset and always executes it: each entry is a check name, a test file or `file::test` (resolved to a runner exactly as for the `edit`/`write` `verify` option), or `failed`, which reruns checks whose latest result was `code_failed`. Results start with `Verify: Passed N, Failed M`, with per-test counts when the runner reports them. Before any change it lists the applicable checks and runs nothing unless `targets` is given. A default `verify` (and the completion gate) may reuse an earlier passing controller-run result of a configured check, turn-locally, only when the operator declared that check's complete workspace inputs, the exact command/config match, fresh bounded before/after snapshots of those inputs are unchanged and no shell/extension effect occurred since; the result is labeled reused with `ran: false` and keeps its original observation revision. Missing, unreadable or oversized inputs disable reuse and the check runs; reproductions are never reused. `repro: {name, command}` registers a shell command that reproduces the reported bug (non-zero exit while it is present) and runs it immediately; it is approved exactly like a `bash` call, rerun by the completion gate, kept for the current turn only (at most 4; reusing a name replaces it), and earns behavioral credit only when it was observed `code_failed` before a change and passes at the final edits (one that never failed is reported `not_shown_to_reproduce`). Offered by default and on the small and lean local tiers when `[verification].enabled` is on.
    - **Standalone artifacts:** when no existing test suite covers a generated file, use
      `verify` with `acceptance: {name, command}` for one focused format/constraint check;
      its command must exit non-zero on a violated requirement. Model-authored helpers remain
      separate from task/operator checks. A no-applicable-check receipt means nothing ran,
      not a pass: do not repeat an unchanged `verify` or hunt for unrelated tests. Reuse
      passing evidence only while its inputs and criteria remain unchanged, and disclose
      any verification gap a meaningful check cannot cover. Empty `targets: []` remains invalid.
    - **Automatic default:** the input-declaration requirement above applies to configured commands. The exact built-in isolated Python AST check can reuse passes without configuration on supported Unix CPython installations. It tracks source, environment and runtime identity; unknown dependencies fall back to execution. See [configuration.md](configuration.md) for runtime assumptions and snapshot bounds. Other built-in builds/tests still execute conservatively.
    - Built-in AST checks use `-I -S -B`: isolated imports, no site initialization, and no bytecode writes. Cold standard-library imports therefore do not change the installed-runtime identity while a syntax check runs; the source is parsed, never executed. Input-capture problems report whether the contract was unavailable before/after execution, concurrent work prevented capture, or source/runtime/environment identity changed. An `unverified` check is not a code failure and earns neither pass nor repair credit; changing or unavailable inputs still require resolution before a rerun can count.
    - **Revision-scoped receipt:** successful execution and no-execution calls return `revision`,
      `results` (empty when nothing ran), and `scope`. Scope reports the applicable check count
      and up to 16 checks without a current pass, with explicit pending/omission counts. A short
      text summary also names pending checks. A passing named subset does not hide other failed,
      unobserved or stale checks or run them implicitly. This is focused-check scope, not proof
      of full behavioral coverage or completion of every requirement. Invalid selections remain
      errors rather than empty successful verification.
    - **Additional failure status:** `failed_unknown_cause` extends the status list above for a
      complete unexplained nonzero result. It is still a failed obligation, but not affirmative
      application-defect evidence, automatic-repair admission or reproduction credit. Diagnose
      the cause without weakening the check; the `failed` target still selects only `code_failed`.
  - `revert_edit` (loadable): restores one file (`path`) to its content before Dial's most recent checkpointed change to it in this turn, or before the turn's first change with `to = "turn_start"`. It refuses files with no journaled change this turn and files changed outside Dial's checkpoint journal since that change (shell commands, external editors). The revert is itself a checkpointed edit: approvals and edit path rules apply as for `edit`, `/undo` still restores the turn, and the file must be read again before the next edit.
  - `ast_search` / `ast_edit`: structural search and rewrite with ast-grep patterns (`$VAR`, `$$$ARGS`) over 28 languages. A search pattern written as a signature without a body (`fn $NAME($$$) -> $T`, `public $T $N($$$A)`) also matches definitions with a body; modifiers such as `pub` still have to appear in the pattern, and `ast_edit` rewrites only what the pattern literally matches. `$$$` captures show the original source span. `ast_edit` previews by default, and a dry run that found changes ends with an `apply_token`: calling `ast_edit` with that token applies exactly the previewed rewrite (approval and checkpoints as for `dry_run: false`) and is refused if any affected file changed since the preview or the token is unknown, spent or evicted (the workspace keeps the last four previews, up to 32 MiB; previews with refused files are not staged); the full `dry_run: false` call still works; applying is all-or-nothing, atomic and checkpointed (`/undo` works). An invalid pattern is refused. When syntax blocking is configured, new high-confidence parser errors refuse the entire batch before commit; otherwise diagnostics remain advisory. `allow_syntax_errors: true` overrides that syntax gate only, including token apply. Both tools remain loadable rather than in the default offer.
    An explicitly selected file that cannot be loaded as bounded UTF-8 source is an error,
    not a successful no-match result. Tree scans report unreadable files separately from content or
    size skips; any coverage warning means the results may be incomplete.
  - MCP: every tool of each `[mcp.<name>]` stdio server appears as `mcp__<server>__<tool>` and requires approval unless the entry sets `trust_read_only_hint = true` and the tool declares `readOnlyHint` (then it runs without approval and read-only subagents are offered it). A server's `readOnlyHint` is not an authorization boundary; it does make the tool an effect-free external read for scheduling (unless it also declares `destructiveHint: true`), so authorized calls in one reply can run together ([async execution](async-execution.md#dependency-scheduling-boundary)). Servers are trusted operator programs (unsandboxed); a server that fails to start only produces a notice. Server size does not determine the default offer: MCP tools are deferred until explicitly loaded.
    Names that sanitise or truncate alike stay unique: the first keeps `mcp__<server>__<tool>` and a colliding one gets `_` plus six hex digits of the SHA-256 of its server and tool names, so the name does not depend on registration order. Descriptions are cut at 2048 characters and input schemas at 16 KiB (annotations dropped first, then nested shapes; the description then says the schema was shortened and the server validates arguments). Results are capped at the agent's 48 KiB per-result limit. A PNG image block is attached for models that accept images, through the same path as screenshots; other image types and audio are noted only. `structuredContent` is appended only when no text block already carries the same JSON.
    With legacy `assistance.compact_tools=false`, the full-context offer includes `mcp_servers` for runtime status. Task routing selects up to **6 additional native/MCP tools**, with a **24 KiB total schema budget**, using the request, indexed capabilities and dependency names from complete `package.json` files already read during that turn. Library/API tasks can expose a generic documentation service without naming its server; named or observed framework domains select their own documentation tools. Native intents select rendered-browser inspection, dependency source, structural search/rewrite, symbol navigation, debugging or prior-session/change history when relevant. Successful MCP discovery exposes matching same-server get/read/query consumers for the next model request, without trusting instructions in result text.
    Routing is deterministic and local: it makes no extra model, filesystem or network calls, and does not execute tools, install servers, grant permissions, persist as pins or expand a child's allowlist. Additions are monotonic within a turn to avoid needless schema churn. Explicit unload suppresses automatic reactivation until the next external request; explicit pins remain durable. Unrelated turns reset automatic selection. Pinned harness lists and small/lean context profiles retain their existing limits. Other tools remain reachable through `tool_search`, which indexes bounded parameter help and enum choices as well as descriptions. Manifest routing currently ignores partial/raw/batched reads and non-`package.json` formats.
    Connected-server instructions join the model prompt only while a tool from that server is offered. Deferred, disallowed and unrelated servers add no instruction block; loading or task-selecting a tool restores its server's instructions before the next model request. Matching uses the registered server identity, including when sanitized or truncated tool names collide. Instructions remain bounded untrusted data and never grant permissions.
    `/mcp list` hides remote endpoint URLs and executable arguments because either can contain
    credentials. Explicit `/mcp test <name>` and extension-browser health checks test disabled
    entries without changing their enabled setting, using the same configured user directory for
    OAuth grants as runtime connections. Browser enable/disable/remove actions update the layer
    that owns the server (project overrides take precedence); the selected install destination
    applies only to new entries.
    Runtime inventory exposes independent connection, notification-channel and catalog states.
    A changed or unverified catalog revokes trusted read-only exemptions. Unknown catalogs are
    revalidated on the next call; changed/stale registered definitions fail closed until reloaded.
    Reconnection restores freshness only when definitions match the registered catalog. Listing
    caches are reused only while notifications are healthy; pagination reports limit failures
    instead of silently returning partial data.
  - `find`: repository retrieval by behavior or exact text. Default behavior mode ranks bounded, source-grounded lexical and syntax-declaration candidates locally; `ranking="semantic"` opts in to an extra configured-model request ranking captured candidate indices. Every row states its attribution (`via=lexical`, `via=exact-text` or `via=model-rerank`); paths, spans and snippets come from captured workspace evidence. `mode="exact"` never calls a model. Semantic ranking is bounded and non-exhaustive, uses no embeddings or probability scores, and errors honestly if the model or response is unusable. Optional `lsp_path`/`lsp_query` adds live LSP metadata. Behavior mode also appends by default (`related=true`; exact mode omits it unless `related=true`) a bounded related-evidence bundle (at most ~6 KiB, three top hits): enclosing owners and signature types as tree-sitter `provenance=syntax` spans, plus call-sites, mentions and config references found by whole-word text match (`provenance=lexical`; not a resolved call graph). `related=false` omits that bundle. Each related row carries `path:start-end` and `source_sha256`, respects scope and protected paths, is omitted when stale, and is not an edit witness. Only optional language-server metadata is resolved symbol data.
    Top code hits can additionally include freshly read, secret-screened source excerpts (up to three files and 40 lines each, within a shared 6 KiB budget). Complete numbered excerpt rows are edit witnesses, with snapshot tags; ranking snippets and related-evidence summaries remain navigation aids and are not witnesses. Omitted, truncated, sensitive or changed content grants no extra edit authority. Restoring a session revalidates retained excerpt rows against the file and its digest; the excerpt can differ from the earlier ranking if the file changed between retrieval and the fresh read.
    When related evidence is enabled, the existing excerpt budget prioritizes the top hit, then
    one conventionally named test and one configuration reference from the already discovered
    related rows, before remaining ranked hits. These are lexical inspection candidates, not
    resolved ownership or proof of test coverage. Unsupported configuration formats remain
    summary-only; no extra search, model request or permission is introduced.
    Related discovery also follows identifier-like literal contract names from `name` methods
    and Rust string-array constants inside the top three hits. It reuses the bounded scope scan for
    `contract-reference` rows across source directories (one representative per directory, up to
    12 per hit, from a pool of 40 directories and eight names, at most four directories per
    source group). Groups (crate/source module, or top-level directory) get priority over extra
    directories from the same group. References match quoted names in code/config files, avoiding
    ordinary identifiers such as `Vec::retain`. This helps map string-based tool names across
    configuration, storage, dispatch and history without separate agent searches.
    Computed names are not inferred. These are lexical inspection candidates, not confirmed
    owners or an exhaustive impact map; scan, row and byte caps still apply. `related=false`
    disables this expansion too; no additional model request or permission is introduced.
  - `browser`: isolated real Chromium tabs, frame-aware DOM actions/evaluation, history navigation (`back`/`forward`/`reload`), screenshots, readable article extraction, dialogs, console/network evidence and sequenced tab `events`, uploads/downloads and approved storage operations. Runs on a built-in pinned Playwright runtime with no operator setup (`playwright_module` overrides it); exact operator-owned local host/port exceptions enable dev-server verification. Downloads use fresh target authority and bounded atomic writes.
    - Navigation and input actions can request `observe: {"snapshot": true, "screenshot": true,
      "events": true}`. Each flag defaults to false; `max_nodes` bounds the DOM snapshot
      (1–1000, default 300). The successful action runs once, followed by observations within a
      budget of at most three seconds, limited by the remaining operation deadline.
      Screenshots use the native image attachment channel. `observations.events` includes only
      new console errors and network failures, capped at 100 entries. Individual observation
      errors are reported under `observations` without replaying or relabeling the successful
      action. Options and supported actions are validated before input/navigation; the cmux
      backend refuses this option. Network approvals, frame targeting and URL guards still apply.
  - `browser` pointer and keyboard actions: `scroll` (element or page, `dx`/`dy` pixels, returns the resulting position), `dblclick`, `drag` (`selector` to `target`), `click_at`/`mouse_move`/`mouse_down`/`mouse_up` (viewport `x`/`y` in CSS pixels, `button` left/right/middle), `wheel` (`dx`/`dy`), `key_down`/`key_up` (`key`), and `highlight` (red overlay for `duration` ms, default 2000, max 10000). Coordinate and keyboard actions are page-level and reject `frame`. Element queries read the first match: `text`, `html`, `value`, `attr` (`name`), `count`, `box`, `is_visible`, `is_enabled`, `is_checked`, and `styles` (`props`, default common layout and text properties, max 32). `wait` also accepts `url` (substring of the tab URL), `text` (substring of rendered text, optionally inside `selector`), or `response` (substring of a response URL); these conditions are mutually exclusive and `url`/`response` reject `frame`. Selector waits keep their existing `state` behavior.
  - `browser` `action=locate` with `description` (at most 500 characters): asks the `grounding` model to find the described element in the current viewport and returns its `x`/`y` in viewport CSS pixels for `click_at`. It also returns `model` and a reminder to verify the result. It fails with setup guidance while the role is unassigned. The `computer` tool's `target.locate(description)` does the same for desktop, display and window screenshots, returning screenshot pixels without clicking. See [model-vision.md](model-vision.md).
    Native browser and desktop captures carry image-digest/media-type receipts and, when known,
    an exact URL/window/display locator. Requirement evidence counts matching pixels only after
    delivery to a subsequent successful model request at the current effect revision, not because
    a tool name or output text says a screenshot was taken. Exact target identity is not build
    identity or a visual-correctness verdict. See [requirement evidence](requirement-verification.md).
  - `eval`: persistent JavaScript (`language: "js"`, the default) and Python (`language: "py"`) state, top-level await, stdout/stderr, results and tracebacks. Recorded `kernel` calls replay as Python `eval` calls. Authorized tool callbacks use the actual owning controller; nested code-mode/turn-control recursion is refused. `tool.batch([{name, args}, ...])` (both languages) returns ordered `{is_error, text, data}` results and runs all-read-only batches together in the parent and in workers; see [execution-and-web.md](execution-and-web.md#persistent-kernels-and-evaluation). Reset/deadline/cancel discards the namespace. OS containment is required and fails closed; JavaScript uses Node 22+ when `eval.node` resolves and otherwise the embedded engine. Use `bash` for child processes.
  - `eval` backgrounding: when `[shell] auto_background_secs` is set above 0 (the default 0 keeps cells in the foreground until they finish or reach `timeout`), a cell still running after that many seconds moves to a `bg` job; `background: true` starts it there at once and `background: false` keeps it in the foreground. `title` (at most 256 bytes) names the job. While it runs, its runtime is busy and host tool callbacks are refused. Named `bash` sessions follow the same threshold, and a session held by a job is busy until the job settles. See [execution-and-web.md](execution-and-web.md#backgrounding-eval-cells-and-named-bash-sessions). The CLI accepts `kernel` as an alias of `eval` in tool and integration lists.
  - `dap`: an actual Debug Adapter Protocol client with initialization/launch/attach, threads, breakpoints, stepping/pause, stack/scopes/variables and output events. Requires an operator-installed adapter and approval; unsupported requests are errors, not simulated debugger state.
    Runtime **probes** already use DAP logpoints rather than a separate `probe` tool. Load `dap`,
    launch or attach to an approved target, and inspect `status` for `supportsLogPoints`. Before
    `configurationDone` (or while configuring an existing session), send for example:
    `{"action":"setBreakpoints","source":{"path":"/workspace/router.rs"},"breakpoints":[{"line":142,"logMessage":"model={request.model}; backend={selected_backend}"}]}`.
    Expressions and formatting are adapter/language-dependent and may have side effects; this
    is not a guaranteed read-only observation. Check `status.breakpoints` for adapter verification,
    then use `output` to collect logpoint events without deliberately stopping the debuggee.
    Optional `condition` and `hitCondition` require the corresponding adapter capabilities;
    hit conditions filter hits and do **not** guarantee capture of only the first N hits or
    automatic disarming. Output retention is bounded and reports losses. Remove the logpoint with
    `{"action":"removeBreakpoint","source":{"path":"/workspace/router.rs"},"line":142}`
    when finished. Setting a logpoint replaces any existing breakpoint on the same source line;
    removal does not restore that previous breakpoint. An adapter that lacks logpoint support
    returns an error rather than silently using a stopping breakpoint. No adapter is installed or
    target launched merely by loading the tool.
  - `notebook`: revision-checked cell listing/read/source edits/insert/delete/move with immutable fresh source witnesses, JSON metadata preservation, stale-output handling and atomic rollback. It never executes cells.
  - `glab_mr` (opt-in via `integrations = ["glab_mr"]`): GitLab merge requests through the `glab` CLI, the GitLab counterpart of `github`. Read ops `mr_list`, `mr_view`, `mr_diff` need no approval; `mr_create`, `mr_checkout`, `mr_comment`, `mr_approve`, `mr_merge` need approval. `mr_checkout` adds a detached worktree (the primary checkout is untouched) and removes it if glab fails. `repo` takes `group/project` or `host/group/project`; when a token environment variable is set, an explicit host not listed in `GITLAB_HOST` is refused rather than sent the token. A missing `glab` or unauthenticated session is reported with install and `glab auth login` guidance.
  - `ida` (opt-in via `integrations = ["ida"]`): IDA Pro reverse engineering through idalib. Each open database runs in its own Python worker process, so the database and its `exec` namespace persist across calls. Actions: `list`, `open` (`db` = binary path, runs auto-analysis), `save`, `close` (saves by default and releases the database), `exec` (Python in the database namespace, with `db`, `ida_domain` and the `ida_*` modules preloaded; the last expression is shown as `=> value`), `rename`, `comment` (`repeatable`), `set_type` (`decl`), and `make_function`. Address or symbol targets accept `0x401000` or a name. `list` needs no approval; every other action does. Settings: `ida.python` (interpreter, default `python3`), `ida.idadir` (IDA install directory, or `IDADIR`), and `ida.timeout_secs` (default 120; `exec` may override with `timeout`). A missing interpreter or an IDA install without a usable idalib is reported with setup guidance. Opening or closing a database writes its `.i64`/`.idb` next to the binary, so those actions count as workspace mutations.
  - Rich `read`: bounded JSON/JSONL selection, PDF pages, notebook cells, OOXML DOCX/PPTX/XLSX extraction and read-only SQLite. Archives (ZIP, TAR including `.tar.gz`/`.tar.xz`/`.tar.bz2`/`.tar.zst`, 7z, RAR, CAB, ISO 9660, and bare gzip/xz/bzip2/zstd streams) are listed or read by member and never extracted to disk. They are decoded in an isolated worker under a 128 MiB decoded cap and 10,000-entry cap; traversal, absolute, backslash and duplicate member names are refused. Bare compressed streams have no `:member` selector. RAR reads are limited to 64 MiB of archive, stored or decodable members only, and refuse encrypted members and multi-volume sets; ISO reads cover ISO 9660 without Rock Ridge or Joliet and refuse multi-extent members. PDF requires Poppler.
    Local rich reads capture bounded authorized bytes using the same root and read-denial checks as the source-investigation tools, including added workspace roots. Parser workers receive those bytes rather than reopening the source path. Extraction caches use content digests (not just size/mtime), authorize before reuse and recheck source bytes before returning. SQLite remains an uncached live read so committed WAL data stays visible. Compact file handles are expanded before format detection in rich reads, `data` (including joins) and `document`.
  - Media `read`: images are downscaled to the 5 MiB model cap, accepting sources up to 64 MiB and 64 Mpx. `path?q=question` on an image asks the `vision` role and returns only its answer; without a configured vision role it reports an error. Without a vision-capable session model, images are described by format, size, dimensions and digest instead of pixels. SVG rasterizes with `path.svg:img`; text inside SVG is not rendered. HEIC/HEIF converts with `sips` on macOS only. `page=N:img` on a PDF renders page N with Poppler `pdftoppm`. Video (mp4, m4v, mov, webm, mkv, avi) returns metadata plus a 3×3 preview grid by default; `clip.mp4:N` reads the Nth frame (0-based), and `clip.mp4:90s` or `clip.mp4:01:23` reads a time. Video needs `ffmpeg` and `ffprobe` (`brew install ffmpeg`); a missing tool is reported with that guidance.
    HEIC, video and PDF image reads require confirmation and execution-controller authorization to run installed converters outside the shell sandbox on a bounded private source copy; context-free reads fail closed. Read these external-converter targets one at a time, not through `paths`. PDF images use the specialist renderer's 2048-pixel bound during rendering and bounded output capture, rather than checking only after allocation.
  - Resource reads: `read` also accepts `pr://N[/diff[/all|/I]]` (pull request diff), `issue://N` (issue body, labels and comments, marked untrusted) — both via `gh`, optionally qualified as `[host/]owner/repo/N` — and `conflict://`. A bare `conflict://` lists Git's unmerged paths with hunk counts; `conflict://PATH` summarizes each hunk's ours/base/theirs line ranges, then reads exactly the marked lines through the normal read path, so the output carries edit anchors and a hunk is resolved by replacing its whole range. Path permission rules judge `conflict://PATH` as a read of `PATH`. Unterminated or nested markers are reported instead of guessed.
  - URL `read`: `read` accepts `http://` and `https://` URLs and fetches them through the same guarded client and network gate as `web_fetch` (approval required; refused when the session sandbox disables networking; loopback and private addresses blocked). Downloads are capped at 64 MiB, and selectors, `offset`/`limit`, archive `:member` and document conversion behave as for local files. Port numbers stay part of the URL. The result is untrusted web content; binary non-document responses (images included) are refused with an explicit error.
  - Shell/background output is streamed to the UI and spooled with bounded head/tail retention, not unlimited full retention. Typed output facts distinguish capture, retention and presentation completeness (`complete`, `partial`, `unknown`), with observed/retained/omitted byte counts and artifact references when available. An artifact can retain only part of an otherwise fully captured stream. `artifact://<sha256>` references survive compaction; `read` retrieves retained line ranges (`artifact://H:50+20`), exact base64 byte pages (`:bytes=O+N`), lifecycle records (`:info`) and human annotations (`:annotations`). Abandoned temporary spools are removed.
    Complete artifact-backed Cargo/Rust test output can omit recognized build progress and passing
    test lines even when the command fails, preserving diagnostics, failures, totals and status.
    Completed background reads/notifications use the same minimizer only for untouched whole
    captures; partial/delta, explicit tails and PTY output are not classified as complete runs.
    Large Git status results return exact XY counts and 20 bounded path samples with full output
    retained in an artifact. `git` action `status` accepts `path` to narrow scope; incomplete capture
    refuses exact counts, and unavailable/incomplete artifact retention falls back to raw output.
    For lost Git work, `git` action `reflog` returns recent HEAD movements (default 20 entries,
    at most 50 with `limit`), including abandoned detached-HEAD commits. Use this evidence to
    identify the user's work rather than assuming a larger dangling commit chain is the target.
    The action is read-only and retains the normal Git configuration safety guards.
  - `bash` runs in the project root (`cwd` selects a subdirectory; no `cd <root> &&` prefix needed). `background: true` starts a job at once. By default (`[shell] auto_background_secs = 0`) a foreground command returns its result when it finishes or is killed at its `timeout`, so a long build or test costs one round instead of an adoption notice plus `bg wait` calls. Setting `auto_background_secs` to a positive number opts into automatic adoption: a command still running after that many seconds becomes a background job, an explicit `timeout` longer than the threshold keeps it in the foreground until that timeout and then moves it to `bg` rather than killing it, and a shorter `timeout` kills it as before. When `bg` is loadable but not yet offered (lean and local routes whose core budget leaves it out), a command that becomes a background job, including one started with `background: true`, loads `bg` for the next request, so no separate `tool_load` call is needed. Completion is delivered to the model in the next request, and a `bg wait` that sees the job exit returns its unread output, so no separate `bg output` call is needed. A non-service job that exits successfully before an `until` pattern or port is established is reported as finished, not as a failed wait; a nonzero exit or a service exit is still not ready. A foreground command's result names entries it created directly in its working directory (Dial's scratch directory excluded), so build products and scratch files left behind are visible. Pending bash jobs return control to the model rather than silently blocking completion; continue independent work or explain what is pending and use a bounded `bg wait`. Waits and readiness checks default to 5 seconds (explicit `timeout: 0` remains unlimited). Job output has its own live transcript panel; `bg output` and artifacts retain the captured output. Read-only job waits do not hold the workspace mutation lock. `bg` remains available subject to the session's mode and tool allowlist.
    `pty: true` allocates a terminal at launch and immediately returns a managed `bg` job handle, reusing the existing terminal runner. Use `bg input/output/wait/resize/kill` with its name. PTY mode rejects explicit `timeout` and `session`, has no implicit process deadline, and honors `--no-pty`; it never retries or converts an existing pipe command. Omitted or false `pty` retains ordinary captured execution. See [shell interactive programs](shell.md#interactive-programs).
    Background observation success is separate from child execution success: a successful wait
    can report a nonzero child exit, and a readiness deadline can return an unsatisfied observation
    without claiming that the child failed. Controller-owned process and observation facts are
    preserved independently of output text.
    Use `write`/`edit` for hand-authored source/config changes. A permitted build or computation
    may write its requested derived artifacts directly (for example, computing and serializing
    a CSV in one command), avoiding transcription of intermediate values. This distinction does
    not expand path access, approvals or authorization to rewrite source files.
  - Bash interceptor (`[bash_interceptor]`, on by default in `hint` mode): a command that is only a duplicate of a dedicated tool (`cat`/`head`/`tail` or `sed -n 'A,Bp'` → `read`, `grep`/`rg` → `grep`, `find -name` → `glob`, `sed -i`/`perl -i` → `edit`, `echo >` → `write`, `nohup`/`&`/dev servers/watch mode → `bg`) still runs, and its result gains one `[hint]` line naming the tool. A leading `cd DIR &&` and trailing `| head`/`| tail` count as part of the duplicate; batched compound commands, pipes into other programs, heredocs and `find` with `-exec`/`-delete`/time or size predicates are left alone, since one batched call saves model round trips. A hint names a tool only when the current request offers it. `gh run view|list` or `gh pr checks` output that still shows an unfinished run (or a `sleep` loop around them) gains a hint to wait once with `github` op `run_watch`, or `gh run watch <id> --exit-status` plus `bg` `wait`, instead of polling. `mode = "block"` refuses any command with a matching segment; `enabled = false` turns it off; `patterns` replaces the built-in rules. User `!` commands are never intercepted.
- Native tool calling over streaming. When a local server does not parse tool calls itself, Dial reads the text formats local models emit: `<tool_call>` JSON (Qwen/Hermes), GLM `<tool_call>NAME<arg_key>…</arg_key><arg_value>…</arg_value></tool_call>` pairs (values are raw text, like Qwen3-Coder's), Qwen3-Coder `<function=…><parameter=…>` XML, DeepSeek `<｜tool▁call▁begin｜>function<｜tool▁sep｜>NAME` with a ```` ```json ```` argument fence, Kimi K2 `<|tool_call_begin|>functions.NAME:IDX<|tool_call_argument_begin|>{…}`, Mistral `[TOOL_CALLS]`, Llama 3 `<|python_tag|>` and gpt-oss harmony `to=functions.…` messages. An unreadable block becomes a malformed call that is reported to the model and never executed. The `<｜tool▁calls▁begin｜>`/`<|tool_calls_section_begin|>` section markers around them are removed with the calls. On loopback and private endpoints, a well-formed call that closes the reasoning of a reply with no answer text (inside an unclosed `<think>` block, or a server's reasoning field) is lifted too; reasoning that continues after the call, cut-off calls and fenced JSON examples never run. A reply that still holds the opener of one of these forms and names an offered tool, or a JSON `name`/`arguments` object (also in a code fence, with any spacing) naming one, is treated as a tool call written as text and gets the usual constrained retry or nudge. Tool names are matched after dropping a namespace prefix (`functions.`, `default_api.`, `tools.`) and a `:N` call index; a name that differs from exactly one offered tool only in case or `-`/`_` resolves to it, and ambiguous names stay unknown. On loopback and private endpoints, a reply whose whole text is a list of pythonic keyword calls to offered tools (`[read(path="a.rs")]`, optionally inside `<|python_start|>`…`<|python_end|>`) is lifted too; positional arguments, unknown tools and any other text keep it plain text. Such a reply is streamed before it is lifted.

**Safety**
- Approvals: `write` (default) runs workspace edits, file writes and non-destructive shell commands without a prompt and asks before destructive or privileged commands (recursive deletes, history-rewriting git, `sudo`, piping downloads into a shell) and tools that leave the workspace (network, browser, MCP). `ask` presents Allow once / Always allow this session / Always allow in this project / Deny before every mutation, shell execution and operator tool; `auto` / `-y`; `readonly`. Project-wide grants are saved to the project's `permissions.toml`; before saving, a confirmation states what the grant covers, the project root it applies to, that it lasts until revoked in `/permissions`, and that non-matching calls still ask and deny rules still win. Always-allow for bash records a command prefix, not blanket shell access. Unsupported compound shell syntax cannot inherit a prefix grant. Without a terminal, `ask` and `write` deny what they would have prompted for unless a named tool or permission rule authorizes it.
- Shell sandbox (macOS `sandbox-exec`, fails closed): `dev` (default) can read most of the machine but never `~/.ssh`, `~/.aws`, cloud/keychain dirs or dial's own state, writes only the project (plus configured `extra_write`), secret-looking environment variables are dropped; `strict` is workspace-only with no network; `off` is an explicit opt-out. `sandbox-exec` is deprecated and this is not a security boundary against a determined local attacker.
- Git metadata stays read-only inside writable roots in `dev` and `strict` on macOS and Linux: each root's existing `.git` (directory, `gitdir:` file or symlink), a linked worktree's git directory and its common directory when they lie inside a writable root. Sandboxed commands can read it (`git status`, `git diff`, `git log` work) but cannot change hooks, config, refs or the index, because unsandboxed `git` would later run what they planted. Commit, stage and switch branches with the `git` tool, which asks for approval. `git init` in a directory without `.git` still works. Nested repositories and submodules below a root are not covered, and on Linux a symlinked `.git` is protected through its target only. To let sandboxed commands write Git metadata, list it in `sandbox.extra_write` (for example `extra_write = ["/path/to/project/.git"]`).
- Per-domain network rules: `sandbox.network_allow` and `sandbox.network_deny` take host patterns, `example.com` (exact), `*.example.com` (subdomains, not the apex), `*` (any) or an IP address. When either list is set and the sandbox allows network, sandboxed commands get no direct network. On macOS the profile permits only the loopback port of Dial's filtering proxy. On Linux the command runs in its own network namespace and a small bridge (Dial re-executed with `--dial-net-bridge`) forwards `127.0.0.1:3128` in that namespace to the proxy's Unix socket. `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` (and lowercase forms) point at the proxy, and `NO_PROXY` is cleared. The proxy handles `CONNECT` and plain `http://` requests. It answers `403` (header `X-Dial-Sandbox: network-policy`) for a host that the deny list matches or that the allow list (when set) does not, and for a host that resolves to a loopback, private, link-local or other non-public address, unless that exact host (no wildcard) is in the allow list. Deny wins over allow. Clients that ignore proxy variables, and non-HTTP protocols such as raw SSH, cannot connect. Rules are fixed when the session starts. The `bash` tool and durable background jobs use them, while Dial's own `web_fetch` and model traffic do not.
- Checkpoints: file edits made through Workspace tools (write, edit, ast_edit and similar) are journaled per turn. `/undo` restores the files linked to the last conversation turn and then drops that turn; `/undo n` selects the last `n` conversation turns, including read-only turns with durable empty checkpoints. `/rollback n` instead restores the last `n` file-changing checkpoints and keeps the conversation. Legacy, missing or foreign/forked checkpoint links are refused rather than guessed. Both commands expose restoration warnings; incomplete `/undo` keeps the conversation and durable restoration progress for an inspected retry, although some files may already have changed. Branch-admission hooks run before restoration. Shell commands and approval-requiring extension calls, including `git` switch and branch creation, are uncheckpointed: review them with staged and unstaged Git diffs.
- Durability: edits, checkpoint records, session journals and tool-output artifacts are flushed with the platform's ordinary `fsync`. On macOS this no longer forces `F_FULLFSYNC`, so a power loss can drop writes still in the drive's cache; process and kernel crashes are unaffected. Checkpoint snapshots, manifests and revision copies are published before the edit proceeds and flushed in the background; Dial waits for that flush before the next turn and before `/undo` or `/rollback`. A snapshot cut short by a power loss is detected by its recorded length (or, for `revert_edit`, its digest) and refused rather than restored.
- Configuration, state, memory, sessions and evaluation data live under `~/.dial` (or `DIAL_HOME`). You can run Dial from your home directory without relocating this storage: native file tools and sandboxed commands exclude the private tree, even when it is inside the workspace. Starting a workspace inside the private tree itself is still rejected. Shell sandbox `off` remains an explicit isolation opt-out.
- Linux/WSL2 masks existing private paths inside writable roots. If a denied path there does not exist (for example a credential directory directly under HOME), sandboxed commands still fail closed rather than creating host directories or risking disclosure of credentials created later. This restriction does not apply to macOS.

**Context**
- Project files: `AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `.cursorrules`, `.dial/AGENTS.md` up to the git root (`GEMINI.md` or `.gemini/GEMINI.md` at the root as a fallback); `context_files.extra` adds plain file names read beside each. Linked worktrees also read the primary checkout's instruction files; OpenCode `instructions` entries from `opencode.json` (or `opencode.jsonc`, with comments and trailing commas) are read from the project root; `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` directories contribute their `AGENTS.md`. `~/.dial/SOUL.md` for identity; `.dial/SYSTEM.md` / `APPEND_SYSTEM.md` overrides. Instruction files appear under one `Project instructions` heading, each wrapped and attributed by path; the model follows them for work in the repository, below the user's direction and operator guidance, and they never change approvals or the sandbox.
- `@file`, `@dir/`, `@diff`, `@staged` references in messages.
- Skills (`.dial/skills/<name>/SKILL.md`, `~/.dial/skills`, agentskills.io format) loaded on demand; the agent can create and refine them (`skill_manage`).
- Provenance memory lives in private `$DIAL_HOME/memory.sqlite3`. One `memory` tool takes a required `action`: `recall`, `retain`, `update`, `forget`, `history` and approved `import`, supporting project/global scope, source/session provenance, revision-checked corrections (`update` by the id and revision `recall` returns) and expiration. `recall` and `history` need no approval; the other actions follow the normal approval policy. `memory` is offered by default. Relevant bounded entries are retrieved on demand with lexical BM25 ranking. Legacy `MEMORY.md`/`USER.md` import is explicit (`confirm=true`) and preserves originals; old configuration tool selectors (including the former split names `recall`, `retain`, `memory_update`, `forget`, `memory_history`, `memory_import`) are migrated to `memory`, not kept as executable aliases, and recorded calls under those names resume as `memory` calls with the matching action. Forget is logical redaction, not forensic disk erasure.
- **Saved codebase understanding.** Dial automatically builds a project-scoped source map in private `$DIAL_HOME/projects/<identity>/codebase.sqlite3`, outside the repository. It records file outlines, actual syntax declarations, imports, module documentation and bounded manifest/README excerpts; unchanged file hashes reuse saved outlines across restarts and new sessions. Each turn refreshes changed sources in a background worker. With `[codebase] ambient_context = true` (off by default) a bounded, task-relevant brief is attached to each user message, keeping the system prompt stable, with no extra model request: only entries whose cited sources still match their saved hashes (at most four findings with clipped summaries, source paths and revisions as links; outline lines matching the task as `path` + `line: text`), stale findings named but never summarized, no directory inventory, and a reminder that the user's message and constraints take precedence and that the brief grants no edit access. With it off (the default), retrieval is left to the loadable `codebase_memory` tool. The brief is capped by `efficiency.ambient_context_bytes` (default 8 KiB). Independent child agents reuse the same map. Agents save deeper responsibilities, relationships, entry points and check commands with `codebase_memory` `action="remember"`, a `topic`, factual `summary` and 1–8 supporting `sources` paths. Findings carry source hashes, the originating parent session and a revision; changed, removed or inaccessible sources withhold the old summary and expose the topic/revision/paths for targeted refresh. When every cited source is gone, architecture sections and findings are shown compactly (identity, revision, status and how to invalidate/forget or re-synthesize) without source or dependency path lists; saved findings are never deleted automatically. These are agent observations, not independent verification proof. `action="recall"` retrieves more; `refresh` updates the map and drops index entries for deleted files even when a scan is partial; `forget` removes a topic. Updates and forgetting require the observed `expected_revision`; a new topic defaults to 0. Cache operations do not edit project files or require a project mutation approval. The automatic map respects ignore rules and excludes generated directories, links, binary and oversized files; copied text uses the existing secret/instruction screening. Scans are bounded to 2,048 files, 16 MiB and three seconds, with partial coverage reported; files over 256 KiB are omitted and at most 128 findings are saved. Source outlines are structural excerpts, not a complete call graph or generated architectural explanation. Default turn context is 8,000 bytes. Configure `[codebase]` or `/settings` → Models → Project understanding; disable it with `enabled=false`. Sealed evaluations receive no persistent project knowledge.
  In the ambient brief a source appears only when the task names it, a distinctive path word matches, or an outline word matches (two outline words when the task has more than six words, since a long prompt shares some word with almost every file). Path words shared by more than a fifth of the indexed files (such as `crates` or the project name) and generic ones (`src`, `mod`, `tests`) do not count, and a generic file name such as `mod.rs` or `tests.rs` counts as named only together with its directory. Test, benchmark and fixture sources must be named, or the task must mention tests or benchmarks and match their path. Explicit `codebase_memory` recall keeps the complete view.
- **Code change history.** With `[codebase].history = true` (the default), the end of every turn that changed project files records one version per file in private `$DIAL_HOME/projects/<identity>/history.sqlite3`: a unified diff from the file's content before the turn's first write to its content when the turn ends, with the git branch (or `detached@<sha>`), the prompt that produced it, the session and the turn number. No model calls are made. Diffs are redacted for recognized credentials and capped at 32 KiB each (`[diff truncated]`); binary files record only that they changed; private paths (`.env*`, `.git`, `.dial`) and files in added directories are skipped; the oldest versions beyond 5,000 are pruned. The loadable `code_history` tool searches these across sessions: `action="search"` with `query` words (full-text over path, prompt and diff; newest first without a query) and optional `path` (file or directory) and `branch` filters, and `action="show"` with an `id` for one diff. `/code-history [words] [path:<p>] [branch:<b>]`, `/code-history show <id>` and `/code-history clear` do the same in the terminal. Only changes made through Dial are recorded, so history is not a substitute for `git log`; results describe past versions, not the current file.
- Sessions: exclusive-writer JSONL with crash/durability reporting, resume/fork, branch-aware rewind, configurable atomic compaction and full-text `session_search`. Read-only snapshots never claim mutation ownership; experimental notes/reset records remain branch-local.
- Historical evidence stays append-only. The request context is a projection of it, built to keep the conversation prefix byte-identical from step to step so a provider or local server can reuse its prompt cache: recalled notes are stored with the user turn they were recalled for and placed ahead of its text, a superseded read becomes a fixed placeholder once (and only while little text follows it, or when the context is filling up), and under context pressure old, large `read`/`grep`/`glob`/`find`/`bash`/`lsp`/web results outside the newest fifth of the window are replaced once by a pointer to their full text (`read` on the `artifact://` URI retrieves it; such retrievals are never pruned themselves). Mutation results, todo and memory updates, and small results are never replaced. `working_state` preserves explicit goals, constraints, decisions and unfinished work; compaction also carries the original goal and evidence references. Agent notes are not verification proof.
- `working_state` `update` replaces ordinary notes but preserves omitted deliverables and their
  runtime provenance. `view` returns a revision; `patch` requires `expected_revision` and preserves
  every omitted field, refusing stale writes atomically.
  Optional addressable `claims` carry `id`, `text`, `status` (`unverified`, `supported`, `retracted`,
  `superseded`) and `source_refs`; even `supported` is a model assertion, not verification proof.
  State is limited to 32 KiB and 64 claims. Compaction carries an exact machine-readable projection
  of the current request, original goal, working state, todos and evidence-index ownership outside
  lossy summary prose. Oversized critical contracts fail explicitly without committing compaction
  rather than dropping constraints; historical working-state tool calls remain compatible and inert.
  Optional `deliverables` (up to 32) links concrete outcomes (`id`, `description`, `status`) to
  model-reported `evidence_refs`: open, in-progress and blocked outcomes prevent completion;
  removing, cancelling or redefining an outcome requires explicit human scope confirmation through
  the existing question surface. Refusals, timeouts and unattended surfaces preserve the entire state.
  Confirmed redefinitions reopen the obligation and clear old evidence. Runtime-owned provenance and
  confirmation receipts cannot be supplied as model arguments; legacy cancellations without a receipt
  still block completion. Satisfied outcomes require references. A `check:<name>` reference
  must resolve one selected/configured check with a current passing observation; missing,
  stale, failing, unavailable or unknown linked checks block completion without executing them.
  File/artifact references remain unverified model claims merely by being named. Operator-owned
  `verification.requirements.<id>.deliverables` mappings independently require every mapped check
  and visual obligation to be current before a deliverable claimed `satisfied` can complete, even
  with `strict_requirements = false`. Omitting a `check:` reference or citing an unrelated pass
  cannot bypass that contract. Completion evidence keeps claimed status and references separate
  from controller acceptance (`observed`, `needs_verification`, or `unassessed` without a mapping),
  without treating omitted requirements as delivered.
  Runtime user-message provenance (original text, journal ID and turn/steering origin) survives
  goal continuation and critical carry separately from working-state assertions. Earlier overflow
  is explicit; recover missing scope from session history. Harness continuations never replace
  the current user instruction.
- Custom slash commands: `.dial/commands/*.md` (`$ARGUMENTS`, `$1`…).
- Session commands include `/help /new /sessions /resume /fork /tree /label /rename /export /copy /plan /permissions /cost /compact /undo /rollback /todo /skills /memory /commands /search /approval /outcome /context /improve /understanding /laya /reasoning /thinking /exit`. Launch-only modes have in-session switches: `/advisor on|off` (coach-role review of each turn, like `--advisor`), `/external-thinking on|off` (private scratchpad, like `--external-thinking`) and `/plan-yolo [MODEL|off]` (auto-approve the next plan and hand it to MODEL, like `--plan-yolo-into`). CLI maintenance runs in the background with its output in the transcript: `/gc [apply]`, `/anonymize [SESSION] [-o PATH]`, `/images [status|doctor|probe|purge]`, `/update [VERSION]` (then `/restart`), `/benchmark [flags]` and `/schedule list|add|remove|log|run`. `/relay start [ADDR]|stop|status` runs a local collaboration relay for `/collab`, and `/hooks` lists the configured lifecycle hooks. Independent supported read tools run concurrently with mutation barriers and ordered results; read-only `bash` commands (the safe-command gate) join those groups unless they set `session`, `pty`, `background` or `async`, which stay serial. Interrupted tool-call groups are closed with explicit results for safe resume; automatic compaction is announced. Process startup and non-cancellable external tools may still delay interruption.
- `dial gc` reclaims disk space and only reports until `--apply`. With no sweep flag it removes unreferenced artifact blobs, archives cold sessions and checkpoints SQLite WAL files. Opt-in sweeps delete history: `--stale` (dangling artifact records), `--subagents` (worker transcript directories) and `--orphaned-projects` (other projects' Dial state whose project directory no longer exists). The last two delete only directories with no file changed for `--prune-after-days` (default 30); the current project and projects whose directory is unknown are always kept.
- `/graphify [tree|html|callflow]` renders the project's [Graphify](https://github.com/Graphify-Labs/graphify) knowledge graph and opens it in the browser: `tree` (the default) is a collapsible tree by directory, `html` an interactive graph (one node per community above 5000 nodes) and `callflow` Mermaid architecture and call-flow diagrams. It reads `graphify-out/graph.json`, or the directory `$GRAPHIFY_OUT` names, runs Graphify's exporter through `uvx --from graphifyy` as a background job with its output in the transcript, and writes the page beside the graph (`GRAPH_TREE.html`, `graph.html` or `callflow.html`). [uv](https://docs.astral.sh/uv/) must be installed; without it the job reports that `uvx` was not found on PATH. It never builds the graph; when none exists it names the build command (`uvx --from graphifyy graphify extract . --code-only`). It runs at once even during a turn. With `DIAL_NO_BROWSER` set, or when no opener works, it reports the page path instead.

## Desktop window targeting and recovery

The `computer` tool distinguishes a running application from an available target window.
On macOS, discovery lists at most 48 on-screen, shareable windows. Hidden/minimized windows,
other Spaces and capture policy can exclude a window; absence does not prove it closed or
justify reopening the app. Explicit IDs use targeted lookup rather than that list cap, but
still require an available, capturable window. Refresh discovery and verify the intended
target; never replace a missing main window with an application's popup.

Background keystrokes on macOS are process-routed, not reliably window-routed. A non-minimized
sibling (including a password-manager popup) therefore causes a safety refusal. Prefer an
exact-window accessibility action; otherwise use authorized foreground input on the original
window. A listed `focused` flag alone does not make process-routed keys safe.

Native menus support `win.menu.select(["File", "New Tab"], {takeover: true})` for explicit
foreground recovery after a background key-context refusal. Omitted `takeover` follows an
acquired task-control grant, otherwise it defaults to background; explicit `false` remains
background. macOS must establish that exact native window as key before traversal and again
before command dispatch. There is no implicit escalation, sibling substitution or retry.
Foreground input and task-control acquisition retain normal interactive approvals; menu
commands with consequential effects require user authorization. If delivery or restoration
may already have occurred, inspect the target before retrying rather than duplicating a command.

Accessibility `find({role})` uses case-insensitive **exact normalized roles**: `tab` is not
`tabgroup` or `table`. Title/value filters remain case-insensitive substring matches. Nodes
advertising actions but no press action include bounded action hints in the text tree.
Inspect `actions()` before choosing an operation: `AXShowMenu` opens a context menu and must
not substitute for activation. Use the verified element's click or fresh window pixels when
the intended control exposes no activation action. Unsupported actions fail without dispatch.

## Editing grammars and safety

`edit.mode` defaults to `hashline` and accepts `replace`, `hashline`, `apply_patch`, `patch` and `sloppy`. Replace uses
`path`, `old_string`, `new_string` and optional `replace_all`; no `old`/`new` aliases.
Hashline, apply-patch and sloppy accept one `input` string. Structured patch accepts
`path` and `edits` with `op`, `rename` and `diff`. Sloppy uses `*** Edit File`, `*** Find`
and `*** Replace`/`*** Insert Before`/`*** Insert After`; its bounded recovery ladder is
separate from replace/patch fuzzy matching.

### Concurrent snapshot edits

Agents can read and prepare edits to the same file without claiming it or exchanging coordination
messages. Submit the ordinary `[path#TAG]` patch using the line numbers from that snapshot. Dial
serializes cooperating writers only during the edit call, acquiring every source/destination in
path order before inspecting current content. Unchanged, contiguous anchors are translated onto
the current file; ambiguous placements and overlapping changes are refused, never overwritten.
External editors remain subject to exact filesystem identity checks rather than these leases.

A rejected multi-file batch writes nothing and reports conflicts across files together. Each
overlap includes bounded current context and a fresh tag; unaffected anchors in that file include
snapshot-to-current range/gap mappings. Unchanged previously read rows remain witnesses under the
fresh tag; only complete newly displayed rows gain read coverage. Repair and resubmit the complete
batch, not just the conflicting operation. Omitted or truncated conflict context may still need a
targeted read. Whole-file deletion and changed notebook projections still require fresh reads.

Change notices are deduplicated and bounded. Changes outside observed ranges, including line shifts
that the editor can translate, do not produce notices. Replacements/deletions of observed rows,
insertions inside observed ranges, unavailable files and changes too large to map conservatively
still invalidate context. Notices never grant read coverage. No extra model calls or new tool
arguments are needed; this does not guarantee semantic compatibility between disjoint code edits.

### Editing configuration

```toml
[edit]
mode = "hashline"
fuzzy_match = true
fuzzy_threshold = 0.95
streaming_abort = false
recover_inline_edits = true
block_auto_generated = true
enforce_seen_lines = true

[edit.model_variants]
"openai/gpt-5" = "apply_patch"
"anthropic/claude" = "sloppy"

[edit.blackbox]
enabled = false

[edit.auto_repair]
enabled = false
```

Process/CLI pins take precedence over an explicit model-profile `edit_mode`, a learned harness pin,
model selector rules, the applicable local-assistance default, and the global mode, in that order.
Unpinned local routes default to `replace`; hosted routes default to the global `hashline` mode.
`assistance.local.edit_mode = "inherit"` makes local routes follow the global mode instead. When a
local route replaces an explicitly configured `[edit] mode` (and no pin decides), Dial says so once
per session, naming both modes and the `inherit` switch.
Selector matching is case-insensitive; the longest matching substring wins, with deterministic
lexical ties. `--edit-mode` and `DIAL_EDIT_VARIANT` accept all five modes; environment value `auto`
inherits this policy rather than benchmarking or adaptively choosing a grammar. The per-model
Edit style setting shows the inherited grammar without persisting it as a model-specific pin.
`DIAL_EDIT_FUZZY` accepts `1`/`true`/`0`/`false`; `DIAL_EDIT_FUZZY_THRESHOLD` must be finite in `[0,1]`.
Loading a valid saved configuration migrates the old root `edit_mode` into `[edit].mode`;
model-profile and harness `edit_mode` fields remain intentional per-model pins.

Whole-file writes whose bytes already match the existing file are refused without committing a
replacement. Reread the relevant range and make a targeted edit, or check the remaining behavior;
resending identical file contents is not progress. New files and genuinely changed replacements
still use the normal read-witness, approval and transaction checks. An overwrite of a file of 40+
lines that leaves at least half of its lines unchanged succeeds but adds a rewrite advisory naming
the unchanged count and recommending `edit` for further changes, since regenerating a whole file
costs far more output than a patch. The hashline `edit` description shows one call carrying several
operations and several file sections; every planned change should go in one call.

Source excerpts appended by `glob`/`grep`/`find` previews count as read: a later `read` of rows
they already showed in full, for the same unchanged file, is answered with a pointer to that result
instead of the rows again. The `read` description asks for `grep` or `:outline` first on large or
unfamiliar files and targeted ranges instead of paging.

Replace-mode misses explain themselves: when the first line of `old_string` occurs in the file, the
error leads with that line number before any fuzzy closest match. Identical `old_string` and
`new_string` absent from the file are refused before any search as code the file does not contain
(typically from an earlier refused edit). On files of 40+ lines, an `old_string` covering most of
the file points to `write` for whole-file rewrites, on both misses and successes.

All grammars retain fresh-file checks, complete-target approvals, cancellation and atomic rollback.
Only full verbatim returned read/search lines establish edit witnesses; truncated, skipped,
lossy and declaration-summary rows do not. Generated-file protection and seen-line enforcement
are operator controls, not model arguments. Optional parse repair uses the actual coach role,
returns complete source, is reparsed and is authorized again before any write. Blackbox recording
stores bounded source in private state without promising secret redaction; opt in deliberately.
Configured syntax blocking uses the same high-confidence error-count gate for native edits,
whole-file writes and AST rewrites, including staged AST token apply. The complete AST candidate
is checked before any sibling commits. Unsupported grammars remain nonblocking, existing error
counts may not increase, and `allow_syntax_errors: true` bypasses only this parser gate, not
approvals, witness checks or transaction freshness. Syntax success is not behavioral proof.
A syntax refusal states that nothing from the edit exists in the file and names identifiers it would
have introduced. Its retry advice scales with the change: a rewrite of most of a 40+ line file
points to `write`, a change of 40+ lines suggests smaller edits, and smaller changes are asked to
fix the text and retry.
Completed edit previews never execute tools or abort partial payloads; typed no-change results are
not failures. Previews without a current source-read grant are skipped rather than leaking diagnostics
or prompting for an implicit permission. Inline recovery applies only to an entire unfenced sloppy
completion and follows ordinary approvals, hooks and journaling.

Notebook editors use a source projection with `# %% [code|markdown|raw] cell:N` markers,
not a lossy rewrite of notebook JSON. The rules for editing that projection are not in the `edit`
schema: they follow every ordinary `.ipynb` read and are appended to a failed notebook edit. Existing indexed cells retain their IDs and metadata;
changed code source clears stale execution output. Duplicate or nonexistent old indices are
rejected, while new unindexed cells receive new IDs. Dedicated cell reads accumulate only
genuinely returned Unicode ranges for the same fresh revision. Raw JSON inspection is separate
from projected-source evidence. Generic and dedicated writes share BOM/line-ending handling
and preserve arbitrary-precision JSON numbers.

## Tool reliability and measured interfaces

Dial builds execution validation from the same tool-schema snapshot sent with each model request.
The system catalog is historical configuration; current schemas are authoritative. Current-call
names, valid argument examples and single-call restrictions appear in the offered definitions,
without inserting transient messages into the conversation or rewriting its cached prefix.
Investigation checkpoints still require real todos; tools withheld by a checkpoint are explained
as temporarily gated, and invalid todo calls receive a valid argument shape.

Tolerated argument forms, each equivalent to the canonical call: an object- or array-typed
field sent as a JSON string holding that container (`"repro": "{\"name\": …}"`) is decoded before
validation, while string-typed fields are never decoded; `verify` with `targets: []` is the
omitted selection; and an absolute `glob` pattern under the workspace root is the same pattern
relative to it (patterns elsewhere are still refused).

```sh
dial reliability status
dial reliability check
dial -m mlx/mlx-community/Qwen2.5-0.5B-Instruct-4bit reliability check --json
```

The explicit check makes up to eight model requests, each with a 60-second wall-clock limit, through the selected backend: read, grep,
glob, write, todo, edit, a fixture assertion through bash, and a two-call batch. Model output is
executed only after matching the exact expected tool and arguments, inside disposable fixtures.
It never edits project sources. The edit fixture uses the configured hashline or replace contract.
A profile that explicitly disables parallel calls skips the batch case rather than scoring it as
a model failure. Transport or fixture infrastructure failures preserve the previous profile rather than counting
as model failures. Local checks wait for idle compute, yield to foreground coding and obey stop
controls; Ctrl+C interrupts calls and compute waits. No training is required. Hosted checks use
the selected service and can incur its normal inference charges.

Results are private, model-scoped `reliability.json` records. Binding includes the selected backend,
model, model profile, edit format, server helper, available local path metadata and Hugging Face
main revision pointer. Results expire after seven days. This is metadata invalidation, not a full
hash of all weights; recheck after changing an external server's model behind an unchanged ID.
These fixtures measure tool contracts and operations, not overall coding accuracy. Automatic
interface selection excludes sessions with LoRA or steering because calibration currently tests
the base model. Failed fixture cases select one-call operation; two consecutive live contract
failures also select it for recovery. Invalid multi-call batches in this mode execute nothing.
Three consecutive contract-failure rounds stop by default. Normal runtime errors retain the
existing path/patch recovery, identical-call guards and stall limits.

Use `/reliability` during a turn to inspect successful calls, schema and runtime failures,
corrected rounds, check outcomes, time to the first useful action and the selected interface.
The panel reads a cached session snapshot; it performs no inference, Git or database work on the
render thread. **Settings → Improvement → Tool reliability** controls the gate, examples, automatic
interface selection and failed-round limit. CLI status shows saved base-model results, not another
session's live observations.

For providers with documented strict tool support, explicitly declare the exact model profile:

```toml
[model_profiles.my_model.capabilities]
strict_tools = true
parallel_tool_calls = false
```

This extends an existing exact provider/model profile; do not infer support from a model's name.
Compatible object schemas receive native strict flags in Chat Completions, Responses and Anthropic
requests. Optional fields become nullable on the wire and their null placeholders are removed
before validating against the original execution schema. Required nulls, unknown properties and
invalid values remain errors. Unsupported schema compositions retain ordinary validation.
Dial-owned MLX does not use the `strict_tools` capability flag: its server applies the
schema-constrained tool-call decoder automatically for supported JSON-object tool templates (see
*Schema-constrained tool calls* in [improvement.md](improvement.md#fast-weight-candidates-without-long-training-runs)), so a strict declaration for that backend is rejected rather
than presented as a separate constraint. Runtime validation is always
active. Strict schemas constrain structure, not whether a path, edit or solution is correct.
See [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling)
and [Anthropic strict tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/strict-tool-use).

`finish completed` checks pending edits with existing focused verification before accepting the
claim, refuses unresolved failed checks, refuses blocked todos (and, on lean local routes, any open
todo step; elsewhere an accepted completion closes open steps), and refuses while background jobs it
awaits are still running. An edit whose complete diff a tool result already showed does not trigger
a review; a file changed after its last shown diff, or by a non-edit tool, gets one bounded diff in
the refusal, and the next `finish completed` is accepted once that diff has been shown. A pending
diff or visual review continues the agent normally; it is not a tool failure or an accepted completion. Visual edits require fresh rendered
pixels in a subsequent model round. If inspection is unavailable, the agent reports that limit;
the visual reminder is bounded for each revision. Operator-declared behavioral checks and existing
bounded repair remain in force. `/reliability` distinguishes missing automatic checks from passing
checks; build or syntax success is not a certificate of task correctness. Inspect the diff and
use behavioral checks that cover the requested outcome.

## Resource edits and annotations

LSP workspace edits support ordered file create, rename, delete and text operations in one validated
transaction. Directory operations and confirmation-required annotations are refused before writes;
use the explicit directory workflow when needed. Revert refuses to overwrite external changes.
Refactoring snapshots skip common dependency/build directories (`node_modules`, `target`, `.venv`,
`venv`, and `__pycache__`) and continue past unreadable subdirectories. A server edit or resource
creation in an unobserved or incompletely scanned directory is refused, rather than assuming its
pre-request state. The snapshot is bounded to 100,000 entries; targets outside the captured set
are likewise refused. Valid same-position insertions are applied in server order.

`dial annotate artifact://HASH --note TEXT --start N --end N` stores bounded human feedback against
an immutable artifact hash and source range. `--list` returns a paginated annotation record.
`read` on `artifact://H:annotations` reads these records as source data; annotations never grant
execution authority or replace the artifact.
