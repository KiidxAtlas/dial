# Tools, safety and context

The built-in tool roster, specialist integrations, safety model, context sources, editing grammars and tool-reliability measurement.

## Tool roster, safety and context sources

**Tools** (file edits confined to the project; shell access follows the chosen sandbox)
- The lean default is `read`, `write`, `edit`, `glob`, `grep`, `bash`, `todo`, `tool_search`, `tool_load`, `artifact_read` and `working_state`, plus `check` when `[verification].enabled` is on. Specialists are discoverable by name/capability rather than sending every schema on every request. `tool_load` takes `names`; newly enabled schemas are available in the **next** model round, and enabled selections survive session resume. Explicit profile allowlists remain authoritative.
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
  evidence references, changed paths and integration risks) apart from narrative previews. Missing
  acceptance claims remain `not_verified`; completed execution is never acceptance proof.
  Complete reports are retained when artifact storage is available; synchronous plain report text
  and custom output-schema JSON stay unchanged, with a separate model-visible metadata note. Child working state is
  packed as whole JSON fields, prioritizing constraints and current work rather than prefix-clipping.
  Oversized critical request/state snapshots explicitly require clarification from the parent;
  an incomplete handoff never claims to contain the full contract.
- `read` takes line selectors in every edit mode: `path:50-200`, `path:50+150`, `path:-60` (last 60 lines), `path:5-16,960-973`, or `offset`/`limit`; the footer identifies the returned range. `path:outline` lists a source file's syntax declarations (functions, types, impls/classes, methods, nested) with 1-based line ranges, using the same tree-sitter declarations as `find`; outline rows are summaries, not edit witnesses, so read a line range before editing, and unsupported file types return an error suggesting a line range. Pass `paths` (up to 16 targets, each with its own selector) to read several files or ranges in one call. `grep` supports Rust regexes, literal `fixed_string`, `ignore_case`, `glob`, context rows and bounded `max_results`. Its `output` selects the page shape: `lines` (default) shows matching rows; `files` lists each matching file with its match count in the same sorted order, with `max_results`/`skip`/`cursor` counting files; `count` reports total matches and files. Summary pages use the same scopes, ignore rules, exclusions, read limits and coverage/continuation footer as `lines` and record no edit evidence; a capped `lines` page suggests `output="files"`. When a workspace path does not exist, `read`, `edit`, `grep` and `glob` keep their error text and add a `Did you mean:` line with up to five existing paths (case or typo matches, the same name elsewhere, close names in the nearest existing directory); the search reuses the ignore-aware walker, never names private, secret, ignored or dependency paths, and is limited to 20,000 files and 300 ms.
- Reading a directory lists directories first, then files, by name. `.git`, build/dependency directories, the scratch directory, gitignored directories and nested linked git worktrees are named but not expanded. `glob`, `grep`, AST search and the codebase index skip nested linked worktrees (a `.git` file pointing into another repository's `worktrees/`); submodules are still searched. When a search walk covered its whole scope, its footer is one short line (`walk complete; …`, `next_cursor: null`); full counters, omitted paths and the continuation order appear only when something limited the walk. When `grep` stops a page at a cap (per-file matches, files with matches, the match limit or the 48 KiB output budget), the coverage line names it as `page_capped_by=…` with `page_files_with_matches=…`. Matches and files past the cap are not counted; repeat the call with the page's `next_cursor` and `remaining_skip` to read them.
- Saved codebase understanding treats a cited file that no longer exists as stale (`source_status: "missing"`), even when the freshness snapshot is partial.
- **Edit modes.** Hashline is the default. `read` returns fresh `[path#TAG]` snapshots and `N:text` rows; `edit` accepts `PUT`/`CUT`/`MV`/`REM` operations and capture registers. Changed or unseen anchors cannot authorize a write. Syntax-aware block anchors use actual outermost-starting nodes where supported. Replace, apply-patch, structured patch and sloppy are independently selectable globally or by model; see [Editing grammars and safety](#editing-grammars-and-safety).
  Hashline preflights all operation anchors before staging changes. For current snapshots, a rejected patch reports unread ranges across operations and files together and shows a bounded preview, so a retry does not discover just one missing range at a time. Only complete displayed rows gain read coverage; omitted or truncated rows still require a read. Stale snapshots never gain coverage from this preview.
- **Loop safeguards.** The system prompt is identical on every request of a session (until a tool is loaded or plan mode changes); recalled notes and verified repair patterns are decided once per user turn and stored with that message, so a provider or local server can reuse its prompt cache. A reply that stops at the provider's output-token limit is never taken as the answer. Reasoning-only replies use the reasoning gate below; truncated answer text or tool arguments can retry with a doubled budget that fits the context window, then receive bounded nudges. `harness.max_tokens` defaults to `0` (no operator output cap). Intrinsic tool paging and protocol protections still apply; oversized results report how to fetch the rest.
  Every failed tool result receives recovery feedback in the shared parent/worker engine, including external tools and failures with uncertain effects. The first failure of a kind gets a one-line instruction to fix the reported cause before retrying, plus tool-specific advice. When the same tool fails the same way again in a row (and always for uncertain effects), the feedback adds the full contract: briefly report the observed cause (or that it is unknown), address the diagnostic before retrying, and gather targeted evidence instead of repeating an equivalent failed call. Each request carries at most one advisory harness note, chosen by priority (safety, failure recovery, repetition/loop, completion review, todo reminder); protocol notes, refusals, withheld calls and completion gates are unaffected. Tool-specific advice preserves permission boundaries, distinguishes verification gaps from code failures, and uses source already returned before asking for another read. This feedback is guidance, not proof of a diagnosis or a new permission grant; existing repetition guards and the stop for uncertain effects remain in force. Numeric-looking malformed read selectors such as `:25- fifty` report selector syntax guidance instead of a misleading missing-file error.
  After repeated unsuccessful recovery, the interactive interface asks for direction: continue with
  a corrected approach, supply an instruction, or pause while keeping completed work. Noninteractive
  and sealed runs retain finite loop safeguards; declining the prompt also pauses safely.
  `/todo` opens a read-only plan inspector; selecting a step shows full blocked reasons, findings and recorded evidence. `/todo list` prints the list. The evidence-expansion shortcut expands todo change cards and wraps long reasons.
  Retained artifacts have private lifecycle records. `artifact_read` with `info=true` reports
  availability, retention timestamps and byte size without reading content. A missing blob with
  a record is distinguished from a legacy/foreign reference with no local record; removal or
  expiry cause is not guessed. Lifecycle metadata is storage history, not correctness proof.
- **Silent inference recovery.** Requests managed by the model event pump have a separate
  silence watchdog: by default, 120 seconds without a worker event cancels only that inference
  attempt, discards its partial reply/tool arguments, then retries the same context once.
  Local loading and prompt processing are exempt until generation starts, so a slow cold load or
  prefill is not repeatedly cancelled by the generation-silence limit. Explicit cancellation and
  configured startup/transport limits still apply. Completed tools, files, conversation and todos
  are not rolled back or replayed. After repeated silence, interactive main-model requests ask for
  retry/direction or pause; noninteractive exhaustion reports incomplete while keeping work. The
  old worker must finish before a new attempt can start. Failed/cancelled Dial-managed model servers
  are stopped before retry, ensuring abandoned GPU work cannot block a new request; external servers
  are never killed. This is independent of task/step budgets. Configure it outside the project:

  ```toml
  [request_recovery]
  silence_secs = 120  # 0 disables this watchdog and permits unlimited silent waiting
  retry_once = true  # false requires direction without an automatic retry
  ```
- **Reasoning gate.** Dial-owned MLX-LM requests with recognized thinking markers close the thinking span inside the same generation when token blocks repeat four times, reasoning reaches three quarters of the configured byte/time allowance, or thinking consumes half the output-token budget. Sampling then continues into an answer or tool call with the existing KV cache; this does not end the task or change `harness.max_steps`. Only reasoning is controlled: repeated answer text and tool arguments remain untouched. Speculative/distributed inference, MLX-VLM and external providers retain the client fallback below.
  Main and child agent reasoning time and byte limits default to `0` (unlimited). With positive limits configured, reasoning events and keep-alives do not reset the deadline; hidden reasoning is covered by time, not guessed token counts. Model loading and prefill use their provider timeout rather than the reasoning deadline. A fallback hit cancels only that request and discards all partial calls. One action-focused recovery is allowed per turn; another hit ends the turn as incomplete. Adjust limits in `/settings` or `[reasoning_guard]`. These settings are outside the improvable harness. `enabled = false` disables the time deadline; a positive byte cap remains independent.
  The streamed-reasoning byte cap remains active after answer text or tool arguments start; a short preamble cannot disable it. Main and child agents also compare successful observations, including `bash` results: changed timeout arguments and artifact handles do not disguise unchanged evidence. Failed-call and observation guards share argument normalization: presentation metadata does not disguise a retry, while nested payloads and meaningful arguments remain distinct. Repetition gets explicit investigation feedback; the main agent temporarily withholds the repeating tool until a confirmed editor diff or a successfully completed/blocked todo establishes recovery. Unrelated reads, artifact retrieval, no-op edits, todo polling and plan rewording do not restore withheld tools. Four repeats of the same unchanged observation stop the run; unrelated checks and errors do not erase its history. Only nonzero editor diff summaries reset observation history for fresh verification, and new user steering resets the turn-local guards. A plain, explicit final clarification question ends an interactive turn as `needs_input`, preserving unfinished todos rather than injecting a continuation reminder. This conservative fallback recognizes direct clarification requests, not arbitrary question marks; `finish(needs_input)` remains the preferred explicit protocol. Repeating a substantial explanation three times also sends a loop self-check to the model, even with different calls. Wording alone does not stop genuine progress; result and failure guards enforce those stops.
- **Model I/O robustness.** Remote silence uses `remote_model_idle_timeout_secs`, which defaults to 300 seconds and bounds both the wait for the first event and silence between events; a stalled remote stream fails and is retried under `retry.max_attempts`. Local workers use `worker_timeout_secs`, which defaults to `0` (unlimited). Local model servers (localhost or LAN) are never limited by the remote timeout. Setting `remote_model_idle_timeout_secs = 0` opts out. Positive limits use actual stream progress, not keep-alives or read boundaries. The reasoning gate separately applies any configured pre-action limits. The UI stays editable while waiting; Ctrl-C or Esc cancels. Tool-call arguments that are almost JSON are repaired rather than regenerated; cut-off or trailing-garbage arguments are refused and never executed.
  With a positive timeout configured, native Anthropic streams also bound the first-event wait, including through loopback proxies. `ping`, usage-only updates, and empty deltas do not reset the idle deadline; actual text, reasoning, signatures, tool arguments, and validated lifecycle events do. Timeouts and transient streamed errors use bounded retries, discarding failed partial replies and their tool calls. Cancellation, malformed events, and nontransient errors are not retried.
  OpenAI-compatible chat streams ignore empty/role-only data chunks for idle progress. A nonempty
  `finish_reason` commits the reply; Dial drains optional usage and `[DONE]` for at most 2.5 seconds,
  even with ordinary idle limits disabled, so a finished stream cannot hang on trailing keepalives.
  Optional usage arriving after that grace is unavailable. Empty finish reasons do not commit a
  partial reply; malformed events and cancellation still fail rather than becoming success.
- **Selectable native editors.** Replace and patch grammars support bounded, operator-configurable fuzzy matching; sloppy has its own Unicode/indentation/capture recovery ladder. Ambiguous targets and overlapping edits fail atomically. Canonical replace fields are `old_string` and `new_string`. All modes preserve fresh snapshots, seen-source guards, generated-file protection and complete-target approvals.
- **Forgiving hashline bodies.** A `PUT` body row written without its `+` prefix, or a raw blank line between body rows, is taken as content and the result warns how many rows lacked the prefix, instead of failing the whole patch. Rows that look like unified-diff deletions (`-…`) are still refused. Lines the model was shown are rebuilt from the session's earlier `read`/`edit`/`write` results when the agent restarts or is rebuilt, for files whose content is unchanged since, so a resumed session does not reject edits with "no read of this file is recorded".
- **Compaction is checked, not assumed.** `[compaction]` configures triggers, retained complete tool groups, reserve, method preference, model/agent overrides, speculative and idle maintenance. Available methods are native/remote, image archives, structured handoff, shake and soft summaries. A prepared result is committed only if its branch/model/configuration still match and the full next request, including protected instructions, tools and output reserve, fits. Unsupported native routes, unknown image billing and unreconstructable encrypted history fail explicitly instead of fabricating state. Original journal evidence stays searchable.
- **Stuck-turn detection.** Besides the identical-call guard (warns at the third identical failure, stops at the fourth), dial counts failures whatever the arguments: the same file tool (`read`, `edit`, `write`, `glob`, `grep`, `ast_edit`, `ast_search`, `artifact_read`) failing five calls in a row gets a warning and ten stops the turn, even with successful reads in between (the read-guess-read-guess loop); `bash` and `kernel` failures are task feedback and are never counted. Four rounds in which every call failed add a warning, eight stop the turn. A stopped turn is **reported**, not raised: the reason is recorded in the conversation, shown as a notice, the outcome is `incomplete`, and print mode exits 2 (with `--output-format json` the result has `outcome: "incomplete"` and no `error`). The step limit ends the same way.
- **Step budget.** `harness.max_steps` counts model calls that did work; `0` means unlimited interactive turns, including when an admitted harness has a finite budget. Positive values keep their step limit. Automatic repair and sealed evaluation still have finite caps. A reply that was unusable (truncated answer text or tool arguments, an unreadable call, a tool call written as plain text) is retried without spending a step, up to eight times per turn, and compacting after the server rejected the prompt size is never charged. Sealed evaluation runs get no refunds: their budget is what they measure.
- **Completion and todos.** `todo` supports `set`, `append`, `start`, `done`, `drop` and `block` with a reason; a closed active entry advances automatically. Lists survive resume and compaction and remain visible in the TUI and protocols. Findings/evidence are optional, and todo updates never trigger verification rounds. Accepted completion closes stale progress entries; explicit blockers and running workers cannot be silently completed. Plain final replies and `finish` share completion prerequisites, final configured verification and bounded repair. Task outcomes are `completed`, `blocked`, `needs_input`, `incomplete`, `failed` and `interrupted`; unsuccessful print runs exit with code 2. A stop hook that remains blocking after its retry bound reports blocked, and cannot restart a turn waiting for user input. These contracts do not independently prove arbitrary model claims true.

- **Specialist integrations** (available per `integrations` in the config, loaded on demand; never in sealed evaluation/self-play). MCP tools follow `[mcp.*]`; enabling a schema never bypasses approval:
  - `bg`: long-running processes (`start`/`list`/`output`/`wait`/`kill`) under the same sandbox as `bash`, in their own process group, at most 8 at once, 256 KiB of output kept each, all killed when dial exits. Browser tool loading also exposes `bg` when enabled. Start or reuse local preview servers here, wait for readiness, then open the actual HTTP URL; foreground `bash` kills its process group on return/timeout and refuses common dev-server commands before launching them. Stop temporary preview processes you created after inspection, preserving existing user services.
  - `web_fetch` / `web_search`: HTTP(S) fetch with HTML→text conversion and an SSRF guard (loopback, private, link-local and metadata addresses are refused on every connection and redirect), and search through DuckDuckGo's HTML endpoint (a scraping path that can break) or your own `[web] search_url` template containing `{query}`. Both need approval and are disabled by `sandbox.mode = "strict"` or `allow_network = false`.
  - `lsp`: definitions, references, hover, implementations, type definitions, symbols and diagnostics, plus transactional rename/code-action edits with document-version, UTF-16, freshness and complete-target permission checks. Server commands/startup remain explicit trusted effects; language servers come from `PATH` or `[lsp.<language>]`. After an edit, if a language server for that file is already running, Dial waits up to 1.5 s and appends `[LSP advisory, not verification]` with up to 8 `path:line:col: message` lines: only error diagnostics the edit introduced, compared with what the server reported for that file before the edit. With no running server nothing waits and no server is started. Turn it off with `[lsp] diagnostics_on_write = false`.
  - `dep_source` (read-only, loadable): the source of a dependency at the version this workspace locked or installed, for cargo (Cargo.lock + cargo registry/git checkouts), npm (`node_modules`), python (project `.venv` site-packages) and go (`go.mod` + `vendor/` or the module cache). Arguments: `name` (optionally `@version`), `ecosystem` (inferred if omitted), `query` (literal search, `path:line` results), `path` (package-relative file with optional `:N-M`, or a directory). It never fetches, installs or runs package code and never leaves the package root. Caches outside the workspace are read only under the Dev sandbox policy and outside its denied paths; under Strict, add the cache with `/add-dir`. Read-only subagents may use it too.
  - `check`: runs the focused checks the completion gate selects for this turn's changes (build/type checks, related tests, configured `[verification].commands`) and reports each check's status (`passed`, `code_failed`, `environment_unavailable`, `permission_denied`, `cancelled`, `timed_out`, `unverified`), its command and key diagnostics. Commands use the same approval and sandbox path as automatic verification and results go into the same evidence ledger: a pass at the current edits counts toward final verification exactly like a gate run, and any later edit makes it stale. `names` runs a subset; `failed_only` reruns checks whose latest result was `code_failed`. Before any change it lists the applicable checks and runs nothing unless `names` is given. `repro: {name, command}` registers a shell command that reproduces the reported bug (non-zero exit while it is present) and runs it immediately; it is approved exactly like a `bash` call, rerun by the completion gate, kept for the current turn only (at most 4; reusing a name replaces it), and earns behavioral credit only when it was observed `code_failed` before a change and passes at the final edits (one that never failed is reported `not_shown_to_reproduce`). Offered by default and on the small and lean local tiers when `[verification].enabled` is on.
  - `revert_edit` (loadable): restores one file (`path`) to its content before Dial's most recent checkpointed change to it in this turn, or before the turn's first change with `to = "turn_start"`. It refuses files with no journaled change this turn and files changed outside Dial's checkpoint journal since that change (shell commands, external editors). The revert is itself a checkpointed edit: approvals and edit path rules apply as for `edit`, `/undo` still restores the turn, and the file must be read again before the next edit.
  - `ast_search` / `ast_edit`: structural search and rewrite with ast-grep patterns (`$VAR`, `$$$ARGS`) over 28 languages. `ast_edit` previews by default; applying is all-or-nothing, atomic and checkpointed (`/undo` works). An invalid pattern is refused; a rewrite whose result no longer parses is applied with a syntax advisory, never vetoed. Both are loadable rather than in the default offer: the system prompt lists them and `tool_load` enables them for the next round.
  - MCP: every tool of each `[mcp.<name>]` stdio server appears as `mcp__<server>__<tool>` and requires approval unless the entry sets `trust_read_only_hint = true` and the tool declares `readOnlyHint` (then it runs without approval and read-only subagents are offered it). A server's `readOnlyHint` is not an authorization boundary. Servers are trusted operator programs (unsandboxed); a server that fails to start only produces a notice. Server size no longer determines the default offer: tools are selected for the task, rather than loading every tool from every small server.
    The full-context offer includes `mcp_servers` for runtime status. Task routing selects up to **6 additional native/MCP tools**, with a **24 KiB total schema budget**, using the request, indexed capabilities and dependency names from complete `package.json` files already read during that turn. Library/API tasks can expose a generic documentation service without naming its server; named or observed framework domains select their own documentation tools. Native intents select rendered-browser inspection, dependency source, structural search/rewrite, symbol navigation, debugging or prior-session/change history when relevant. Successful MCP discovery exposes matching same-server get/read/query consumers for the next model request, without trusting instructions in result text.
    Routing is deterministic and local: it makes no extra model, filesystem or network calls, and does not execute tools, install servers, grant permissions, persist as pins or expand a child's allowlist. Additions are monotonic within a turn to avoid needless schema churn. Explicit unload suppresses automatic reactivation until the next external request; explicit pins remain durable. Unrelated turns reset automatic selection. Pinned harness lists and small/lean context profiles retain their existing limits. Other tools remain reachable through `tool_search`, which indexes bounded parameter help and enum choices as well as descriptions. Manifest routing currently ignores partial/raw/batched reads and non-`package.json` formats.
  - `find`: repository retrieval by behavior or exact text. Default behavior mode collects bounded, source-grounded lexical and syntax-declaration candidates (including declarations with no query-word overlap), then asks the configured model to rank candidate indices; paths, spans and snippets come only from captured workspace evidence. `ranking="lexical"` is deterministic and local; `mode="exact"` never calls a model. The semantic request is bounded and non-exhaustive, uses no embeddings or probability scores, and errors honestly if the model or response is unusable. Optional `lsp_path`/`lsp_query` adds live LSP metadata.
  - `browser`: isolated real Chromium tabs, frame-aware DOM actions/evaluation, history navigation (`back`/`forward`/`reload`), screenshots, readable article extraction, dialogs, console/network evidence and sequenced tab `events`, uploads/downloads and approved storage operations. Runs on a built-in pinned Playwright runtime with no operator setup (`playwright_module` overrides it); exact operator-owned local host/port exceptions enable dev-server verification. Downloads use fresh target authority and bounded atomic writes.
  - `kernel` / `eval`: persistent Python and JavaScript state, top-level await, stdout/stderr, results and tracebacks. Authorized tool callbacks use the actual owning controller; nested code-mode/turn-control recursion is refused. Reset/deadline/cancel discards the namespace. OS containment is required and fails closed; JavaScript requires Node 22. Use `bash` for child processes.
  - `dap`: an actual Debug Adapter Protocol client with initialization/launch/attach, threads, breakpoints, stepping/pause, stack/scopes/variables and output events. Requires an operator-installed adapter and approval; unsupported requests are errors, not simulated debugger state.
  - `notebook`: revision-checked cell listing/read/source edits/insert/delete/move with immutable fresh source witnesses, JSON metadata preservation, stale-output handling and atomic rollback. It never executes cells.
  - Rich `read`: bounded JSON/JSONL selection, PDF pages, notebook cells, OOXML DOCX/PPTX/XLSX extraction, ZIP/TAR/gzip members and read-only SQLite. Archives never extract to disk; PDF requires Poppler. JSON selection is a documented bounded subset, not a claim of full jq semantics.
  - Resource reads: `read` also accepts `pr://N[/diff[/all|/I]]` (pull request diff), `issue://N` (issue body, labels and comments, marked untrusted) — both via `gh`, optionally qualified as `[host/]owner/repo/N` — and `conflict://`. A bare `conflict://` lists Git's unmerged paths with hunk counts; `conflict://PATH` summarizes each hunk's ours/base/theirs line ranges, then reads exactly the marked lines through the normal read path, so the output carries edit anchors and a hunk is resolved by replacing its whole range. Path permission rules judge `conflict://PATH` as a read of `PATH`. Unterminated or nested markers are reported instead of guessed.
  - Shell/background output is streamed to the UI and fully spooled before clipping. `artifact://<sha256>` references survive compaction; `artifact_read` retrieves line ranges or exact byte pages. Abandoned temporary spools are removed.
  - `bash` runs in the project root (`cwd` selects a subdirectory; no `cd <root> &&` prefix needed). `background: true` starts a job at once; a command still running after `[shell] auto_background_secs` (default 15) becomes a background job. An explicit `timeout` longer than that threshold keeps the command in the foreground until the timeout, so a long build or test returns its result in one call; a command still running then becomes a background job instead of being killed. Completion is delivered to the model in the next request, and a `bg wait` that sees the job exit returns its unread output, so no separate `bg output` call is needed. Pending bash jobs return control to the model rather than silently blocking completion; continue independent work or explain what is pending and use a bounded `bg wait`. Waits and readiness checks default to 5 seconds (explicit `timeout: 0` remains unlimited). Job output has its own live transcript panel; `bg output` and artifacts retain the captured output. Read-only job waits do not hold the workspace mutation lock. `bg` remains available subject to the session's mode and tool allowlist.
  - Bash interceptor (`[bash_interceptor]`, on by default in `hint` mode): a command that is only a duplicate of a dedicated tool (`cat`/`head`/`tail` or `sed -n 'A,Bp'` → `read`, `grep`/`rg` → `grep`, `find -name` → `glob`, `sed -i`/`perl -i` → `edit`, `echo >` → `write`, `nohup`/`&`/dev servers/watch mode → `bg`) still runs, and its result gains one `[hint]` line naming the tool. A leading `cd DIR &&` and trailing `| head`/`| tail` count as part of the duplicate; batched compound commands, pipes into other programs, heredocs and `find` with `-exec`/`-delete`/time or size predicates are left alone, since one batched call saves model round trips. A hint names a tool only when the current request offers it. `gh run view|list` or `gh pr checks` output that still shows an unfinished run (or a `sleep` loop around them) gains a hint to wait once with `github` op `run_watch`, or `gh run watch <id> --exit-status` plus `bg` `wait`, instead of polling. `mode = "block"` refuses any command with a matching segment; `enabled = false` turns it off; `patterns` replaces the built-in rules. User `!` commands are never intercepted.
- Native tool calling over streaming. When a local server does not parse tool calls itself, Dial reads the text formats local models emit: `<tool_call>` JSON (Qwen/Hermes), Qwen3-Coder `<function=…><parameter=…>` XML, Mistral `[TOOL_CALLS]`, Llama 3 `<|python_tag|>` and gpt-oss harmony `to=functions.…` messages. An unreadable block becomes a malformed call that is reported to the model and never executed.

**Safety**
- Approvals: `write` (default) runs workspace edits, file writes and non-destructive shell commands without a prompt and asks before destructive or privileged commands (recursive deletes, history-rewriting git, `sudo`, piping downloads into a shell) and tools that leave the workspace (network, browser, MCP). `ask` presents Allow once / Always allow this session / Always allow in this project / Deny before every mutation, shell execution and operator tool; `auto` / `-y`; `readonly`. Project-wide grants are saved to the project's `permissions.toml`. Always-allow for bash records a command prefix, not blanket shell access. Unsupported compound shell syntax cannot inherit a prefix grant. Without a terminal, `ask` and `write` deny what they would have prompted for unless a named tool or permission rule authorizes it.
- Shell sandbox (macOS `sandbox-exec`, fails closed): `dev` (default) can read most of the machine but never `~/.ssh`, `~/.aws`, cloud/keychain dirs or dial's own state, writes only the project (plus configured `extra_write`), secret-looking environment variables are dropped; `strict` is workspace-only with no network; `off` is an explicit opt-out. `sandbox-exec` is deprecated and this is not a security boundary against a determined local attacker.
- Checkpoints: file edits made through Workspace tools (write, edit, ast_edit and similar) are journaled per turn. `/undo` drops the last turn and restores its journaled files; `/rollback n` restores journaled files only. Shell commands and approval-requiring extension calls, including `git` switch and branch creation, are uncheckpointed: review them with staged and unstaged Git diffs.
- Configuration, state, memory, sessions and evaluation data live under `~/.dial` (or `DIAL_HOME`). You can run Dial from your home directory without relocating this storage: native file tools and sandboxed commands exclude the private tree, even when it is inside the workspace. Starting a workspace inside the private tree itself is still rejected. Shell sandbox `off` remains an explicit isolation opt-out.
- Linux/WSL2 masks existing private paths inside writable roots. If a denied path there does not exist (for example a credential directory directly under HOME), sandboxed commands still fail closed rather than creating host directories or risking disclosure of credentials created later. This restriction does not apply to macOS.

**Context**
- Project files: `AGENTS.md`, `CLAUDE.md`, `.cursorrules`, `.dial/AGENTS.md` up to the git root; `~/.dial/SOUL.md` for identity; `.dial/SYSTEM.md` / `APPEND_SYSTEM.md` overrides. Treated as data, not commands.
- `@file`, `@dir/`, `@diff`, `@staged` references in messages.
- Skills (`.dial/skills/<name>/SKILL.md`, `~/.dial/skills`, agentskills.io format) loaded on demand; the agent can create and refine them (`skill_manage`).
- Provenance memory lives in private `$DIAL_HOME/memory.sqlite3`. `recall`, `retain`, `memory_update`, `forget`, `memory_history` and approved `memory_import` support project/global scope, source/session provenance, revision-checked corrections and expiration. `recall` and `retain` are offered by default; `memory_update` (a correction by the id and revision `recall` returns) and the rest are listed as loadable and enabled with `tool_load`. Relevant bounded entries are retrieved on demand with lexical BM25 ranking. Legacy `MEMORY.md`/`USER.md` import is explicit (`confirm=true`) and preserves originals; old configuration tool selectors are migrated, not kept as executable aliases. Forget is logical redaction, not forensic disk erasure.
- **Saved codebase understanding.** Dial automatically builds a project-scoped source map in private `$DIAL_HOME/projects/<identity>/codebase.sqlite3`, outside the repository. It records file outlines, actual syntax declarations, imports, module documentation and bounded manifest/README excerpts; unchanged file hashes reuse saved outlines across restarts and new sessions. Each turn refreshes changed sources in a background worker. With `[codebase] ambient_context = true` (off by default) a relevant slice is attached to each user message, keeping the system prompt stable; otherwise retrieval is left to the explicit `codebase_memory` tool. Independent child agents reuse the same map. Agents save deeper responsibilities, relationships, entry points and check commands with `codebase_memory` `action="remember"`, a `topic`, factual `summary` and 1–8 supporting `sources` paths. Findings carry source hashes, the originating parent session and a revision; changed, removed or inaccessible sources withhold the old summary and expose the topic/revision/paths for targeted refresh. When every cited source is gone, architecture sections and findings are shown compactly (identity, revision, status and how to invalidate/forget or re-synthesize) without source or dependency path lists; saved findings are never deleted automatically. These are agent observations, not independent verification proof. `action="recall"` retrieves more; `refresh` updates the map and drops index entries for deleted files even when a scan is partial; `forget` removes a topic. Updates and forgetting require the observed `expected_revision`; a new topic defaults to 0. Cache operations do not edit project files or require a project mutation approval. The automatic map respects ignore rules and excludes generated directories, links, binary and oversized files; copied text uses the existing secret/instruction screening. Scans are bounded to 2,048 files, 16 MiB and three seconds, with partial coverage reported; files over 256 KiB are omitted and at most 128 findings are saved. Source outlines are structural excerpts, not a complete call graph or generated architectural explanation. Default turn context is 8,000 bytes. Configure `[codebase]` or `/settings` → Models → Project understanding; disable it with `enabled=false`. Sealed evaluations receive no persistent project knowledge.
- **Code change history.** With `[codebase].history = true` (the default), the end of every turn that changed project files records one version per file in private `$DIAL_HOME/projects/<identity>/history.sqlite3`: a unified diff from the file's content before the turn's first write to its content when the turn ends, with the git branch (or `detached@<sha>`), the prompt that produced it, the session and the turn number. No model calls are made. Diffs are redacted for recognized credentials and capped at 32 KiB each (`[diff truncated]`); binary files record only that they changed; private paths (`.env*`, `.git`, `.dial`) and files in added directories are skipped; the oldest versions beyond 5,000 are pruned. The loadable `code_history` tool searches these across sessions: `action="search"` with `query` words (full-text over path, prompt and diff; newest first without a query) and optional `path` (file or directory) and `branch` filters, and `action="show"` with an `id` for one diff. `/code-history [words] [path:<p>] [branch:<b>]`, `/code-history show <id>` and `/code-history clear` do the same in the terminal. Only changes made through Dial are recorded, so history is not a substitute for `git log`; results describe past versions, not the current file.
- Sessions: exclusive-writer JSONL with crash/durability reporting, resume/fork, branch-aware rewind, configurable atomic compaction and full-text `session_search`. Read-only snapshots never claim mutation ownership; experimental notes/reset records remain branch-local.
- Historical evidence stays append-only. The request context is a projection of it, built to keep the conversation prefix byte-identical from step to step so a provider or local server can reuse its prompt cache: recalled notes are stored with the user turn they were recalled for, a superseded read becomes a fixed placeholder once (and only while little text follows it, or when the context is filling up), and under context pressure old, large `read`/`grep`/`glob`/`find`/`bash`/`lsp`/web results outside the newest fifth of the window are replaced once by a pointer to their full text (`artifact_read` retrieves it). Mutation results, todo and memory updates, and small results are never replaced. `working_state` preserves explicit goals, constraints, decisions and unfinished work; compaction also carries the original goal and evidence references. Agent notes are not verification proof.
- `working_state` keeps legacy `update` replacement semantics. `view` returns a revision; `patch`
  requires `expected_revision` and preserves omitted fields, refusing stale writes atomically.
  Optional addressable `claims` carry `id`, `text`, `status` (`unverified`, `supported`, `retracted`,
  `superseded`) and `source_refs`; even `supported` is a model assertion, not verification proof.
  State is limited to 32 KiB and 64 claims. Compaction carries an exact machine-readable projection
  of the current request, original goal, working state, todos and evidence-index ownership outside
  lossy summary prose. Oversized critical contracts fail explicitly without committing compaction
  rather than dropping constraints; historical working-state tool calls remain compatible and inert.
- Custom slash commands: `.dial/commands/*.md` (`$ARGUMENTS`, `$1`…).
- Session commands include `/help /new /sessions /resume /fork /tree /rename /export /copy /plan /permissions /cost /compact /undo /rollback /todo /skills /memory /commands /search /approval /outcome /context /improve /understanding /laya /reasoning /thinking /exit`. Launch-only modes have in-session switches: `/advisor on|off` (coach-role review of each turn, like `--advisor`), `/external-thinking on|off` (private scratchpad, like `--external-thinking`) and `/plan-yolo [MODEL|off]` (auto-approve the next plan and hand it to MODEL, like `--plan-yolo-into`). CLI maintenance runs in the background with its output in the transcript: `/gc [apply]`, `/anonymize [SESSION] [-o PATH]`, `/images [status|doctor|probe|purge]`, `/update [VERSION]` (then `/restart`), `/benchmark [flags]` and `/schedule list|add|remove|log|run`. `/relay start [ADDR]|stop|status` runs a local collaboration relay for `/collab`, and `/hooks` lists the configured lifecycle hooks. Independent supported read tools run concurrently with mutation barriers and ordered results; read-only `bash` commands (the safe-command gate) join those groups unless they set `session`, `background` or `async`, which stay serial. Interrupted tool-call groups are closed with explicit results for safe resume; automatic compaction is announced. Process startup and non-cancellable external tools may still delay interruption.

## Editing grammars and safety

`edit.mode` defaults to `hashline` and accepts `replace`, `hashline`, `apply_patch`, `patch` and `sloppy`. Replace uses
`path`, `old_string`, `new_string` and optional `replace_all`; no `old`/`new` aliases.
Hashline, apply-patch and sloppy accept one `input` string. Structured patch accepts
`path` and `edits` with `op`, `rename` and `diff`. Sloppy uses `*** Edit File`, `*** Find`
and `*** Replace`/`*** Insert Before`/`*** Insert After`; its bounded recovery ladder is
separate from replace/patch fuzzy matching.

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
model selector rules and the global mode, in that order. Selector matching is case-insensitive;
the longest matching substring wins, with deterministic lexical ties. `--edit-mode` and
`DIAL_EDIT_VARIANT` accept all five modes; environment value `auto` follows persisted policy.
`DIAL_EDIT_FUZZY` accepts `1`/`true`/`0`/`false`; `DIAL_EDIT_FUZZY_THRESHOLD` must be finite in `[0,1]`.
Loading a valid saved configuration migrates the old root `edit_mode` into `[edit].mode`;
model-profile and harness `edit_mode` fields remain intentional per-model pins.

All grammars retain fresh-file checks, complete-target approvals, cancellation and atomic rollback.
Only full verbatim returned read/search lines establish edit witnesses; truncated, skipped,
lossy and declaration-summary rows do not. Generated-file protection and seen-line enforcement
are operator controls, not model arguments. Optional parse repair uses the actual coach role,
returns complete source, is reparsed and is authorized again before any write. Blackbox recording
stores bounded source in private state without promising secret redaction; opt in deliberately.
Completed edit previews never execute tools or abort partial payloads; typed no-change results are
not failures. Previews without a current source-read grant are skipped rather than leaking diagnostics
or prompting for an implicit permission. Inline recovery applies only to an entire unfenced sloppy
completion and follows ordinary approvals, hooks and journaling.

Notebook editors use a source projection with `# %% [code|markdown|raw] cell:N` markers,
not a lossy rewrite of notebook JSON. Existing indexed cells retain their IDs and metadata;
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
claim, refuses unresolved failed checks, refuses open todos, and refuses while background jobs it
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

`dial annotate artifact://HASH --note TEXT --start N --end N` stores bounded human feedback against
an immutable artifact hash and source range. `--list` returns a paginated annotation record.
`artifact_read` with `annotations: true` reads these records as source data; annotations never grant
execution authority or replace the artifact.
