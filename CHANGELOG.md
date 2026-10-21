# Changelog

All notable changes to Dial are recorded here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Entries are taken from the git history (commit subjects, shown by short hash) and tags `v0.1.0` through `v0.1.7`.

## [Unreleased]

## [0.1.15] - 2026-10-21

### Added
- Task-driven tool routing selects bounded native and MCP specialists from the request and dependency manifests already read, without requiring server names. Successful MCP discovery exposes relevant same-server consumers. Small MCP servers are no longer loaded wholesale; automatic additions share a six-tool/24 KiB budget and explicit unloads stay suppressed for the turn. Tool search also indexes parameter help and enum choices.
- Verification causality: unchanged bounded nonbehavioral baseline failures remain disclosed without unrelated automatic repair; explicit runner metadata recognizes missing prerequisites conservatively. Rust compilation targets owning packages, including non-default workspace members, and broadens for shared manifest/lock changes.
- Revision-checked `working_state` patches and addressable unverified claims; compaction carries exact designated critical state outside summaries. Worker handoffs pack priority fields and expose separate acceptance/blocker envelopes without changing custom-schema JSON results.
- Opt-in strict declared-requirement settlement and selective bounded independent coach review, using existing approvals and evidence. Neither reviewer claims nor model-recorded state become verification proof.
- Browser runtime provisioning can be disabled with `browser.allow_install = false` for cache-only execution and isolated integration evaluations.
- Live local-model request settings in the status bar (`inference` in custom layouts), with deduplicated Model settings notices when controls change and full details in `/policy`. Server-controlled values are explicitly unknown rather than guessed.
- Busy messages default to agent-selected routing: changes of approach steer the current task, while clearly independent requests start isolated background workers. Explicit after-turn queueing remains available.
- The `browser` tool's `back`, `forward`, `reload` and `events` actions run in Dial's own Chromium driver. Previously they worked only inside an external Tern pane. `events` returns the tab's console, network, dialog, download and navigation events in `seq` order; pass `after` set to the returned `last_seq` to get only newer events. `evidence` also includes the tab's navigation history.
- `check` tool: runs the focused checks the completion gate would select for the current changes and records results in the same evidence ledger, so the agent and the gate agree on what passes. Offered by default and on the small/lean local tiers when verification is enabled.
- Loadable `revert_edit` tool: restores one file to before Dial's last change this turn (or the turn start), refusing files changed outside Dial's journal; the revert is itself checkpointed.
- Loadable read-only `dep_source` tool: reads and searches a dependency's source at the version the workspace locked or installed (cargo, npm, python venv, go) without fetching or running anything.
- `read path:outline` lists a file's declarations with line ranges (not an edit witness).
- File tools add a `Did you mean:` line with up to five existing paths when a workspace path does not exist.
- After an edit, a running language server's new error diagnostics for that file are appended as an advisory (at most 1.5 s, 8 lines; no wait without a live server).
- Small local tier: prompt ends with a compact worked example in the active edit grammar (its check step uses `check` when offered), and calls are one per response unless the model's saved reliability record passed the batch check.
- `/model settings` suggests enabling coach escalation (choose the coach in `/roles`) for local models with many failing rounds and no coach role.
- llama-server: a notice names `--jinja --chat-template-file` when `/props` shows the chat template lacks native tool support.
- Read-only subagents (scouts, reviewers, planners) may use `dep_source`; its results count as observations for pacing advice and context pruning.
- Benchmarks: `reliability.py` reports unbiased per-task and mean `pass_at_k` and `pass_hat_k` (τ-bench pass^k) curves for k = 1..repetitions.
- `grep` `output = "files"` lists matching files with match counts (paged by file) and `output = "count"` gives totals; capped line pages point to them. Sessions recorded before this addition still resume.
- MLX: schema-constrained tool calls cover Qwen3-Coder's XML format (`<function=…><parameter=…>`): declared names and keys, required keys, string enums, and typed values checked as each parameter closes.
- `check` `repro: {name, command}` registers a turn-scoped bug reproduction (at most 4), approved like `bash`; it earns behavioral credit only after it was seen failing before a change and passes at the final edits.
- Dial-launched llama-server: when the installed build supports `--kv-unified`, `--kv-unified-per-slot` and `--no-cache-idle-slots`, it starts two slots over one shared pool (the full fitted window plus a side reserve, only if that fits memory), so side requests and workers no longer evict the main conversation's cache or halve its window. Older builds keep one slot.

### Changed
- Small and lean local tiers: `edit` refuses a change that would add parser errors to a Rust, Python, TypeScript/TSX, JavaScript or Go file and writes nothing; reads stay valid for a corrected retry, and `allow_syntax_errors: true` overrides. The full tier keeps the advisory.
- First tool failures get a one-line recovery instruction; the full recovery contract is attached when the same tool fails the same way again (always for uncertain effects). Each request carries at most one advisory harness note, chosen by priority.
- Local escalation steps take effect at the next user turn, so the prompt and tool offer stay stable within a turn; the reached step is still saved per model immediately.
- The system prompt only mentions `skill`, saved project maps and the LSP when those tools are offered.
- Codebase-memory sections and findings whose sources are all gone are shown compactly, without source or dependency path lists.

### Removed
- The Tern integration. Tern is not part of Dial. Removed: its browser-pane backend (`TERN_PANE_SOCKET`/`TERN_PANE`, `DIAL_BROWSER_TERN`), the `browser.tern` setting, the Tern-only `fork` browser action and the `tern` terminal id for notifications. Saved configuration that still sets `browser.tern` is migrated on load.

### Fixed
- Windows startup resolves the user directory from `USERPROFILE` when `HOME` is missing; model discovery uses the same fallback. First-run setup follows panel navigation and connection tests instead of treating them as cancellation, and local-server setup explains rescanning and preserves real connection errors.
- Remote model requests whose connection died while the model was silent (sleep/wake, a Wi-Fi or VPN switch, an expired NAT mapping) no longer wait out the full 300 s idle limit before retrying: TCP keepalive probes detect the dead path within about a minute. A real header timeout now reads "no response before the silence limit" instead of "timeout: receive response", and retry notices say how long the failed attempt ran.
- Changing models on the welcome screen updates its model label and the status bar together as soon as the new configuration is adopted, without resetting recent-session navigation.
- Local models that ship `do_sample: false` now request greedy decoding (temperature zero) instead of falling back to the harness temperature. Explicit sampling overrides retain precedence.
- Local thinking now defaults to `assistance.local.thinking = "model"`, preserving model/server settings instead of automatically switching thinking by request phase. Phase-based switching remains an explicit opt-in.
- A llama-server started without `--jinja` that rejects `tools` is now a fatal error naming the fix, instead of being retried as transient.
- `codebase_memory refresh` drops index entries for deleted files even when a scan stops at a limit.
- Dial-launched llama-server passes each option and its value as separate arguments; current llama.cpp rejects the `--host=127.0.0.1` form, so the launch failed.

## [0.1.13] - 2026-10-19

### Added
- New conversations open on a centered launch screen: a braille knob and wordmark (the needle turns into place at start-up when `ui.motion` allows) above a compact input card. One line beneath shows the model, current directory and Git branch; setup and unsafe-mode warnings appear only when relevant. `Down` reveals suggested requests or recent sessions, and small windows shed artwork before warnings and selected actions.

### Changed
- Configured MCP servers connect in the background while the start screen draws, and typing stays live until they are ready, so slow servers no longer hold up the first frame.
- Session listings are cached per project (`session-index.json`, keyed by each journal's size and modification time), so the start screen and `/resume` parse only journals that changed. The index is refreshed in the background at start-up, and resume-picker previews load off the UI thread.
- The `band` composer is a tinted card whose accent edge follows the input state, with the status line and key hints beneath it. The Dial dark theme uses a quieter selection background.
- Picker rows drop a description that only repeats their label; the detail pane still shows it in full. Idle palettes no longer show a `ready` title note.

## [0.1.12] - 2026-10-19

### Fixed
- Release container builds receive the tagged source revision explicitly, so excluding Git metadata from the Docker context no longer causes release compilation to fail. Containers also embed the release's trusted update public key.
- The terminal lifecycle regression follows the interactive work-plan inspector, including paging through long blocked-step details and returning to the plan.
- The lean-tool fallback integration test derives its context limit from the actual full request instead of assuming a platform-specific prompt size, retaining coverage on Linux and macOS.

## [0.1.10] - 2026-10-19

### Added
- The MCP browser and `mcp_servers search` also list npm packages tagged `mcp` that declare a command, and GitHub repositories, next to the official registry, Smithery and the curated list. Sources are read concurrently, and a server listed by several sources shows once with every badge. npm servers install as `npx -y <package>@<version>`. A GitHub repository installs only when its root `package.json` or `pyproject.toml` names an npm or PyPI package that names the same repository as its source and has one clear command; otherwise the row links the repository for manual setup. Every install still asks for approval and must start and list its tools before it is saved.
- The start screen is live: it sits above the input until your first message, with the model, project and mode beside your recent sessions (or starter ideas on a first visit). Down from an empty input selects a row and Enter resumes that session or loads that starter; Esc returns to the input. After the first message it folds into one summary line in scrollback. Terminals under 24 rows get a compact single-column version. The wordmark gets one 0.7 s brand-colour sweep at start-up.
- `ui.motion` (Settings → Display → Motion, on by default) controls the start-up sweep and the short reveal when a full-screen panel opens. Motion is also off under `NO_COLOR`, with ASCII glyphs and without truecolor.
- Code change history: every turn that changes project files records each file's diff with the git branch and the prompt that produced it, in private per-project storage and without model calls. The loadable `code_history` tool and `/code-history [words] [path:<p>] [branch:<b>] | show <id> | clear` search these versions across sessions. On by default; turn off with `codebase.history = false` or `/settings` → Project understanding → Code change history.

### Changed
- Completed the Rust workspace layout: command handlers, platform backends, provider compaction, configuration schemas and embedded tool drivers live with their owning modules. Architecture checks guard module definitions and workspace layering; CI now runs the embedded driver protocol contracts.
- Fewer model round trips on long commands. A `bash` call with a `timeout` longer than `shell.auto_background_secs` (15 s by default) now waits in the foreground up to that timeout, so a long build or test returns its result in one call; if it is still running then, it moves to `bg` instead of being killed. A `bg wait` that sees a job exit now returns its unread output, so a separate `bg output` call is no longer needed. The system prompt tells the agent to give long checks a `timeout`, and to launch subagents with `task action=start` when it has its own work to continue.
- The slash menu opens upward from the input, so the input stays put, and shows 12 rows by default (`ui.autocomplete_items`). Typed alone, `/` lists everyday commands grouped as Models, Settings, Sessions, Context, Changes, Tasks and Tools, each group named once at the right; typing still ranks fuzzy matches across every command.
- Your messages in the transcript sit on a faint full-width tint behind the accent bar (the Dial themes now define `userBg`), and each run of tool calls hangs off one thin rail in the first column, so prose, your turns and the agent's work read as separate blocks.
- Panel footers use the same words everywhere: Esc is `close` at a panel's top level, `back` inside a nested view, `cancel` for an edit or question and `clear search` while a search or filter is typed, and the Esc hint changes with what the next Esc will do (Extensions, the subagent hub). Tab strips share one style (accent, bold, underlined) in every panel, and ←/→ also switches the subagent hub's tabs. The Compute panel no longer says `back` where Esc closes it, and keys follow your keymap instead of a hard-coded `Esc`.
- `/extensions` is described as the MCP server, plugin and skill browser it opens; `/status` still prints the configuration.

### Fixed
- Windows shell status painting imports its application type on every supported platform.
- Windows private storage recognizes the current token's default owner on elevated processes and installs private ACLs through an identity-checked handle without changing append-only access.
- macOS builds retain compact unwind information so caught panics and panic-contract tests unwind correctly instead of aborting.
- Document writers recognize Windows lock contention and wait for the existing writer; concurrent subprocess tests allow bounded time for inherited lock descriptors to close.
- The cmux client reads buffered replies after a Unix-socket peer closes on macOS instead of failing while updating the disconnected socket's timeout.
- JavaScript eval preserves falsy asynchronous rejection reasons on Node 22 and keeps the REPL usable after a failed cell.
- Browser tests keep orphan-process bookkeeping inside temporary state directories instead of recreating runtime folders in the checkout.
- The GitHub-release-only root package is explicitly non-publishable, matching its workspace members and satisfying the dependency policy for local path dependencies.

## [0.1.9] - 2026-10-16

### Added
- The `lsp` tool installs a missing language server the first time a file needs it, then continues the request. It looks on `PATH` and in Dial's own `~/.dial/tools/*/bin` and `node_modules/.bin` first, asks for the same approval as a server start, installs through npm, uv, `go install`, `rustup component add` or Homebrew (never sudo), and installs Node.js or uv first when the server needs them. A server you configured under `[lsp.<name>]` is never replaced. Install failures are appended to the original "no language server" error.
- Install recipes for the bash, yaml, html, css, json, eslint, svelte, vue, astro, php (intelephense), docker, prisma, vim, graphql, tailwind, emmet and biome language servers (npm), pylsp, basedpyright, ruff and ty (uv), and marksman, texlab, jdtls, elixir-ls, gleam, helm-ls and nixd (Homebrew). rust-analyzer now installs with rustup and gopls with `go install`, each falling back to Homebrew.
- `/models` download has an Embeddings shelf that searches Hugging Face for GGUF embedding models. A downloaded GGUF whose architecture is an embedder (bert, nomic-bert, jina-bert-v2, modern-bert and similar) is classified as an embedding model so it can be assigned to the `embedding` role.
- `/setup memory` lists each memory backend with its prerequisite and status, and enables the one you pick. `/memory help` lists the commands of the selected backend, and `/memory` with the backend off explains how to choose one.
- `dial doctor` and `/setup list` end with a Memory section: the selected backend with what it still needs, and every backend's readiness with the `dial config set memory.backend <id>` command that selects it. Headless setups no longer need the TUI to see why memory is inactive.
- Semantic memory offers `memory_update` and `forget` tools, accepts `scope: "global"` on `retain`, and addresses user-wide facts as `user:<n>` in `/memory forget|update|history`. `/memory clear user` clears the user-wide bank.
- Assigning the `embedding` role or finishing `/setup embeddings` offers to switch to semantic memory.
- `dial -p` runs one bounded memory maintenance pass before exiting.

### Changed
- Automatic memory recall runs on every user message for the semantic and Hindsight backends and attaches only facts not already shown. Compaction recalls from the selected backend.
- Semantic auto-retain pauses while the `memory` role is unassigned instead of using the main model. The Roles view shows the memory role as required (local, sharpshooter) or optional (semantic) rather than "follows main".
- Settings → Semantic memory → Embedding model assigns the `embedding` role instead of writing half of an advanced override, and `validate` rejects a half-filled `embedding_base_url` / `embedding_model` pair. `/memory clear` on semantic memory clears only the project bank.
- The embedding role picker also lists models whose kind the server does not declare, labelled "unclassified".
- The system prompt now says when to use the memory tools the selected backend offers (`recall` for past decisions and preferences the repository does not show, `retain` for durable facts), for the root agent and for subagents that have them. Before, only auto-learn had guidance, so the default backend's tools were used rarely.
- The `zip` dependency moves to 9.x because every 4.x release was yanked; ZIP members whose names are not valid UTF-8 are now rejected in documents, archives and runtime downloads instead of being lossily decoded.

### Fixed
- Memory recall no longer discards keyword results when the embedding service is slower than the recall deadline.
- Hindsight writes queued by earlier sessions or history generations are now sent; sharpshooter envelopes and observations from turns finished while maintenance was busy are no longer stranded.
- The memory-paused notice reappears after settings change the backend, with consistent next steps.
- Memory tools remain available to lean/local models, and `learn`, `reflect` and the auto-learn guidance reach the root agent.
- Semantic memory: consolidation no longer merges facts whose numbers differ ("limit is 1 rps" vs "limit is 2 rps"), which discarded the correction and kept the stale value. `/memory clear` also drops turns queued for extraction instead of re-extracting them into the cleared bank. An extraction reply wrapped in one Markdown code fence is accepted.
- Semantic memory failed with "database is locked" when a second dial session, or a background thread, opened the store while another connection had it open: each open switched the file from WAL to rollback-journal mode and back, and that switch cannot wait for other connections. The store now stays in WAL mode.
- Background compaction on ChatGPT plans no longer injects an unsupported output-token limit into the summary model profile. Other providers retain the summary budget, and explicitly configured unsupported overrides still report validation errors.
- The `browser` tool failed with "Chromium is not installed" when `browser.playwright_module` pointed at a Playwright whose own Chromium revision was never downloaded, even though other Chromium builds were installed. It now falls back to the newest complete Playwright build (dial's private runtime, `PLAYWRIGHT_BROWSERS_PATH`, or Playwright's per-user cache such as `~/Library/Caches/ms-playwright`) and then to an installed Chrome, Chromium or Edge. The built-in runtime also uses this fallback when its Chromium download fails, for example offline. When no browser is found, the error gives the exact `node …/cli.js install chromium` command for the configured Playwright.
- Scouts and other read-only subagents no longer get the `retain` and `learn` tools.
- Windows: private-file verification accepts files owned by `BUILTIN\Administrators`, which an elevated token creates; the DACL still grants only the current user.

## [0.1.7] - 2026-10-15

Includes the changes prepared for v0.1.5 and v0.1.6, whose publication was blocked by an outdated integration test and a Windows compile error. The test now verifies that unseen-line errors provide read evidence for a direct retry; sandbox cache setup is correctly limited to non-Windows platforms.

### Added
- The stats ledger records each request's transport and cache-invalidation fingerprint (`transport`, `invalidation`, `common_prompt_bytes`, `prompt_bytes` in `requests`), so a prompt-cache miss can be attributed to a prompt change or to the provider.
- **Teach this model** (`/settings` → Model → Learning, `improve.teach`): an opt-in switch that lets a local model (MLX, Ollama, LM Studio) learn from your coding sessions. Turns are recorded, and the latest version admitted by the evaluation gate serves the session, LoRA adapter included. Turns are approved without typing scores: committing a turn's exact changes, Alt+Up, or passing behavioral checks counts as good; Alt+Down or `/undo` as bad. Hosted models are never recorded.

### Changed
- Faster releases and CI. Release binaries use thin LTO instead of fat LTO with one codegen unit, which had taken 14-43 minutes per target. Release builds start when the tag is pushed and run alongside CI; publishing still waits for CI to pass on the tagged commit. CI builds and tests with the `ci` profile (no debug info), which lowers memory use and link time on the macOS runners.
- On models with very large context windows, the history used for compaction and pruning is now capped at 262,144 tokens by default (`efficiency.history_token_cap`; 0 restores the full window). On a 1M-token window, automatic compaction now starts at about 178k tokens instead of 680k. Windows of 262k tokens or less are unaffected.
- The bash interceptor (`[bash_interceptor]`) is now wired into the agent's `bash` tool and on by default in a new `hint` mode. A command that only duplicates a dedicated tool (`cat`, `sed -n 'A,Bp'`, `grep`/`rg`, `find -name`, `sed -i`, `echo >`, `nohup`, dev servers) still runs, and its result gains one line naming `read`, `grep`, `glob`, `edit`, `write` or `bg`. Batched compound commands are left alone, because one batched call saves model round trips. `gh run view|list` and `gh pr checks` results that still show an unfinished run get a hint to wait once (`github` op `run_watch`, or `gh run watch` with `bg` `wait`) instead of polling. `mode = "block"` keeps the old refusal behavior. Before this change the setting was accepted but never applied.
- Interceptor rules: `cat >file` and `cat <<EOF >file` count as writes, not reads; `tail -f` points to `bg`, not `read`; `find` with `-exec`, `-delete`, time, size or permission predicates is no longer flagged; `perl` counts as an in-place edit only when `-i` is one of its leading switches; and the service, background and watch rules name `bg`.

### Fixed
- GPT background compaction requests now send an empty tools array instead of null, avoiding HTTP 400 errors from endpoints that require an array.
- Resuming or reopening a session could fail with "already open for writing" right after its writer closed, because a tool process being started at the same moment briefly holds a copy of the session file's lock. Reopening now waits up to two seconds for such a lock before refusing. This also removes intermittent failures in the session and resume tests.
- Fewer avoidable tool failures and bookkeeping rounds. `codebase_memory remember` normalizes cited paths (`./`, `path:12-40`, absolute paths in the root) and re-indexes changed sources inline instead of refusing, and names the path when it still cannot. Sandboxed dev commands get writable uv/pip/npm/Go/XDG caches under dial's cache directory. Generic recovery advice is no longer attached to mechanical errors. A response that sends only `todo` updates gets a once-per-turn nudge to do the work in the same response.
- An edit that touched lines no read had shown failed with "read them first", which cost a read call plus a retry. The error now shows those lines and counts them as read, so the same edit can be re-issued directly. Nothing is written until then, and lines whose snapshot is stale still need a fresh read.
- ChatGPT/Codex sessions lost nearly all prompt caching after an idle gap (a background wait, a long tool, a pause before the next message). The reused websocket had been dropped by the server, its first write failed, and the session was moved to SSE for the rest of the process. SSE requests carried no session affinity headers, so they reached arbitrary servers. Idle sockets are now replaced before use, a dead reused socket is reconnected and the request replayed instead of switching to SSE, and SSE requests send `session_id`/`conversation_id`.
- `/outcome` and `/undo` feedback had nowhere to go in coding sessions, and `/improve` told you to use them anyway; both now record when Teach this model is on and say how to turn it on otherwise.

## [0.1.4] - 2026-10-14

### Added
- Native Cursor provider: `/providers` sign-in, model catalog, usage report and streaming chat with tool-call handoff over Cursor's Connect/protobuf agent protocol, with no Node or Bun runtime. Cursor's protocol is private, so this path is verified against local fake servers only.
- `semantic` memory backend: per-project and user-level SQLite banks with keyword (FTS5) recall out of the box, hybrid keyword + embedding recall when the `embedding` model role is assigned (Alt+M lists only embedding-capable models for it), automatic fact extraction after substantive turns with the `memory` role (`memory.semantic.auto_retain`), periodic duplicate consolidation (`consolidate_interval_minutes`), a token budget, and `/memory rebuild|history|update|forget`. Saved settings that selected the earlier sidecar memory backend migrate to `semantic`.
- Skill packages: `/skills install|update|remove|installed|search` and `dial skills …` install skills from Git URLs (`url@ref#path`) or user-configured registries (`skills.registries`), with a lock file recording the resolved commit and integrity, a Skills tab in `/extensions`, and precedence project > user > packages > other tools' skill folders. Install scripts never run.
- Browser relay: `dial browser-relay install|serve|status` and a bundled Dial Chrome extension let the `browser` tool drive your logged-in Chrome tabs over a token-authenticated loopback relay, with no remote-debugging flag (`browser.relay`, `browser.allow_attach`, `DIAL_BROWSER_RELAY`).
- Built-in shell: `shell.backend = auto | system | embedded | powershell`. `auto` uses a system bash (Git for Windows bash on Windows), otherwise a built-in bash-compatible shell with common coreutils, so the `bash` tool works without a system bash. About 3.3 MiB of binary; it can be compiled out with the `embedded-shell` feature.
- Isolation backends for workers (`isolation.backend`): `auto` picks overlayfs (kernel, then fuse-overlayfs) on Linux, `clonefile` on macOS and ProjFS on Windows, then a git worktree, then a copy, and says in the status line which one ran and why. Overlay hides secret files with pre-created whiteouts; dependency directories stay visible and are never merged back. ProjFS is compile-checked only.
- `message` tool: the parent and live workers exchange short messages (`to`: a worker id or label, `parent`, or `all`), delivered at the recipient's next step with bounded mailboxes. The Workers panel shows recent traffic; replaces the `task` action `message`.
- Word completion in the composer: a dim ghost finishes the word you are typing from your past prompts and this project's identifiers and file names; Tab or Right accepts (`ui.word_completion`). Prompts that look like secrets are never learned.
- `judge` model role (falls back to `small`, then main) for cheap typed decisions, with a 30-day local answer cache. `thinking = "auto"` (`--thinking auto`, `/effort auto`, Settings) judges each user turn's reasoning effort once; judge calls appear in session usage. The judge can be the Laya decision engine (Alt+M → Roles → judge → “laya/decision checkpoint”). It uses the decision role's checkpoint through Laya's typed `choice` API.
- `read` opens `.epub` books in reading order, with `[chapter N: title]` markers and a `:chapter=N` selector.
- Benchmark reports record each failed case's redacted reason (`evidence.agent_error_detail`) and flag runs in which no case reached the model (`invalid_run`); such runs exit non-zero, cannot serve as baselines and are never recommended by machine ranking.
- Directory reads list directories first, then files, by name, and name ignored/generated directories without expanding them; complete `glob`/`grep` walks report a one-line coverage footer.
- `memory_update` and `ast_search` are loadable on demand instead of offered on every request (about 2 KB less schema per request).
- Interface pass: brand gradient tokens (`brand.start`, `brand.mid`, `brand.end`; themes without them derive one from their palette); a welcome banner with a gradient wordmark, a readiness card (model, project and branch, approval and sandbox) and numbered starters; a working shimmer and pulsing spinner; outcome marks and timing on every turn's closing rule (`ui.show_turn_stats` now defaults on); reasoning shown by default; rounded frames and `◆ Title` panel titles everywhere; `/help` and the action palette share ten command categories, with Maintenance and Learning folded until `/help all`.
- Workers panel and live worker card: a roles line that groups roles following the main model, a running/total count, and one row per worker with its task, steps and elapsed time. The transcript view shows a name and model rule and marks successes and failures; internal IDs and raw tool-argument JSON are gone, and a worker with no current tool reads "thinking".
- Command results read as finished panels: background commands (`/usage`, `/images`, `/update`, `/setup` installs and others) fill one panel in place instead of printing "Started" and each line separately; `/setup` groups installed and missing capabilities in aligned columns; `/computer` and `/session` print labeled rows; `/debug` separates what runs from what Dial lacks; `/policy` and `/experience` say on and off instead of true and false.
- Bare commands explain themselves: `/search` and `/outcome` show usage with an example, `/todo` explains when tasks appear, `/effort` says how to rescan reasoning levels, and bare `/wt` lists worktrees (creating one, which moves the session, now needs `/wt create`).
- `/plan-yolo` with no `small` role hands execution to the main model and says so; naming a role with no model now says how to assign one.
- Clarifying questions open as a framed panel with radio marks; the question card shows the question and the chosen answer instead of JSON. `ask` is offered directly when enabled.
- Approval prompts, the session picker and the question dialog use the shared panel title and `›` pointer; the plan card lines up with other tool cards.
- Providers list the ones you can use first; providers you have not set up and local servers that are not running read as neutral states instead of warnings and errors.
- A failing model request explains itself once per turn instead of adding a notice for every retry attempt.
- `/context`, `/approval` and `/permissions` say what the value means and how to change it; settings rows show a source badge only when a value is overridden; Workers and Processes titles count in words.
- The light Dial theme's rules and frames reach 3:1 contrast against white.
- In-session commands for features that were command-line only: `/advisor`, `/external-thinking` and `/plan-yolo` toggle the `--advisor`, `--external-thinking` and `--plan-yolo[-into]` modes for the running process; `/gc`, `/anonymize`, `/images`, `/update`, `/benchmark` and `/schedule run` run the matching `dial` subcommand as a cancellable background job with its output in the transcript; `/schedule list|add|remove|log` edits the project's schedules; `/relay start|stop|status` runs a local collaboration relay (stopped when dial exits); `/hooks` lists configured lifecycle hooks.
- `--approval-mode write` maps to the `write` approval tier instead of being rejected.
- Background shell jobs: `bash` accepts `background: true`, and a command still running after `[shell] auto_background_secs` (default now 15; `0` disables) moves to a background job on its own. Completions reach the model as a transient message in its next request. A turn that would end while `bash`-started jobs still run waits for them (up to 30 minutes) instead of ending early. `bg` is always offered with `bash`.
- Worker hub (Alt+A, `/agents`, double-Left): a split roster and inspector, with the inspector behind Tab on terminals under 96 columns. Each roster row shows status, name, parent, unread messages, role/model, task and metrics (cost, time, requests, tools, tokens, context, last activity). The order stays stable while the hub is open, there is a flat or by-parent tree view (`t`) and a fuzzy filter (`/`). The inspector shows task, current tool and retry, usage, lineage, outputs and isolation, and recent activity. An Activity tab (`2`) lists responses, tool calls, errors, messages and lifecycle events across workers, with a kind filter (`f`), scope (`s`), follow (Space) and search. You can message a running worker, resume a finished one directly with a prompt (`m`), and cancel with `x` pressed twice; the mouse selects, opens and scrolls. Enter opens a full-screen live transcript of the worker, drawn with the main view's tool cards and reasoning blocks (Ctrl+O expands them), with scrolling, follow-at-bottom and a message/continue box. Alt+Shift+A opens the most recently active worker and cycles on repeat. A pinned Workers block above the composer (`ui.pinned_workers`, `ui.worker_live_preview`) and a `2/3 workers · $0.14` status segment show live workers, and a short notice says when one finishes while the hub is closed. Each child's conversation, usage and outputs persist under the session's state directory. `ui.worker_shards` migrates to `ui.pinned_workers`.
- Local sampling configuration and a model settings UI (`dadb48b`).
- Settings handling enhancements and a question dialog (`ae940c4`).
- Notebook and editing functionality with detailed tool descriptions (`066b6a3`).
- `dial benchmark --ablate-assistance`: runs `raw`, `dial` and per-feature ablations on one suite and prints correctness, tool-error, malformed-call, token and wall-time deltas; `[[configurations]].assistance` overrides in machine profiles.
- Constrained tool-call repair for LM Studio (`response_format` json_schema) and Ollama (`format`): one schema-constrained retry after a malformed local tool call (`assistance.constrained_repair`, `capabilities.structured_output`).
- MLX tool-call grammar covers the gemma4, function_gemma, glm47 and mistral tool formats in addition to JSON `<tool_call>` templates.
- Opt-in learning from other models' verified work into local LoRA data (`training.learn_from`, `learn_from_all_verified`, `learn_from_max_percent`).
- `[local_runtime]` MLX speculative decoding (`draft_model`, `num_draft_tokens`) and KV-cache quantization (`kv_bits`, `kv_group_size`, `quantized_kv_start`), per model profile and per benchmark configuration.
- Automatic escalation to the `stronger` role after repeated failed rounds, under the escalate tool's approval decision (`escalation.auto_after_failed_rounds`, `max_auto_per_turn`).
- `dial models recommend`: RAM-aware fit and context estimates for downloaded MLX models.
- Portable adapter and steering packs: `dial improve export|import|evaluate`, `dial steering export|import|promote`; imports never activate without local evaluation.
- Harder gate-only task families and adaptive primary-suite tiers so strong baselines are not at the ceiling.
- `assistance.tool_grammar` and `assistance.repair_hints` toggles.
- Passing behavioral checks on a clean turn (every behavioral check in the turn passed after the last edit) record a `checks:behavioral` outcome of 100, so such turns count as verified live turns; `/outcome` and `/undo` replace it.
- `/improve` reports the verified live turn count and names the gate blocking the next background cycle (pause, background off, cloud `max_daily_usd = 0`, cooldown, turn count, resources).
- Protocol live voice sessions: `live_start`, `live_mute`, `live_stop` and `live_listen` over the RPC control channel (not ACP). While a session is active, `send` text gets its instructions and finished answers are read aloud; speech failures arrive as `live_error`. Documented in `docs/voice.md`.
- MCP legacy HTTP+SSE transport (`transport = "sse"`): the event stream opens before `initialize`, announces the POST endpoint (same origin only), and carries every reply; a lost stream reconnects like other transports.
- MCP server `instructions` are added to the system prompt, each truncated to 2,000 characters (8,000 in total) and labeled as untrusted third-party text.
- `decision` is the Laya checkpoint: assign it in Alt+M → Roles (local path or HF repo id) or with `dial laya install --checkpoint`; the managed Laya service restarts when the checkpoint changes. A legacy chat-model `decision` role migrates to `memory`, and sharpshooter decision memory uses the `memory` role.
- Alt+M → Roles shows each role's effective model and its source (assigned, falls back to main, Laya checkpoint, or not set), with a line on what uses it. Background calls note which role ran.
- Reviewer subagents and review workflows use the `tester` role; the `planner` subagent and plan mode use `planner`; `security-reviewer` is no longer a role mapping, since no such subagent exists.
- Embedding models are detected from checkpoint metadata (sentence-transformers files, `architectures`, `model_type`), not from names. Causal and conditional-generation models are never offered as embedding models.
- The `codebase.architecture_model` default that follows the main model is shown as "architecture setting" in Alt+M → Roles, not "Assigned".

### Changed
- README.md is a short index; its detailed sections moved into `docs/` (new: `benchmarks.md`, `configuration.md`, `improvement.md`, `terminal-interface.md`, `tools.md`). `src/agent/mod.rs` is split into focused submodules with no behavior change.
- Built-in and extension tool calls run on a helper thread while the UI keeps painting and taking input, so a slow `grep`, LSP query, web fetch or MCP call no longer freezes typing and scrolling. The agent itself still runs on the UI thread; moving it is separate work.
- The default roster drops `forget`, `memory_history`, `memory_import`, `experience` and `edit_preview` from every request; they stay in the catalog and load on demand like the other specialists.
- First run opens provider setup (sign in or add an API key for a hosted model) before the local model list; Esc switches to the local list.
- Long error chains in the transcript are shortened to what failed, the immediate reason and the root cause ("A: B … root"); short messages are untouched.
- New default approval mode `write`: workspace edits, file writes and non-destructive shell commands run without a prompt; recursive deletes, history-rewriting git, privilege escalation, piping downloads into a shell, and tools that leave the workspace (network, browser, MCP) still ask. `ask` keeps the old behavior; Shift+Tab cycles write → auto → readonly → ask. The approval prompt gained "Always allow in this project", which saves the rule to the project's `permissions.toml`.
- Progress guards no longer end the turn on first trigger. A repeated observation, a tool failing in a row or a stalled todo now resumes the model with a continue notice up to `reliability.auto_continue_rounds` times (default 2) before the "Recovery needs direction" prompt appears.
- Recovery hints are appended to a tool error only once the same tool has failed twice in a row; the first error stands on its own.
- Internal bookkeeping no longer lands in the transcript: memory and repair lookups that are unavailable, compaction scheduling, architecture maintenance, tool-recovery capture, version switches and the per-turn "Project experience is unavailable" line are diagnostics now (`/policy` and `/experience` still show them).
- Reasoning text is shown by default (`ui.show_thinking`); Ctrl-T style toggle unchanged.
- Settings under the Improvement tab and the Laya, Project understanding, Adaptive policy and Decision memory sections are hidden behind "Show advanced"; the harness commands (`/reliability`, `/policy`, `/steering`, `/experience`, `/understanding`, `/laya`, `/improve`, `/outcome`, `/autoresearch`, `/compute`) list after the coding commands.
- The welcome screen points to `/providers` for a hosted model first; `/setup` is named as the local MLX installer.
- The default tool offer is the core roster plus the essential integrations when configured (`find`, `lsp`, `bg`, `git`, `web_fetch`, `web_search`, `ast_search`). Every other catalog tool (browser, debug adapters, kernels, archives, MCP servers) is listed in the system prompt with a one-line description, so the model loads it with `tool_load` in one round instead of searching. Offering the whole catalog was measured at 55 schemas and 94 KB per request, 15 of them per-adapter `dap_*` copies. `assistance.compact_tools=true` remains the bounded discovery mode for small local models.
- Tool results are shown whole up to the per-model limit, now 48 KiB (was 24 KiB, with successful inspections cut to 8 KiB and `efficiency.tool_preview_bytes` 4096 on top). `read` and `grep` pages grew to 48 KiB. `efficiency.tool_preview_bytes` defaults to 0 (no extra cap).
- The system prompt uses short MUST/NEVER rules about engineering, tools, workflow, verification and delivery, with the harness-mechanics paragraph (tool search, tool loading, artifact paging) removed. About 15% shorter.
- Per-turn ambient context is opt-in: `codebase.ambient_context` (saved architecture notes and outlines, default off; `codebase_memory` remains callable), `assistance.repair_hints` (verified repair patterns and matching past repairs, default off) and `efficiency.task_packet` (the transient task evidence packet, default off). Laya advice was already opt-in.
- Harness-initiated verification is opt-in: `verification.automatic_checks` (default off) controls the behavioral baseline before edits, focused checks at `finish` and after the turn, and the automatic repair rounds. `verification.enabled` still records checks the model runs and gates `finish` on failed, unrepaired checks; the prompt requires a smoke run before yielding.
- History stays byte-stable until compaction: `compaction.supersede_reads` and `compaction.drop_useless` default to off, so older tool results are not rewritten under context pressure.
- `reliability.measured_interfaces` defaults to off: one-call-at-a-time mode and the per-request contract restatement only apply when turned on for a model that cannot batch calls.
- `codebase.synthesize_architecture` defaults to off; `/understanding build` runs it on demand, so a completed turn returns without a local summarization call.
- Claude requests think by default: a profile that leaves `thinking` unset gets the catalog's documented mode for that exact model (adaptive, or a budget of half the output allowance between 1,024 and 16,000 tokens) on the Anthropic API and Bedrock; temperature and top_p are then omitted. `thinking = false` keeps it off.
- `bash` runs in the project root by default; use `cwd` for subdirectories instead of prefixing `cd <root> &&`.
- Repeated `read` of an unchanged range returns a short pointer to the earlier result while that result is still shown in full, instead of the same text again or a withheld error. `read` takes `paths` to read up to 16 targets in one call.
- Prompt tool-use guidance asks the model to batch independent reads, searches and checks into one response.
- `todo` accepts the shapes models send: `op` is inferred from `tasks` (set on an empty plan, append otherwise); `findings` is an alias for `finding`; unknown keys fail with a corrected call; `start` without an index starts the first pending task, and `done`/`drop` without one target the single open task when unambiguous.
- Hashline target errors include a concrete example (`PUT 12.=12:` plus `+` rows).
- Finish refusal for open todos names the exact `todo` calls to batch before `finish`; finish is refused while `bash` background jobs run.
- Syntax advisories no longer parse `.zsh`/zsh rc files with the Bash grammar; zsh is reported unsupported, so it cannot produce false parser errors.
- `[improve] background` defaults to `true`, matching the documentation. Cloud spend stays off until `max_daily_usd > 0`.
- `[budget]` documentation: `0` (the default) or unset disables the cap.
- `dial config list/get` show legacy model-role and memory keys in their migrated form, matching what the loader uses.
- Post-turn focused verification now records its checks as run, so a turn whose checks pass after the last edit is no longer reported as edited but unchecked.
- Token-efficiency work (`fe17c89`).
- Guards and prompts updated (`c339c09`, `f6a7ac1`, `64ced40`).
- Documentation describes verification, recovery, favorites, and runtime controls (`01a1d0f`).
- Model roles: `main`, `small`, `coach`, `tester`, `planner`, `memory`, `decision` and `embedding`. Unassigned roles use `main`; `decision` falls back to `memory` first, and `embedding` has no chat fallback. The former `stronger` escalation role is now `coach`. Legacy `[model_roles]` keys migrate automatically on load (legacy `memory` now maps to `memory`, not `small`; the old `escalation.role` setting is removed); `--smol` now sets `small` and `--slow` sets `coach`.
- Model-calling memory (`local` or `sharpshooter`) requires an explicit model role. Missing assignments now leave memory inactive with a setup error instead of preventing startup, so Alt+M → Roles remains reachable. Restart after assigning the roles to activate memory; no backend or chat-model fallback is silently selected.
- Codex WebSocket write disconnections (including broken pipes on cached sockets) now follow incomplete-stream recovery: the failed attempt is discarded and subsequent retries use SSE, without executing partial tool calls or replaying inline after callbacks.
- Delegation is on by default: `task` and `skill` are offered without a complexity gate (explicit profile allowlists still win) and survive tool compaction. The system prompt is rebuilt when the offered tools or policy change.
- `task` calls take a shared batch `context` and per-item `task`, `solution_space`, and optional `skills`; the `prompt` field is removed. Each child receives its role prompt, the parent goal, open todos, bounded working state, the shared context, its assignment and preloaded skill bodies.
- A user message that shares words with a skill's name or description gets a one-line `skill` hint; reviewers can load skills.
- Subagent results report each child's model role and model.
- No investigation planning gate: after a fixed number of unplanned reads/searches (4–10), the next request no longer withholds every tool except `todo`/`finish`. The opt-in `reasoning_guard.require_plan` gate is unchanged.
- No end-of-turn review round: finishing after edits or non-allowlisted shell commands (`cargo test`, builds) no longer forces another model request with the diff or `git status/diff` instructions, and `finish` is no longer bounced for a pending review. Changed files are listed for the user when the turn ends; configured focused verification still gates `finish`.
- The visual-inspection reminder no longer forces a round; a missing inspection is still disclosed in the final answer.
- The post-edit Laya evidence review that blocked the end of each editing turn is removed; Laya still suggests a retrieval scope before each prompt.
- The per-request `[Current executable tool contract]` message is sent only in single-call mode or after a tool-contract failure; the offered schemas carry the contract otherwise.
- Harness notes added to one request (todo/recovery reminders, background notices, loop redirects, contract guidance) are merged into a single message.

### Removed
- Implementations that ran on another harness's packages or services were replaced by Dial's own (see Added): the Cursor runtime bridge, the sidecar memory backend with its `memory_edit` tool and local embedding server, the hosted skill registry, and the external browser relay. Detection of an externally shared Laya service and that harness's skill roots are gone. Wire and on-disk identity is Dial's own: the ChatGPT/Codex `originator`, the Z.AI key name, LSP `hostInfo`, security fingerprints (`dial-security/v1`), ratchet state (`.dial/ratchet`), X11 MPX device names, kernel prelude globals, portal state, and `DIAL_*` environment overrides (`DIAL_BROWSER_TERN`, `DIAL_BROWSER_CMUX`, `DIAL_COMMIT_*`, `DIAL_GEMINI_CLI_VERSION`, `DIAL_ANTIGRAVITY_*`). Existing security findings get new fingerprints, and saved ratchet state under the old directory is not read.
- Unreachable code: the automatic reliability check (`reliability.auto_check`) and its background worker, the write-only subagent store (`agents.sqlite3`), the composer's unused Vim layer, the unused syntax-highlighted diff renderer (dropping the `syntect` and `two-face` dependencies), the sshfs helpers, MCP manual reconnect, plugin auto-update, and other dead functions, fields and re-exports. Saved configuration that still contains removed keys is migrated on load.
- Duplicated hex encoders were consolidated into one; MIT attribution for adapted code now lives in `THIRD_PARTY_NOTICES`, which release archives, the Docker image and the Homebrew formula ship instead of two separate license copies.

### Fixed
- Windows install is one PowerShell command: `scripts/install.ps1` runs from `irm … | iex`, and from `gh release download … | Out-String | iex` while the repository is private. It works in Windows PowerShell 5.1 and PowerShell 7 with no OpenSSL, verifies the signed `SHA256SUMS` with a built-in Ed25519 check, adds Dial to the PATH of the user and the current session, and runs the x64 build on ARM64 under emulation. Releases now publish `install.ps1` with the signing key embedded. The installer is tested on Windows in CI and against the real archive at release time. The Windows `bash` tool description now says commands run in bash (Git for Windows or the built-in shell) unless `shell.backend = "powershell"`.
- `install.sh` works on stock macOS: when the `openssl` on PATH is LibreSSL (no Ed25519 `pkeyutl -rawin`), it uses a Homebrew OpenSSL 3 and otherwise says to `brew install openssl@3`. Its unsupported-platform message points native Windows users to `install.ps1`. The Windows installer CI job no longer fails as an invalid workflow (`shell` cannot read the matrix; it is set through `defaults.run`).
- File walks and the codebase index skip nested linked git worktrees, so a worktree checked out inside the project is no longer indexed as project source; saved understanding that cites a deleted file is now stale (`source_status: "missing"`) instead of "unchecked".
- Background PTY output retries temporary nonblocking-read unavailability instead of failing capture and terminating the job.
- Clearing the `decision` model role in project scope preserves its inheritance marker instead of migrating it to `memory` and leaving the original assignment active. Role-clear feedback reports when an assignment in another scope is still effective.
- Roles that fall back to the main model explicitly show `unassigned`, so clearing an assignment remains visible even when the fallback uses the same model.
- Ordinary coding output stays quiet, external settings refreshes preserve launch model controls, and terminal changed-file summaries appear once in the review footer. `--thinking off` works without manual profile setup on Anthropic and OpenRouter.
- Remote model streams (Anthropic, ChatGPT, OpenAI-compatible hosts) now default to a 300 s limit on both the first-event wait and silence between events (`remote_model_idle_timeout_secs`; previously unlimited, so a stalled stream hung with no output). A stall fails as a transient error and retries under `retry.max_attempts`. Local model servers on localhost or the LAN stay unlimited, and `0` opts out. Non-streaming remote calls are capped at the same limit.
- ChatGPT/Codex requests always send `prompt_cache_key` for the session when one exists, unless the profile sets `prompt_cache = false`. Previously the key was gated on the profile's `capabilities.prompt_cache` flag, so subscription requests never sent it. The API-key OpenAI path is unchanged.
- Runtime recovery, project verification, and model favorites (`48a2aa1`).
- Gate evaluation retries transient infrastructure failures (429/5xx, timeouts, model load) and excludes persistent ones symmetrically, reporting the category instead of aborting the whole evaluation.
- Feedback proposals tolerate non-JSON replies from local models (lenient parse, one stricter retry, configured `feedback` role).
- Manual compaction with nothing to cut is a successful no-op (`compacted: false`, "nothing to compact yet") instead of an error; automatic overflow compaction still fails when the request cannot fit.
- A single oversized turn picks the harder cut before the first summary pass, so summarization no longer exhausts its step budget.
- LM Studio models backed by an on-disk MLX checkpoint report how to switch to the `mlx` provider for training.
- Documentation and validation no longer claim Dial-owned MLX lacks constrained tool decoding.
- Agent loop: compaction retries keep the tool list; a failed verification repair round no longer leaves the backend unusable; custom checks without extensions run for shell-only edits; `finish` is refused when live-task state cannot be read; the `stop` hook's block verdict now continues the turn (bounded); `/session move` rebinds checkpoints to the new project; fallback models are tried in order when one cannot be built.
- Telemetry and the stats ledger are now recorded (requests, failures, tools, turns) and flushed on exit; `foreign_sources` is honoured.
- Providers: 429 rate-limit bodies no longer trigger compaction; Gemini/Cloud Code `RESOURCE_EXHAUSTED` throttles with a retry delay are retried; account rotation on the final attempt no longer panics; Anthropic no-argument tool calls are no longer treated as malformed; stateless Cursor replay, GitLab Duo token errors, Anthropic OAuth callback on macOS, GCE metadata ADC and `CLOUDSDK_CONFIG` fixed.
- Tools: MCP servers without tools connect; MCP DELETE carries the OAuth token; short-lived MCP tokens are used; background PTY jobs report the rendered screen and `/bg` opens the jobs panel; large plugin responses no longer stall; LSP renames route to matching servers with annotations; git output parsing keeps stderr out of data and handles quoted paths; idle `cancel` no longer poisons the next kernel/eval cell; new files get umask permissions; edit engines handle `--`/`++` body lines, overlapping matches and diff context prefixes.
- Storage and CLI: `dial -c`, `share` and `anonymize` pick the newest session even with pins; huge retention/gc values no longer wrap; torn training-data lines no longer block harvest; schedule state is written atomically; `/setup providers`, LSP dependency settings and workspace command help work; the git panel works from repository subdirectories and stages CRLF hunks byte-exactly.
- UI: Alt+] (next favorite model) works; Ctrl/Alt chords match with Caps Lock; worker-thread panics no longer tear down the terminal; streamed text is replaced correctly when the final text differs.
- Setup and distribution: Hindsight no longer reports ready just because Docker is installed; cancelled or timed-out setup steps stop their whole process group (npm/uv/brew children); `dial update` accepts release archives over ureq's 10 MB body cap and bounds its network requests with timeouts; the release smoke checks, the Homebrew formula checksum updater (each archive now updates its own digest) and both installer version checks match the version token of `dial --version`.
- Sessions: `dial -c` skips empty sessions; `/new`, `/clear`, `/resume`, `/fork`, `/delete` and `/move` reset plan mode, stop `/loop`, use the live project and model, and swap per-session modes, `/switch` selection and cost; `/delete` and `/move` no longer leave partial state; torn session tails no longer block move/fork/import; the picker and RPC prefix lookups no longer parse every session.
- Approvals: bash permission rules match through absolute paths, `env`/assignments, wrappers, `sh -c` bodies and git global options; "always allow" never stores a grant broader than the approved command; `/memory clear` requires approval; `post_tool_use` no longer fires for denied calls.
- Providers and transports: unknown stream events/deltas no longer abort replies; in-stream rate-limit and overload errors retry; Anthropic `end_turn` maps to `stop` and tool errors are sent as errors; `max_output_tokens` truncation returns the partial answer; streamed bodies use the 64 MiB stream cap; Codex websocket falls back to SSE only for unsupported upgrades; OAuth-only providers (Copilot, Cursor, GitLab Duo, Gemini CLI, Antigravity, xAI) are listed and selectable after sign-in; ChatGPT sign-in works off macOS and keeps rotated refresh tokens.
- Tools: legacy MCP `sse` transport works and server instructions reach the prompt; installed plugins actually run; `session_start`/`session_end` hooks no longer fire on settings rebuilds; LSP `start_on_write`, diagnostics for unopened files and format-on-write work; LSP file renames are checkpointed; mixed-line-ending files keep untouched lines' endings; web pages in legacy and UTF-16 charsets decode correctly; browser `aria`/`audit`/`trace_*` actions and attach-only mode work; `/bg` jobs keep output printed before adoption; `dial commit --dry-run` leaves the index unchanged and `[git] bypass_hooks` applies to `/git` and `dial commit`.
- Other: `[compaction] supersede_reads`/`drop_useless` take effect; `ui.history_limit` applies live; `dial config set` validates values; scheduled watches fire across `dial schedule run --once`; `dial -p` no longer spawns background workers or blocks on `ask`; the collab web viewer joins rooms; live voice RPC methods work and stopping speech is not an error.
- Strict tool schemas: optional arguments sent as `null` are treated as absent by every tool (finish, ask, task, todo, edit, kernel/eval, gh, artifact_read and others); `working_state` updates are validated.
- Turn lifecycle: Blocked and Incomplete turns list the files they changed; `session_end` reports the real session on quit and fires on session swaps; session-scoped "always allow" grants, trajectories and plan state no longer leak into the next session, and an approved plan survives a settings rebuild; Ctrl-C at an idle prompt no longer cancels the next prompt; Ctrl-Z suspends the TUI; collab guest aborts interrupt a running turn.
- Cancellation: Esc stops `wait` on subagents, `@diff` expansion, `/cleanse` checks, `/loop` conditions (which now run off the UI thread), MCP reconnects and gate retry backoffs.
- Processes and blocking: hung git/gh/sshfs/terminal helpers, plugins, MCP servers and readers that leave grandchildren holding pipes are bounded and reaped; `/collab start`, Smithery search/install/login and the live adapter monitor run as background jobs; killing background jobs no longer stalls the UI.
- Persistence: child transcripts are private (0600); rule, changelog, runtime, ratchet and image-blob writes are atomic; `dial commit --dry-run` restores the index atomically and reports a failed restore; session pins are locked across processes.
- Other: `[tools.<name>] parameters` reach the model and the program; `shell.bash_timeout_secs = 0` means no deadline; a failing focused check skipped by an interrupt stays a repair obligation; harness candidates on an adapter-bearing version pass integrity checks; `git.bypass_hooks` no longer disables signing for user-initiated commits; per-project settings may define hooks and extensions again (they live outside the repository).

## [0.1.3] - 2026-10-07

### Fixed
- Native calibration respects the opt-out; solve verification fails closed (`38171cb`).

## [0.1.2] - 2026-10-07

### Added
- Windows installer and release packaging with native runtime checks (`647f3dc`).
- Native operating-system boundaries ported for Windows (`55eceaf`).

### Changed
- Windows ACLs applied through the existing security handles (`b88d6e3`).

### Fixed
- Native Windows paths, file-lock contention, and test contracts (`48a60f4`, `6919f63`, `61ab408`).
- Native descendant PIDs and deterministic CI fixtures (`4471249`).

## [0.1.1] - 2026-10-07

### Fixed
- Intel Homebrew command lookup (`4afa9af`).
- Reset sockets recognized in the cancellation closure test (`d94df84`).
- Release installer filename corrected in the usage example (`f01f616`).

## [0.1.0] - 2026-10-07

### Added
- Steering, adaptive scheduling, git and syntax tools, and compute controls (`198734a`).
- LICENSE, CI, and ARCHITECTURE.md (`e2dd00f`).
- Mac and Windows-WSL installation and release builds (`5c0d9c4`).
- Authenticated installation from private releases (`a267515`).

### Changed
- Live steering prioritized and repeated agent work guarded (`ac4fb95`).
- Clippy autofixes applied (`21c514c`).
