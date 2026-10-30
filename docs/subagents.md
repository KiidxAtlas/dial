# Subagents

The `task` tool delegates a self-contained assignment to an independent agent conversation. Use `tasks` for independent work; each task has its own prompt, tool permissions, step budget, and optional JSON Schema result contract. A task is not a continuation of the parent's conversation and sees only its assignment and permitted project context.

Use `/workers` for live status, transcripts, results, cancellation and messages; it is the canonical name. `/agents` opens the same panel when run bare, and `/agents definitions` lists role definitions. `/jobs` and `/hub` are hidden aliases of `/workers`. Empty reports, promises to act and repeated malformed replies cannot mark a child done. They share the parent's bounded recovery rules and end as `incomplete` when recovery is exhausted. This validates the report boundary, not the truth of the model's claims; completed edits still need appropriate checks. Background findings are delivered to the parent without a model status-polling call, with the full artifact available when more evidence is needed.

Delegation briefs avoid repeating automatic parent goals when the exact text already appears in the explicit assignment, scope or shared context. A duplicate working-state goal is omitted; constraints, evidence, claims, todos, distinct historical context, loaded skills and output schemas remain. Explicit task/context text is not shortened. Resumed workers retain developer-role operator instructions as well as system instructions.

For efficient handoffs, supply the outcome, owned files or subsystem, shared interfaces, relevant evidence references, and checks with an owner. Keep shared builds with one integration owner and wait for relevant edits to settle. Worker findings are evidence to use, not a reason to repeat the investigation; failed checks still need diagnosis, repair and rechecking.

Untargeted task status/wait and pool polling retain lifecycle state and report artifacts but do not
repeat a successful narrative already delivered for that exact execution. New results, failures,
blockers, acceptance/audit details and isolation/merge warnings remain visible. Targeted
`task status` with an `id` recovers the full existing preview; `artifact_read` retrieves the complete
canonical report. Polling never labels a running worker complete or counts a report as verification.

### Tool-free side questions

`side_request` lets the main agent ask a reasoning question while a command or independent task
runs. `action="start"` takes `question` and immediately returns an id; the answer uses an immutable
snapshot of the conversation at start time, with no tools. Unlike a `task` worker, it cannot inspect
new files, edit the workspace, run checks, or start nested workers. Answers are untrusted reasoning,
not verification evidence. Use a scout when the question needs new source investigation.

Completed answers are delivered to the main conversation at its next request boundary and survive
there as ordinary session history. `status` lists requests (or takes `id`); `wait` requires `id`
and waits up to `timeout` seconds (default 5, range 0–30; 0 is a nonblocking check); `cancel` requests
cooperative cancellation by id. Completion is blocked while a side question still runs or a completed
answer has not been delivered. At most two may run concurrently and eight recent results are retained
for status queries. Session exit and main-turn interruption request cancellation; unfinished side
questions are not resumed.

Each question sends the snapshot context in a separate request using the session's effective model
configuration, so it consumes model compute and may incur provider charges. Agent-initiated questions
share the parent's worker-request budget and usage ledger (reported as auxiliary work), and respect
observed session/turn cost stop boundaries. In-flight requests can still incur charges after a boundary
is reached. There is no guaranteed speedup on a model server that handles only one request at a time.
The tool is available to the main
agent under the default harness, subject to tool selection and mode restrictions, not to subagents.
It is independent of the user's `/btw` thread UI.

### Delegation preference

Dial defaults to **selective delegation**: the main agent keeps coupled planning, implementation,
regression tests and focused diagnosis in one loop, loading skills for specialized procedures.
It delegates substantial independent parallel work, useful fresh-context reviews and work that
benefits from context isolation. A long-running command alone can use a background process
without starting another model conversation.

In **Settings → Tools → Subagents**, enable **Prefer subagents** for specialist-first guidance,
including tester-owned verification when useful. The equivalent project or user configuration is:

```toml
[task]
prefer_subagents = true # default false
```

This is an advisory preference, not an automatic launcher or a permission grant. It does not
enable a withheld `task` tool, override explicit user instructions, change worker limits, or
disable completion checks. Small coupled checks may still run inline when enabled; independent
verification and parallel workers remain available when disabled. Custom prompts can replace
the built-in guidance. Existing configurations without this key use selective delegation.

### Coordinating delegated testing

When delegating testing, start `test-runner` with `action: "start"` and continue independent work
when available; do not invent unrelated work just to keep a worker busy. Give it acceptance
criteria, owned test files, interfaces, focused commands and a readiness boundary. Tests can be
authored against stable interfaces in parallel; runs wait until the inputs have settled. Do not
edit the same files concurrently.

Built-in test-writing guidance applies to the main agent (including lean/one-call prompts),
implementation workers and test-runners. Prefer compact, risk-driven suites: inspect and extend
the existing owner and fixtures; add a test only for a distinct plausible regression not already
covered. Use table-driven cases for equivalent input classes, not a separate test/file for every
use case or exhaustive permutations. Keep genuinely different contracts separate and choose the
lowest layer that proves them; integration/end-to-end tests cover risks lower layers cannot.
Tests should be deterministic and independent, with isolated state, controlled time/randomness
and resource cleanup; live services/credentials belong in explicit optional integration tiers.
Avoid assertions that pin prose, inspect source or mirror implementation, unless the exact
representation is itself a required contract. Stop when distinct risks are covered, not at a
test-count or coverage-percentage quota. Do not remove meaningful existing coverage to shrink
the suite. The versioned `examples/skills/test-runner` skill expands these design rules.

Start with the changed package/module (for example,
`cargo test --locked -p dial-agent --lib agent::workflow::research`), not overlapping whole-workspace
suites. Broaden for affected cross-cutting behavior, unresolved risk or an explicit project/user
gate. `make test-module` filters test execution but still selects every workspace library crate.
This is advisory prompt guidance, not a hard test-count limit or a measured live-model guarantee.

The tester owns check execution and returns exact commands, source state, observed results,
failure locations, actionable fixes and unverified gaps. It may repair tests within its assignment;
production repairs require an explicit, non-overlapping scope grant. Otherwise it reports the
diagnosis to the implementation owner. Implementation workers own scoped tests and focused
verification unless the parent assigns a separate tester/check owner; then they coordinate
coverage and stable interfaces instead of duplicating test edits or runs.
Coordinate reruns when inputs change; do not repeat a passing run on unchanged inputs.

Before completion, the main agent collects the report and assesses it against the final integrated
changes; a private-copy run may need a focused integration check after merge. Worker claims are
not independent verification proof. Configured controller completion checks and approval gates
remain intact. If delegation is disabled, unavailable or refused, or the user requests inline
testing, the agent explains the exception and performs minimal necessary checks without bypassing
permissions. Local-model provider limits may serialize inference even with an asynchronous task.

The preference is built-in workflow guidance, not a command interceptor or an automatic tester
launch for every shell command. `test-runner` uses the existing `tester` model-role preference
and fallbacks; choose its model in `/roles`.

## Agent definitions and permissions

Built-in roles include `scout`, `planner`, `reviewer`, `worker`, and `test-runner`. Named definitions may be configured under `[agents.<name>]` or discovered from `~/.dial/agents`, `.dial/agents`, and (when enabled) `.claude/agents`. Definitions can narrow tools and set spawn policy, model fallbacks, thinking level, output schema, and autoloaded skills. A role's write permissions come from its base type and tool allowlist; a read-only agent cannot perform operations requiring approval. Its system prompt says so before its first call, and when it is offered `bash` it also lists the inspection commands that run without approval (one plain command per call, no `cd`, `&&`, pipes or redirection; the `cwd` argument replaces `cd`). A refused call repeats that rule. Worker launches and individual effects remain subject to the operator's approval policy.

The reviewer supports both change reviews (defects introduced by the supplied change) and area audits (existing defects in the requested scope). It performs read-only contextual inspection, not commands or checks. An output schema is optional: when supplied it is followed exactly; otherwise the reviewer returns a prose review with ranked, located findings and evidence gaps. A missing diff or schema is not permission to invent one.

`spawns` limits which child roles a definition may start. `task.max_recursion_depth` bounds nested task creation (the main agent is depth zero; the default depth of two permits a child level). `task.max_concurrency` bounds active foreground and background worker branches in this process, shared by project. Batches accept up to 64 tasks and run within that ceiling. Serial descendants borrow their waiting parent’s slot, so a ceiling of one still permits nesting. Live launches also obey `subagent_max_live_jobs`. If another launch already occupies the shared capacity, a new branch reports incomplete without starting; the owner can process existing workers and retry. Descendants use synchronous `run` only. `task.provider_concurrency` limits simultaneous model requests by provider; slots apply to requests, not whole runs, so nested workers do not hold resources while waiting for children. Local model requests (Dial's own runtime, or an endpoint on a loopback, private-range or `.local` host) are serialized per server unless `task.provider_concurrency.<provider>` raises the limit for a multi-slot server.

Provider-slot waits remain cancellable. Child memory-helper requests also check their operation's cancellation, deadline, and session/branch authority while queued and again before starting transport, so an expired helper does not wait for an unrelated request to release capacity.

Output-limit recovery has its own no-progress bound, even when `max_steps = 0`. After finite output-budget increases, eight nonproductive continuation nudges stop the run as incomplete without discarding completed effects. Empty output and incomplete tool calls count as nonproductive. Partial text uses constant-space periodic-cycle detection; once a cycle is detected, further truncated output cannot reset the stall count. Distinct advancing text can continue, and usable completed output or new steering resets recovery. This does not detect every semantically nonproductive prose stream.

## Seance consultations

`/seance <source> <question>` and the `seance` agent (`task` with `agent: "seance"`, `source_session`, `task`, optional `model`) ask a read-only consultant about a recorded session or one of its worker transcripts. `source` is a session id prefix or a worker id from the calling session. The consultant reads the source journal and every nested worker transcript as quoted data (delimited and sanitized, bounded to a fixed byte budget that keeps the start and latest lines), and its tools are only `read`, `glob`, and `grep`. It cannot spawn workers, use extensions or artifacts, or write the source. Its own transcript is kept under the calling session like any other worker.

The consultant runs on the explicit `model` when given; otherwise on the source session's saved model. If that model is not available, the consultation fails with an error naming it and asking for `model`; it never substitutes another model silently. Missing persistent storage or an unknown or ambiguous `source` is also an explicit error. Both surfaces reject a `source_session` on other agents.

Seance is not available from `solve`. Solve's candidate children always run as `worker` agents without a calling session, so a `seance` job cannot be attached there; the consultant requires the calling session to locate worker transcripts and reports `seance needs the calling session to find its worker transcripts` if one is ever attached without it. Use `/seance` or `task` with `agent: "seance"` from a session instead.

## Isolation and results

A worker may request `isolation: "copy"` to work in a private project copy. A successful isolated run merges its edits; failed or interrupted work is retained as a recovery copy. Completed computation is not completed integration: parent conflicts make the task `incomplete`, and merge errors make it `failed`. Nonconflicting files may already have merged. The candidate structured result remains intact, with conflicts, applied paths, errors and recovery location reported separately in isolation metadata. Always inspect that metadata before relying on a worker's edits.

### Isolation backends

Set the backend with `[isolation] backend`, or in Settings under Subagents, "Isolation backend":

```toml
[isolation]
backend = "auto" # auto | clone | overlay | worktree | projfs | copy
```

- `auto` (default) tries the platform's preferred backend first and uses the first one that works here: Linux `overlay`, then `worktree`, then `copy`; macOS `clone`, then `worktree`, then `copy`; Windows `projfs`, then `worktree`, then `copy`. Each skipped candidate is listed in the status line with a one-line reason, for example `copy; clone skipped (...)`, and probe results are cached. On Linux, `overlay` hides the secret and private entries at the top of the project (`.env*`, keys, credentials, `.git`, `.dial`, `.codegraph`). It skips itself with the reason `cannot hide excluded files` when it cannot create the hiding whiteouts, and with `too many entries to scan for secrets` when the project has more entries than `isolation_source_files` allows.
- `clone` is a copy-on-write clone of the project, selected by `auto` on macOS. Each file is cloned with `clonefile(2)` on macOS (APFS). The `clone` backend itself is unavailable on Linux, but the private dependency copies, `worktree` file copies and benchmark/solve workspace copies there use the `FICLONE` ioctl per file (btrfs, XFS with reflinks, bcachefs). A clone shares data blocks with the project until either side is written, so setup costs metadata only and writes in the copy never reach the project. Cloning is attempted per file and any failure (unsupported filesystem, different devices, permissions) falls back to an ordinary byte copy of that file with the same contents, mode, exclusions and size limits. The fallback is automatic and not reported in the status line; `dial_core::platform::fs::copy_counts()` returns the process-wide numbers of cloned and byte-copied files for debugging.
- `copy` is a plain file copy on any platform. Secrets and private entries (`.env*`, keys, credentials, `.git`, `.dial`) and build outputs and caches are not copied: `target/`, `build/`, `dist/`, `node_modules/`, `vendor/`, `.venv/`, `venv/`, `.cache/`, `.tox/`, `__pycache__/`, and at the project root only `.cargo-work/` and `.tmp/`. Root `bin/` is excluded only when the project's `.gitignore` ignores it; tracked files and ordinary bin scripts are preserved. This excludes Dial's `make update` binary and backups without dropping other projects' source scripts. A source file over the fixed 64 MiB per-file limit, or a copy over `isolation_source_bytes`, fails setup and names the file and the limit; move generated output into an excluded directory instead of raising the limit.
- `worktree` is a detached `git worktree` at `HEAD`, with uncommitted and untracked files, and ignored files that are not git-excluded or build outputs, copied in. Git-excluded paths such as `.env` are never copied. Symlinks are not copied. Ignored files count against `isolation_dependency_bytes`; if the budget is exceeded, setup fails and names the limit. The project must be the top of its git repository and have at least one commit.
- `overlay` (Linux only) mounts an overlayfs view with the project as its lower layer, using the kernel overlayfs or `fuse-overlayfs` when available. Before the mount, each secret or private entry at the top of the project is hidden with a whiteout in the upper directory, so the worker cannot see it. Dependency directories (`target`, `node_modules`, `.venv`, `venv`, `vendor`, `build`, `dist`, caches) are not scanned, so a secret file inside one of them, such as `node_modules/.env`, stays visible through the lower layer. Dependency directories stay visible in full, and writes to them stay in the upper directory and are never merged back, the same as the copy backends' change sets. The scan counts the entries it reads against `isolation_source_files`; a project over that limit makes `auto` skip overlay, and a named `overlay` fails with the same reason. Only the upper directory is merged. The mount is unmounted when the worker ends. If Dial is killed mid-run, the mount can remain; unmount its `merged` path under the isolation temp directory with `fusermount3 -u` (or `fusermount -u`, or `umount`).
- `projfs` (Windows, opt-in) mounts a Windows Projected File System view of the project. It needs the optional Windows feature `Client-ProjFS` (`Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS`); without it the named backend reports that reason. Sources are never written. Git initialization is skipped because it would hydrate every file, and dependency copies are not made. This backend is compile-checked for Windows only and has not been runtime-verified.

A named backend that is unavailable is an error that names the reason; Dial never falls back from a named backend silently.

### Choosing the backend per task

A single worker task, or any `tasks[]` item, may set `isolation_backend` to override `[isolation] backend` for that task only. It accepts the same names (`auto`, `clone`, `overlay`, `worktree`, `copy`, `projfs`) and implies `isolation: "copy"`, so batch siblings can run one worker in a `clone` and another in a `worktree`:

```json
{"tasks": [
  {"agent": "worker", "task": "...", "solution_space": "...", "isolation_backend": "clone"},
  {"agent": "worker", "task": "...", "solution_space": "...", "isolation_backend": "worktree"}
]}
```

Without `isolation_backend`, `isolation: "copy"` uses the configured backend exactly as before. Read-only agents reject both fields. Each named backend is checked before any worker in the call starts. For `start`, `create` and `push` the check runs before anything is queued. A backend that cannot run here, such as `worktree` outside the top of a git repository or `clone` off macOS, fails the whole call with the task's label and the reason, for example ``wt: isolation backend `worktree` is unavailable: ...; no worker was started``. Dial never falls back to another backend or to the live checkout. The chosen backend is recorded in a started worker's assignment and reported in the isolation metadata (`backend`) and the status line.

Copy and clone materialize in-root source symlinks. Directory cycle detection is path-local: both
a real directory and its aliases retain their contents, regardless of enumeration order. Cycles
stop at an empty directory; targets outside the project are refused, and excluded targets remain
excluded. Repeated directory aliases consume extra space; source preflight limits do not account
for this expansion. Generic dependency names such as `build` and `dist` are still excluded at any
depth, even when tracked; this audit does not change that exclusion policy.

If removing a worker's temporary worktree metadata or mount fails, the merge result reports a `cleanup` error with the path; it is not hidden. Deletions recorded by fuse-overlayfs (`.wh.<name>` files and `.wh..wh..opq` opaque markers) are understood as deletions when merging back, the same as kernel overlay whiteouts.

The status line names the backend in use, for example `isolation backend: worktree`.

Set `output_schema` to request a structured result. The child is asked to return a JSON value and validates it before completion. Invalid or malformed output is returned to the child for a bounded correction attempt; the task fails if the configured retry budget is exhausted. JSON Schema validation supports local `$ref`, applicators such as `allOf`/`anyOf`/`oneOf`, and common structural, string, numeric, object, and array constraints.

Synchronous task results retain the complete typed job report before creating text previews when artifact storage is available. Long unstructured results are marked as previews and include an `artifact://` reference retrievable with `artifact_read`; the artifact contains the complete `jobs` report, not just the displayed excerpt. If storage is unavailable or retention fails, both the text and typed result remain unabridged at the task boundary; retention failures include a diagnostic. Normal controller context limits still apply. Structured results remain intact.

## Live tasks

Use `action: "start"` to start a live task, then `status`, `wait`, or `cancel` with its id. Live workers and the parent talk through the `message` tool, offered whenever `task` is (it reports an empty roster or an unknown recipient when no worker is live): `to` takes a live job id or label, `parent` (from a worker), or `all` (a broadcast to the other live workers); omit `to` to list reachable recipients. Each message is at most 32 KiB. A recipient's mailbox holds at most `[task].inbox_limit` undelivered messages (default 256); a full mailbox refuses the message explicitly, and a broadcast reports each refusal. Messages are plain text with no authority: they never approve actions or widen a worker's tools. A recipient receives a message, labelled with its sender, at its next model request. The `message` result is a receipt per recipient: `{"receipts":[{"to":"task-1","status":"queued"}]}`. `queued` means the recipient's mailbox accepted the message and it will be read at the recipient's next model step (the sender cannot know that synchronously, so `delivered` is never claimed); `refused` carries a `reason` (only a broadcast reports a full mailbox this way, since a single full mailbox fails the send). A message to a finished or unknown worker fails the call with that reason; Dial never revives a finished worker, and messages still unread when a worker finishes are discarded. A worker's message to `parent` follows the same policy as worker completion notices and background-job completions: it never starts an unrequested turn. While the parent is idle (for example waiting for your input in the TUI), the controller's regular pump (about every two seconds when nothing else is animating) shows `Message from <worker> to parent: <first line>` in the transcript and queues the full text, marked as peer text rather than instructions, for the parent's next request; while a turn runs it surfaces at the next model step. `task_message` remains the RPC form of the same send. Completed task records and transcripts may be retained in the state directory according to `[task].persist`; do not put secrets in task prompts or messages.

Worker messages to `parent` are not held back by the parent's own waiting. While workers are running, the idle terminal pumps at least every quarter second, so the message notice appears promptly. A parent `task` `wait` stops early when a worker message arrives: nothing is cancelled, the workers keep running, the result says why the wait ended, and the message arrives with the parent's next request.

Worker outcome payloads carry the shared `runtime::Phase` rather than a separate status enum. Incompletion stays incomplete in runtime nodes, cards and protocol events. Task-tool results preserve legacy `done`/`cancelled` spellings for compatibility; new execution nodes use `completed`/`interrupted`. Archived child tool results persist tool-owned presentation and the complete redacted evidence separately from model-facing retrieval pointers.

Busy-input routing acknowledges independent work only after confirming worker startup. If startup fails before a worker launches, explicitly independent input is deferred until after the active turn; automatically split input falls back to steering. A worker that actually started is never replayed through that fallback.

Automatic busy-input decisions run at the next safe model-request/tool boundary, not concurrently
with an unfinished foreground inference. They use the `judge` role when assigned (including typed
Laya decisions), otherwise `small` or the current model. Classification has a three-second total
budget (or the shorter worker timeout), including retries. A busy provider slot, timeout, malformed
answer or unavailable decision service falls back to steering with the exact user message; it does
not interrupt the turn or silently lose the request. The UI and existing worker approvals remain
responsive while classifying. Independent launches still use the ordinary worker admission,
isolation and approval rules.

`[task].persist = false` skips durable child conversation capture for newly started workers throughout their lifetime, including while live and after interruption or failure, and disables resume. Runtime status, progress and results remain in the current session's worker UI; the archived transcript view is unavailable for these workers. This does not delete historical transcripts or isolation recovery copies, and does not disable ordinary result artifacts or the parent session's own history. With `persist = true`, available session storage retains redacted transcripts for live viewing and continuation.

## Managed worker pools

`task` also accepts `create`, `push`, `peek` and `close`. `create` returns a pool ID and accepts
`concurrency` from 1 to 32 plus initial `tasks`; `push` appends more tasks to that pool.
Use the pool ID with `wait` or `status`, or use `peek` for its full item history. Ordinary provider,
worker-budget, isolation and approval gates still apply. `close` cancels queued items before they
start and interrupts running workers; a closed pool refuses new items.

Each logical slot retains its backend and completed conversation for later items when their effective
configuration matches. A different configuration invalidates that carry rather than silently keeping
the old role or model. Automatic batch handoffs do not repeat earlier delivered items; explicit
inspection retains the full history. Pools and queued items are in-memory controllers, not durable
queues that restart work after a crash. Lost or uncertain executions are never automatically replayed.


## Continuing a retained worker

Use `{"action":"resume","id":"task-1","message":"Follow up on the finding"}` in the same parent session. Dial restores the redacted conversation and the actual offered tool set, pins the recorded model and agent definition, and applies the current owner’s approvals to future actions. A process lease prevents two owners from resuming the same worker. Call intent and results are flushed to durable storage before proceeding.

Resume refuses unknown tool effects, truncated or oversized history, changed workspace identity, model/definition changes, and histories that used a fallback model. It never replays saved calls. Released isolated workers remain transcript-only; start a new isolated task using their retained findings or recovery copy. A resumed worker does not regain private temporary notebooks, checkpoints, running processes, or native tool sessions; those capabilities start fresh.

`dial --restore` resumes the parent session before reattaching eligible interrupted workers.
`dial -r <id> --restore` chooses a parent explicitly; otherwise it continues the latest conversation.
The main prompt is not submitted again. The newest saved instance of each worker decides eligibility:
completed, failed and explicitly cancelled workers stay stopped. Each accepted restore still passes
the ordinary history, effect-certainty, workspace, model/definition and lease checks above; setup
failures are reported as rejections, not successful starts. Worker status is correlated with the new
runtime node so an older interrupted instance cannot replace its live controls.

## Development copies

By default, isolated work includes private copies of `node_modules`, `.venv`, `venv`, and `vendor`, including nested project dependencies. Supported filesystems use copy-on-write cloning; other filesystems use independent copies. Hardlinks to parent files are never used. Internal dependency symlinks point into the copy, and virtual-environment text launchers are relocated to its paths. Dependency edits do not merge back. Secret files and external dependency-directory links are refused or omitted; virtual-environment interpreter files are copied rather than retaining external writable links.

For Git projects the copy gets an independent filtered-source baseline, so `git status` and `git diff` work. Parent history, branches, remotes, hooks, and credentials are not copied. Git metadata does not merge back.

```toml
[task]
max_concurrency = 32
max_recursion_depth = 2
isolation_dependencies = true
isolation_git = true
isolation_dependency_bytes = 4294967296 # 4 GiB
isolation_source_bytes = 536870912     # 512 MiB total
isolation_source_files = 100000
```

Dependency traversal is additionally capped at 500,000 entries and 128 nested levels; individual source files retain the 64 MiB cap. Exceeding a limit fails the copy explicitly. Set `isolation_dependencies = false` for source-only work, or increase the relevant budget for a larger project.

## Scheduling and measurements

Safe extension calls with `Concurrency::Shared` can execute together; exclusive calls and operations requiring approval remain barriers. Each scheduled call still runs its pre/post hooks. A hook that changes scheduled arguments causes refusal and a request for resubmission through the normal controller. Hook-bearing batches do not deduplicate calls. Queued steering interrupts declared pure waits and preserves completed effects; shell work may transfer to the existing background-process manager.

Shared parent and worker batches publish each tool's result and call-ID-correlated timing when it
finishes, without waiting behind a slower sibling. The canonical provider conversation still settles
the call group in its original order.

`[compaction].preserve_prefix = true` keeps warm-history pruning from rewriting old messages solely to save a small number of tokens. Hard context limits and necessary compaction still apply. Parent request records include `request_prefix` diagnostics after hooks, with stable message bytes and explicit model, system, tool, or history invalidation reasons. These are provider-neutral measurements, not proof of a provider cache hit.

Runtime telemetry records `ok`, `error`, `blocked`, `skipped`, `timeout`, and `aborted` separately. Tool journals and OTLP spans also distinguish `none`, `known`, and `unknown` effects. An uncertain result ends the worker as incomplete and prevents automatic continuation or resume. Stats reports include `tool_outcomes`; trace rows and OTLP tool events carry status. Known controller refusals, shell timeouts and interruption paths supply explicit statuses; ordinary unannotated tool failures remain `error`.

## Model roles

Subagents use the effective project and global model-role assignments. Scouts prefer `small`, reviewers and test runners prefer `tester`, planners prefer `planner`, and implementation workers prefer `task`. TOML custom agents inherit the role of their declared `base`. File definitions use `small` for read-only agents and `task` for writers unless a model selector names another role. Both foreground and background launches load file definitions.

When the preferred role is unassigned, delegation chooses the first configured role in these chains, then the conversation model (`main`): worker `task → coach → small → main`; scout `small → task → main`; reviewer/test-runner `tester → coach → task → small → main`; planner `planner → coach → task → small → main`. These are delegation-only defaults, not provider-error retries or general chat-role fallback rules. An explicit model selector overrides these defaults; an architecture-only setting does not change ordinary scout routing.

A custom definition can override its model with `model: ["@tester"]`, a qualified `provider/model`, or a model name on its default role's provider. An explicit task `model_profile` overrides the definition's model list. `thinking_level` (or a model's `:level` suffix) reaches the selected model's request controls; the explicit definition setting takes precedence over a suffix. `off` and `none` disable thinking. Enabled efforts require exact-model support declared by the selected profile; unsupported controls fail before workers start.

A definition's ordered model list provides bounded startup fallbacks after a recoverable provider failure. Dial never restarts a child that has already executed tools to try another model. Existing work and failure evidence stay with the owner.

`/workers` shows each child's role, selected model, assignment source and activity. It includes inline workers as well as background jobs. Press `r` to open model-role assignments; `/roles` and the model-role shortcut open the same view. A running worker keeps the model captured at launch when role settings change. Background jobs retain their cancellation and message controls; inline workers follow the current turn's interruption control.

To change one live worker, open its transcript and use `/model <selector>` or `/thinking <level>`
(`/effort` is an alias). Controls are local to that worker: they do not change the main session,
saved defaults or sibling workers. A selection is validated against the exact model/profile and
queued for the next request boundary; the banner distinguishes queued, applied and failed changes.
Unsupported effort settings are refused. A failed backend switch retains the prior active backend.

## Delegation and model roles

The `task` tool runs typed subagents (`scout`, `reviewer`, `planner`, `worker`, `test-runner`, or a configured agent) in parallel batches. Each batch takes one shared `context` and per-item `task`, `solution_space` and optional `skills`. Both synchronous `run` and fresh live `start` receive the same bounded parent goal, open todos and working state, alongside the unchanged explicit context and assignment, role prompt and preloaded skill bodies. Resume restores the saved conversation rather than replacing its brief with current parent state. Built-in roles cannot spawn children by default; configured definitions may permit synchronous descendants within their spawn policy and depth limit. Model-role preferences and fallback chains are listed above; results report each child's selected role and model.

`task` and `skill` are offered in ordinary sessions. A user message that shares words with a skill's name or description gets a one-line hint to load that skill.

Open the worker hub with Alt+A or F6 (`app.subagents.hub`), Left twice on an empty composer, or `/workers` (`/agents` opens it too; `/jobs` and `/hub` are hidden aliases). `/bg` and `/processes` open the background process panel instead. The `subagent-hub:activity` action opens the hub directly on its Activity tab. Each child's conversation is saved as JSONL under the state directory (`subagents/<session>/<job>-<millis>.jsonl`), outside the workspace.

Agents tab: Up/Down or `j`/`k` select a worker; Enter opens its transcript at the selected entry; `t` toggles flat and tree views; `/` filters by name, task or id; `m` messages a worker (steers a running one, resumes a finished one); `x` pressed twice within 2 seconds cancels a running worker; `r` opens model roles; PageUp/PageDown scroll the inspector. Existing roster positions stay stable while the hub is open; workers waiting for input or reporting failure can rise for attention rather than continuously reordering by recent activity.

The focused transcript has a full, independent editor. Escape returns to the hub and preserves its
draft, cursor and undo state when reopened; `/back` exits the view. Ctrl+C stops only the selected
worker. Ordinary letters, including `x`, are text there, not hub cancellation shortcuts. A submitted
message clears only after delivery/start acknowledgement; a failed submission keeps the draft.

Activity tab (`2`, or Tab on a wide layout): `f` cycles the kind filter (all, errors, responses, tools, messages); `s` cycles the scope (all, selected worker, selected subtree); Space toggles follow; `/` searches; Enter opens the entry in its transcript.

Below 96 columns the hub shows only the roster; Tab opens the full-width inspector. `1`/`2` or ←/→ switch tabs. Esc clears a search, then leaves the inspector, then closes the hub; the footer's Esc hint names whichever comes next.

Mouse works while the hub is open, because Dial enables mouse capture for overlays: a click selects a worker card or activity row, a second click on the same row within 500 ms opens it, and the wheel scrolls the inspector when the pointer is over it. Long tasks, tool calls and activity text wrap to the pane width; the roster cards and title rows stay single-line.

Limits: `unread` counts inbound mailbox messages in the recent window; it is a log, not a delivery queue. Resuming from the hub launches the worker directly through the same path as `task` with `action: "resume"`; it needs task persistence and a session layout, and the reason is shown otherwise. If a turn is still running, the resume waits until that turn finishes. A running inline worker can be cancelled only where the runtime can stop it; the hub reports the reason otherwise.
