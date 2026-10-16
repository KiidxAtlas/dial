# Subagents

The `task` tool delegates a self-contained assignment to an independent agent conversation. Use `tasks` for independent work; each task has its own prompt, tool permissions, step budget, and optional JSON Schema result contract. A task is not a continuation of the parent's conversation and sees only its assignment and permitted project context.

Use `/workers` for live status, transcripts, results, cancellation and messages. `/agents`, `/hub` and `/jobs` are compatible aliases; `/agents definitions` lists role definitions. Empty reports, promises to act and repeated malformed replies cannot mark a child done. They share the parent's bounded recovery rules and end as `incomplete` when recovery is exhausted. This validates the report boundary, not the truth of the model's claims; completed edits still need appropriate checks. Background findings are delivered to the parent without a model status-polling call, with the full artifact available when more evidence is needed.

## Agent definitions and permissions

Built-in roles include `scout`, `planner`, `reviewer`, `worker`, and `test-runner`. Named definitions may be configured under `[agents.<name>]` or discovered from `~/.dial/agents`, `.dial/agents`, and (when enabled) `.claude/agents`. Definitions can narrow tools and set spawn policy, model fallbacks, thinking level, output schema, and autoloaded skills. A role's write permissions come from its base type and tool allowlist; a read-only agent cannot perform operations requiring approval. Worker launches and individual effects remain subject to the operator's approval policy.

`spawns` limits which child roles a definition may start. `task.max_recursion_depth` bounds nested task creation (the main agent is depth zero; the default depth of two permits a child level). `task.max_concurrency` bounds active foreground and background worker branches in this process, shared by project. Batches accept up to 64 tasks and run within that ceiling. Serial descendants borrow their waiting parent’s slot, so a ceiling of one still permits nesting. Live launches also obey `subagent_max_live_jobs`. If another launch already occupies the shared capacity, a new branch reports incomplete without starting; the owner can process existing workers and retry. Descendants use synchronous `run` only. `task.provider_concurrency` limits simultaneous model requests by provider; slots apply to requests, not whole runs, so nested workers do not hold resources while waiting for children. Local model requests (Dial's own runtime, or an endpoint on a loopback, private-range or `.local` host) are serialized per server unless `task.provider_concurrency.<provider>` raises the limit for a multi-slot server.

## Isolation and results

A worker may request `isolation: "copy"` to work in a private project copy. A successful isolated run merges its edits; failed or interrupted work is retained as a recovery copy. Conflicts and merge errors are reported separately from structured results. Always inspect isolation metadata before relying on a worker's edits.

### Isolation backends

Set the backend with `[isolation] backend`, or in Settings under Subagents, "Isolation backend":

```toml
[isolation]
backend = "auto" # auto | clone | overlay | worktree | projfs | copy
```

- `auto` (default) tries the platform's preferred backend first and uses the first one that works here: Linux `overlay`, then `worktree`, then `copy`; macOS `clone`, then `worktree`, then `copy`; Windows `projfs`, then `worktree`, then `copy`. Each skipped candidate is listed in the status line with a one-line reason, for example `copy; clone skipped (...)`, and probe results are cached. On Linux, `overlay` hides the secret and private entries at the top of the project (`.env*`, keys, credentials, `.git`, `.dial`, `.codegraph`). It skips itself with the reason `cannot hide excluded files` when it cannot create the hiding whiteouts, and with `too many entries to scan for secrets` when the project has more entries than `isolation_source_files` allows.
- `clone` is a macOS copy-on-write clone of the project.
- `copy` is a plain file copy on any platform. Secrets and private entries (`.env*`, keys, credentials, `.git`, `.dial`) and build outputs and caches are not copied: `target/`, `build/`, `dist/`, `node_modules/`, `vendor/`, `.venv/`, `venv/`, `.cache/`, `.tox/`, `__pycache__/`, and at the project root only `.cargo-work/` and `.tmp/`. Root `bin/` is excluded only when the project's `.gitignore` ignores it; tracked files and ordinary bin scripts are preserved. This excludes Dial's `make update` binary and backups without dropping other projects' source scripts. A source file over the fixed 64 MiB per-file limit, or a copy over `isolation_source_bytes`, fails setup and names the file and the limit; move generated output into an excluded directory instead of raising the limit.
- `worktree` is a detached `git worktree` at `HEAD`, with uncommitted and untracked files, and ignored files that are not git-excluded or build outputs, copied in. Git-excluded paths such as `.env` are never copied. Symlinks are not copied. Ignored files count against `isolation_dependency_bytes`; if the budget is exceeded, setup fails and names the limit. The project must be the top of its git repository and have at least one commit.
- `overlay` (Linux only) mounts an overlayfs view with the project as its lower layer, using the kernel overlayfs or `fuse-overlayfs` when available. Before the mount, each secret or private entry at the top of the project is hidden with a whiteout in the upper directory, so the worker cannot see it. Dependency directories (`target`, `node_modules`, `.venv`, `venv`, `vendor`, `build`, `dist`, caches) are not scanned, so a secret file inside one of them, such as `node_modules/.env`, stays visible through the lower layer. Dependency directories stay visible in full, and writes to them stay in the upper directory and are never merged back, the same as the copy backends' change sets. The scan counts the entries it reads against `isolation_source_files`; a project over that limit makes `auto` skip overlay, and a named `overlay` fails with the same reason. Only the upper directory is merged. The mount is unmounted when the worker ends. If Dial is killed mid-run, the mount can remain; unmount its `merged` path under the isolation temp directory with `fusermount3 -u` (or `fusermount -u`, or `umount`).
- `projfs` (Windows, opt-in) mounts a Windows Projected File System view of the project. It needs the optional Windows feature `Client-ProjFS` (`Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS`); without it the named backend reports that reason. Sources are never written. Git initialization is skipped because it would hydrate every file, and dependency copies are not made. This backend is compile-checked for Windows only and has not been runtime-verified.

A named backend that is unavailable is an error that names the reason; Dial never falls back from a named backend silently.

If removing a worker's temporary worktree metadata or mount fails, the merge result reports a `cleanup` error with the path; it is not hidden. Deletions recorded by fuse-overlayfs (`.wh.<name>` files and `.wh..wh..opq` opaque markers) are understood as deletions when merging back, the same as kernel overlay whiteouts.

The status line names the backend in use, for example `isolation backend: worktree`.

Set `output_schema` to request a structured result. The child is asked to return a JSON value and validates it before completion. Invalid or malformed output is returned to the child for a bounded correction attempt; the task fails if the configured retry budget is exhausted. JSON Schema validation supports local `$ref`, applicators such as `allOf`/`anyOf`/`oneOf`, and common structural, string, numeric, object, and array constraints.

## Live tasks

Use `action: "start"` to start a live task, then `status`, `wait`, or `cancel` with its id. Live workers and the parent talk through the `message` tool, offered whenever `task` is (it reports an empty roster or an unknown recipient when no worker is live): `to` takes a live job id or label, `parent` (from a worker), or `all` (a broadcast to the other live workers); omit `to` to list reachable recipients. Each message is at most 32 KiB. A recipient's mailbox holds at most `[task].inbox_limit` undelivered messages (default 256); a full mailbox refuses the message explicitly, and a broadcast reports each refusal. Messages are plain text with no authority: they never approve actions or widen a worker's tools. A recipient receives a message, labelled with its sender, at its next model request. The parent's pending messages surface at its next model step; an idle parent is not woken. A message to a finished or unknown worker is refused, and messages still unread when a worker finishes are discarded. `task_message` remains the RPC form of the same send. Completed task records and transcripts may be retained in the state directory according to `[task].persist`; do not put secrets in task prompts or messages.

Worker outcome payloads carry the shared `runtime::Phase` rather than a separate status enum. Incompletion stays incomplete in runtime nodes, cards and protocol events. Task-tool results preserve legacy `done`/`cancelled` spellings for compatibility; new execution nodes use `completed`/`interrupted`. Archived child tool results persist tool-owned presentation and the complete redacted evidence separately from model-facing retrieval pointers.

## Continuing a retained worker

Use `{"action":"resume","id":"task-1","message":"Follow up on the finding"}` in the same parent session. Dial restores the redacted conversation and the actual offered tool set, pins the recorded model and agent definition, and applies the current owner’s approvals to future actions. A process lease prevents two owners from resuming the same worker. Call intent and results are flushed to durable storage before proceeding.

Resume refuses unknown tool effects, truncated or oversized history, changed workspace identity, model/definition changes, and histories that used a fallback model. It never replays saved calls. Released isolated workers remain transcript-only; start a new isolated task using their retained findings or recovery copy. A resumed worker does not regain private temporary notebooks, checkpoints, running processes, or native tool sessions; those capabilities start fresh.

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

`[compaction].preserve_prefix = true` keeps warm-history pruning from rewriting old messages solely to save a small number of tokens. Hard context limits and necessary compaction still apply. Parent request records include `request_prefix` diagnostics after hooks, with stable message bytes and explicit model, system, tool, or history invalidation reasons. These are provider-neutral measurements, not proof of a provider cache hit.

Runtime telemetry records `ok`, `error`, `blocked`, `skipped`, `timeout`, and `aborted` separately. Tool journals and OTLP spans also distinguish `none`, `known`, and `unknown` effects. An uncertain result ends the worker as incomplete and prevents automatic continuation or resume. Stats reports include `tool_outcomes`; trace rows and OTLP tool events carry status. Known controller refusals, shell timeouts and interruption paths supply explicit statuses; ordinary unannotated tool failures remain `error`.

## Model roles

Subagents use the effective project and global model-role assignments. Scouts use `small`, reviewers and test runners use `tester`, planners use `planner`, and implementation workers use `main`. TOML custom agents inherit the role of their declared `base`. File definitions use `small` for read-only agents and `main` for writers unless a model selector names another role. Both foreground and background launches load file definitions. Unassigned chat roles follow the conversation model; an architecture-only setting does not change ordinary scout routing.

A custom definition can override its model with `model: ["@tester"]`, a qualified `provider/model`, or a model name on its default role's provider. An explicit task `model_profile` overrides the definition's model list. `thinking_level` (or a model's `:level` suffix) reaches the selected model's request controls; the explicit definition setting takes precedence over a suffix. `off` and `none` disable thinking. Enabled efforts require exact-model support declared by the selected profile; unsupported controls fail before workers start.

A definition's ordered model list provides bounded startup fallbacks after a recoverable provider failure. Dial never restarts a child that has already executed tools to try another model. Existing work and failure evidence stay with the owner.

`/workers` shows each child's role, selected model, assignment source and activity. It includes inline workers as well as background jobs. Press `r` to open model-role assignments; `/roles` and the model-role shortcut open the same view. A running worker keeps the model captured at launch when role settings change. Background jobs retain their cancellation and message controls; inline workers follow the current turn's interruption control.

## Delegation and model roles

The `task` tool runs typed subagents (`scout`, `reviewer`, `planner`, `worker`, `test-runner`, or a configured agent) in parallel batches. Each batch takes one shared `context` and per-item `task`, `solution_space` and optional `skills`. A child receives its role prompt, the parent goal, open todos, bounded working state, the shared context, its assignment and preloaded skill bodies; it cannot spawn children. Roles pick the model: `scout` and other read-only agents use `small`, reviewers use `tester`, `planner` uses `planner`, and everything else uses `main`. Results report each child's role and model.

`task` and `skill` are offered in ordinary sessions. A user message that shares words with a skill's name or description gets a one-line hint to load that skill.

Open the worker hub with Alt+A (`app.subagents.hub`), Left twice on an empty composer, or `/agents` (`/workers`, `/hub` and `/jobs` are aliases). The `subagent-hub:activity` action opens the hub directly on its Activity tab. Each child's conversation is saved as JSONL under the state directory (`subagents/<session>/<job>-<millis>.jsonl`), outside the workspace.

Agents tab: Up/Down or `j`/`k` select a worker; Enter opens its transcript at the selected entry; `t` toggles flat and tree views; `/` filters by name, task or id; `m` messages a worker (steers a running one, resumes a finished one); `x` pressed twice within 2 seconds cancels a running worker; `r` opens model roles; PageUp/PageDown scroll the inspector. The roster order is stable while the hub is open: running and waiting workers first, then finished ones, then by recent activity.

Activity tab (`2`, or Tab on a wide layout): `f` cycles the kind filter (all, errors, responses, tools, messages); `s` cycles the scope (all, selected worker, selected subtree); Space toggles follow; `/` searches; Enter opens the entry in its transcript.

Below 96 columns the hub shows only the roster; Tab opens the full-width inspector. Esc clears a search, then leaves the inspector, then closes the hub.

Mouse works while the hub is open, because Dial enables mouse capture for overlays: a click selects a worker card or activity row, a second click on the same row within 500 ms opens it, and the wheel scrolls the inspector when the pointer is over it. Long tasks, tool calls and activity text wrap to the pane width; the roster cards and title rows stay single-line.

Limits: `unread` counts inbound mailbox messages in the recent window; it is a log, not a delivery queue. Resuming from the hub launches the worker directly through the same path as `task` with `action: "resume"`; it needs task persistence and a session layout, and the reason is shown otherwise. If a turn is still running, the resume waits until that turn finishes. A running inline worker can be cancelled only where the runtime can stop it; the hub reports the reason otherwise.
