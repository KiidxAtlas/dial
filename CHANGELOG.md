# Changelog

Notable changes to Dial, grouped by version, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Release attribution uses local release-tag trees from `v0.1.4` through `v0.2.6`; dates use the tag's calendar date. Tags establish source contents, not successful publication or live-provider compatibility. Older untagged notes are explicitly marked below. Unreleased describes the current checkout, not an installed release. Later entries supersede earlier defaults and behavior; optional integrations still require their documented prerequisites and approvals.

**Capability guide — current checkout, not first-release attribution.** The entries below track changes rather than repeat every unchanged feature. These guides cover Dial's main capability areas, including features inherited by the earliest available release tag:

- **Models and inference:** hosted and local providers, API-key/subscription authentication, model discovery/downloads, role routing, thinking, fallback and cache controls ([providers](docs/provider-catalog.md), [downloads](docs/model-downloads.md), [routing](docs/model-routing.md), [transport](docs/llm-transport.md)).
- **Coding and verification:** bounded file/search/AST tools, editing grammars and notebooks, Git/jj/GitHub/GitLab workflows, LSP, debugger adapters, checkpoints/undo, focused checks and requirement evidence ([tools](docs/tools.md), [verification](docs/requirement-verification.md)).
- **Execution and access:** foreground/background shell jobs, persistent JavaScript/Python evaluation, web search/fetch, Chromium and native desktop control, approved SSH and added-directory access; platform and sandbox limits apply ([execution/web](docs/execution-and-web.md), [shell](docs/shell.md), [SSH](docs/ssh.md), [directory access](docs/directory-access.md)).
- **Sessions and terminal UI:** streaming conversations, command palette, settings/keymaps/themes, branches/resume/export/portable archives, side conversations, live work dashboard and voice ([terminal](docs/terminal-interface.md), [composer](docs/composer.md), [voice](docs/voice.md)).
- **Delegation and collaboration:** role-scoped workers, isolated copies/pools, peer coordination and encrypted host/guest collaboration ([subagents](docs/subagents.md), [peers](docs/session-coordination.md), [collaboration](docs/collab.md)).
- **Extensibility and clients:** MCP discovery/OAuth/tools/prompts/resources, lifecycle hooks, skill packages, the Rust embedding SDK, print mode, RPC and ACP ([extensions](docs/extensibility.md), [skills](docs/skills.md), [protocols](docs/rpc-and-acp.md)).
- **Memory and improvement:** persistent project/user memory, saved source understanding and change history, explicit local learning, evaluation gates, benchmarks and autoresearch experiments ([memory](docs/memory-and-learning.md), [improvement](docs/improvement.md), [benchmarks](docs/benchmarks.md), [autoresearch/archive](docs/autoresearch-and-archive.md)).
- **Media and investigations:** image generation/editing/search/grounding, documents/audio/data/video, retained citations, research/timelines, offline runtime/HAR/SQLite investigations and approved monitoring ([images](docs/image-generation.md), [vision](docs/model-vision.md), [evidence](docs/evidence-investigation.md), [system investigation](docs/system-investigation.md), [monitoring](docs/application-monitoring.md)).
- **Operations and distribution:** setup/doctor, runtime maintenance, schedules/background services, redacted encrypted sharing, local statistics/optional telemetry, signed release installers and updates ([setup](docs/setup.md), [services](docs/background-services.md), [sharing/statistics](docs/share-stats-telemetry.md), [releases](docs/release-and-install.md)).

## [Unreleased]

### Security

- Updated vulnerable dependencies flagged by Dependabot. `zip` 9.0.4 → 9.0.5 fixes a critical archive-header parsing flaw that could lead to arbitrary file overwrite. In the MLX worker environment (`python/uv.lock`): Pillow 12.3.0 → 13.0.0, aiohttp 3.14.3 → 3.14.5, filelock 4.0.8 → 4.2.10 and certifi 2026.7.22 → 2026.10.23, which fix image-decoder memory-safety bugs, HTTP parsing issues, lock-file races and a compromised CA root.

## [0.3.3] - 2026-11-09

- Windows: replacing or deleting Dial's private files (sessions, settings, state) no longer fails with "access is denied" or "being used by another process" while another process, such as a second Dial window or a reader, has the file open. Renames and deletes now use POSIX semantics on NTFS/ReFS, and fall back to the old calls on filesystems without them. A sharing violation from a brief scan by antivirus, indexing or sync tools is retried for about 0.3 s before it is reported (`docs/windows.md`).
- Windows: `write` and `edit` no longer fail with `opening workspace ...: The process cannot access the file because it is being used by another process. (os error 32)` when the workspace folder is the current directory of a shell or other program. Dial no longer asks for delete access on directories it writes into; it never deleted those directories through that handle.
- `write` and `edit` accept an absolute path that names the workspace by a different spelling of its root, such as `C:\Users\...` against Dial's `\\?\C:\Users\...` root on Windows or a symlinked parent on macOS/Linux. Before, these failed with `native mutation must declare canonical workspace targets`.
- The `/watch` pane's **Peers** tab is now **Chats**: a text-message style conversation per peer,
  labelled by model, with grouped bubbles, ages, read receipts and an inline composer, plus a
  **This session** thread that prompts this session's own model. Peer messages wake idle, finished,
  blocked, failed and interrupted sessions and start a session with no task yet, with eight
  wake-ups per user turn at least 5 seconds apart; a message typed in the pane refills the budget
  ([terminal integration](docs/terminal-integration.md),
  [session coordination](docs/session-coordination.md)).
- Claude models newer than the bundled catalog no longer need a `models.json` update before
  thinking works. Dial reads the thinking mode and effort levels Anthropic's model listing declares
  (`capabilities.thinking`, `capabilities.effort`), saves them with the discovered models, and uses
  them for default thinking, `/effort` and the status line when the bundle has no record
  ([provider catalog](docs/provider-catalog.md)).
- The default status line now shows the thinking level at every terminal width, and shows
  `think auto` while `/effort auto` judges effort per turn
  ([terminal composer](docs/terminal-composer.md)).
- New opt-in `shell.undo_snapshot` (off by default): Dial copies the project's source files before
  a turn's first shell command that may change files, and files `bash` then changes, creates or
  deletes are restored by `/undo` and `/rollback` like tool edits. Ignored files, dependency and
  build directories, credential-like files and effects outside the project are not covered;
  projects over 20,000 source files or 512 MiB are not copied. The effect on model behaviour is
  unmeasured ([shell](docs/shell.md#undo-for-shell-changes)).
- `tool_search` no longer returns an empty `search_matches` when no tool matches most of a broad
  query (for example a list of synonyms). It now lists up to five closest partial matches, not
  loaded, beside the catalog page, so a model can find and `tool_load` the capability it asked
  about. Automatic routing keeps the strict threshold. The `computer` readiness note now says that
  only `locate` needs a grounding model ([tools](docs/tools.md#tool-roster-safety-and-context-sources)).
- A `computer` `run` whose `code` is function source (for example `async ({ desktop }) => …`) now
  fails with a message to pass it as `fn` or write a function body. Before, nothing ran and the
  result was the function's source text, which models misread as a broken tool.
- On Windows, Dial now turns on virtual terminal processing before drawing its interface. Before,
  the classic console window (`powershell.exe`, `cmd.exe`) printed raw codes such as `←[2K` in place
  of the interface. A console that cannot enable it ("Use legacy console") now gets a message to
  use Windows Terminal ([Windows](docs/windows.md#terminal-and-administrator-shells)).
- Local MLX vision models such as Gemma 4 now reuse their prompt cache between turns. The vision
  runner kept one exact snapshot within 1 GiB. That snapshot ended in the chat template's generation
  prompt, which the next turn renders differently, so it never matched and every request
  re-read the whole prompt. It now keeps four checkpoints within a quarter of physical memory. On
  Gemma 4 26B-A4B with a 36–50k-token agent conversation, time to first token fell from 25–40 s to
  3–4.5 s, with about 93% of prompt tokens cached ([transport](docs/llm-transport.md)).
- Clarify that Windows releases need no Azure account or paid code-signing certificate. Installers
  retain signed-checksum verification; unsigned executables can still receive Windows publisher
  and reputation warnings ([Windows installation](docs/windows.md#publisher-signatures-and-download-warnings)).
- Windows: when Smart App Control or another application control policy stops the downloaded
  `dial.exe` from starting, the PowerShell installer now says so and names Smart App Control when
  it is on, instead of reporting an empty "version check failed". Nothing is installed. Use WSL2 on
  machines with Smart App Control on ([Smart App Control](docs/windows.md#smart-app-control)).
- Repeated acceptance-helper failures keep fresh execution/status evidence but use a short notice
  when their bounded diagnostics are unchanged; changed failures and failures after a pass still
  print the diagnostic ([verification](docs/requirement-verification.md#model-registered-helper-diagnostics)).
  Python and release-policy test targets now fail early with the selected interpreter and a
  pinned-environment setup command when their required imports are unavailable
  ([source checks](docs/release-and-install.md#build-from-source)).
- Semantic memory starts Ollama for a local `embedding` role when it is not running, instead of
  falling back to keyword search. Dial windows share one `ollama serve` through a lock and leases,
  never start a second one, and stop it when the last Dial process exits; a server you started
  is never stopped. `/setup embeddings` now starts its server through the same owner. Turn it off
  with `memory.semantic.embedding_autostart = false`
  ([embeddings](docs/model-embeddings.md#starting-ollama-automatically)).
- Python edit diagnostics now catch literal newlines inside short quoted strings, a blind spot
  in the linked syntax grammar, while allowing triple quotes, escaped line continuations and
  multiline f-string expressions. Retry barriers retain bounded multiline compiler diagnostics
  instead of only their first line. Shell checks with explicit `check_paths` stay withheld after
  unrelated file changes and become eligible after their declared source changes
  ([agent loop](docs/agent-loop.md)).
- A model that keeps repeating one identical tool call is stopped sooner and sees the real
  error. Duplicate-call references now quote the last executed result instead of nesting earlier
  references or retry-barrier refusals, and repeating the call again after a reference ends the
  turn as blocked instead of running on to the 16-attempt recovery limit. A refused shell retry
  whose error points at the command line (`No module named …`, `command not found`) names the
  command to correct. Refused calls no longer inflate the report's "Outside /undo" shell count
  ([agent loop](docs/agent-loop.md)).
- One `evidence` tool replaces `evidence_search`, `workbench`, `investigate`, `reality`,
  `timeline` and `perception`, cutting six schemas from the catalog. Their operations are now `evidence` actions
  with the same bounds: `search`, `audit`/`lineage`/`correct`, `diff`/`interpret`/`plan`/`result`,
  `check`/`assess`, `timeline` and `budget_open`/`budget_status`/`budget_step`/`budget_close`.
  Saved `integrations` and `harness.tools` entries, `--tools`
  names and resumed sessions that used the old tools move to `evidence` automatically
  ([tools](docs/tools.md), [evidence investigations](docs/evidence-investigation.md)).
- `working_state` is now part of `todo`: the `state_view`, `state_update` and `state_patch` ops
  take the former `view`, `update` and `patch` fields in `state`, with the same revision checks,
  deliverable confirmations and compaction carry. One planning tool replaces two in the default
  offer, and `task` still fits the hosted core budget. Saved `harness.tools` lists, `--tools
  working_state` and resumed sessions that used the old tool move to `todo` automatically
  ([tools](docs/tools.md), [agent loop](docs/agent-loop.md)).
- Specialist commands are grouped under `dial dev` (`benchmark`, `edit-bench`, `improve`,
  `steering`, `weights`, `laya`, `policy`, `reliability`, `rules`, `anonymize`, `gc`), so
  `dial --help` lists everyday commands. The old top-level spellings still work and are hidden
  from help ([everyday](docs/everyday.md#command-line)).
- First launch names local inference servers that are already running (Ollama, LM Studio,
  llama.cpp and others) and highlights **Local model**, so a machine with a local server needs no
  API key to get started. When none is running on Apple Silicon, it suggests the largest MLX coding
  model (7B and up) that fits your memory instead. Nothing is selected or downloaded without your
  confirmation ([setup](docs/setup.md)).
- The Unix installer warns about bubblewrap only when it is missing, prints the install command for
  your package manager and shows the WSL interop note only on WSL. The documentation no longer says
  `bash` needs `sandbox.mode = "off"` on Linux; that applies to Windows only
  ([release and install](docs/release-and-install.md)).
- The install docs now give the Homebrew tap command, and `packaging/` holds submission-ready
  definitions for the AUR (`dial-bin`) and homebrew-core ([packaging](packaging/README.md)).
- Releases now update the AUR `dial-bin` metadata along with the Homebrew formula, re-verify the
  signed `SHA256SUMS` first, and merge the packaging pull request automatically, so the tap no
  longer lags behind a release ([release and install](docs/release-and-install.md)).
- New [Quickstart](docs/quickstart.md) page; the docs index starts with a "Start here" section.

## [0.3.2] - 2026-11-09

- Safer interaction with the agent. Approval and question prompts ignore answer keys for a moment
  after they appear, saving a project-wide grant asks for confirmation, and Ctrl+C at a prompt
  denies and stops the turn (Esc or `n` only deny). Keys typed just before a prompt are no longer
  replayed after the turn. The busy chooser picks with digits instead of plain letters, Ctrl+C with
  a draft clears it before stopping, the first Ctrl+C in a panel opened mid-turn only closes it,
  and Esc cancels a question. Approvals lead with the action; batch approvals list one line per
  change; plan approval reads "Review plan" and renders the plan as markdown
  ([composer](docs/composer.md), [shortcuts](docs/terminal-shortcuts.md)).
- Keys are consistent across panels: the command palette defaults to Alt+K/F1 so Ctrl+P/Ctrl+N
  browse history and move list selections everywhere; the session picker pins with Ctrl+B and the
  headers editor adds rows with Ctrl+O; pickers, dialogs and panel text fields follow remapped
  keys; every panel has F1/`?` key help; the newline hint shows Ctrl+J when Shift+Enter cannot be
  sent; the shortcuts panel records a chord after Tab and names `config.toml [keys]` as the save
  location.
- Transcript states are accurate: denied and stopped tool calls render muted instead of failed,
  progress follows each call, spinners animate on every card, failures show their last output
  lines, shell cards keep one shape with a live timer, and task cards settle when the call returns.
  Resumed sessions no longer show harness messages as user text, and pasted indentation is kept.
- Workers that end needing input or blocked are shown as finished and needing attention (and can
  be continued), stay pinned until opened, show their result in the hub, and are named "worker"
  throughout. The status line keeps one layout while busy, names what the model is doing, and keeps
  workers and inbox on narrow terminals; extra rows share a height budget at small sizes; the
  update notice unpins after five minutes. Panels share one selection, tab, footer and scroll style,
  and durations, counts and costs use one format ([subagents](docs/subagents.md),
  [terminal](docs/terminal-interface.md)).
- Peer send receipts clarify that queued messages were accepted into the recipient's inbox, not
  acknowledged or guaranteed to wake an idle session. Automatic terminal wake-up remains conditional
  on recipient settings and current eligibility; the receipt does not establish either.
- A failing check no longer ends the turn with `Specialist schemas are loaded only by tool_load`
  under the default compact tool offer. Diagnostic capability routing now leaves the stable offer
  unchanged instead of failing; specialists still load through `tool_load`.
- A compound shell command that exits zero after an earlier step printed a recognised failure (for
  example `step 2>&1; ls` where `step` was `not found`) now names that failure in a one-line note;
  the exit status is unchanged ([shell](docs/shell.md)).
- Tools accept more of the call shapes models actually send. Top-level `const`/`let`/`class` in a
  JavaScript eval cell can be redefined by a later or retried cell, and under Node a cell
  variable can shadow a builtin module name such as `fs` ([execution](docs/execution-and-web.md)).
  An object or array field sent as a JSON string is decoded, `verify` treats `targets: []` as
  omitted, `glob` accepts an absolute pattern under the workspace root, and a file reference
  copied with its `#TAG` names the same file ([tools](docs/tools.md)).
- Failure guidance is more specific: a missing program names a known substitute or package
  ([shell](docs/shell.md)); in unattended runs an outside-workspace refusal says that
  `request_directory` cannot be approved ([directory access](docs/directory-access.md)); and a
  Python eval that cannot start says when JavaScript eval is available.
- Fewer surprises while Dial works. After a busy Ctrl+C clears a draft, the hint offers `Ctrl+-` to
  restore it; a panel opened during a turn says on its blank bottom row that Dial is still working
  and that Ctrl+C closes the panel. Saving a project-wide grant states what it covers, the project
  root it applies to, that it lasts until revoked, and that non-matching calls still ask. Worker
  notices quote the first line of a worker's question or blocker, and the worker view header says
  how to continue it. A tool running for 10 seconds shows its elapsed time in the status line, and a
  model request that succeeds after provider retries adds one **Recovered** line
  ([shortcuts](docs/terminal-shortcuts.md), [tools](docs/tools.md), [turns](docs/terminal-turns.md),
  [composer](docs/terminal-composer.md)).
- Workers a `task` call left running in the background announce their finish, failure or question
  again, and stay in the pinned worker block. Before, such a worker could end with only an inbox
  count, because its task card had stopped updating when the call returned.
- Security: the bundled SQLite is updated to 3.54.1 (`rusqlite` 0.41), fixing RUSTSEC-2026-0459,
  where a crafted database and `-wal` file pair could return stale process memory.

## [0.3.1] - 2026-11-06

- Sessions compacted with Anthropic native compaction no longer fail every later request with
  "invalid stored Anthropic native compaction messages": the replay check now accepts the plain
  string content that the retained wire tail stores for text turns. Affected sessions resume
  without re-compacting.
- The setup model and coding/tool compatibility checks honor their single-attempt limit again, so
  an unreachable endpoint reports "Endpoint unavailable" promptly instead of retrying until the
  20-second diagnostic budget expires and reporting a timeout.
- DAP discovery now explicitly advertises existing runtime probes through non-stopping logpoints;
  tool documentation includes setup, output collection, cleanup and adapter/side-effect limits.
  This reuses the debugger session rather than introducing a separate probe execution pipeline.
- Standalone-artifact verification guidance points to one explicit format/constraint acceptance
  helper instead of empty verification calls or unrelated test searches. No-applicable-check
  receipts explicitly say nothing ran and do not claim a pass; required checks remain unchanged.
- `read` continuations after a byte-budget cut keep the requested bounds and later ranges
  (`f:10-16,960-973`, not `f:10-`), and raw reads continue as raw (`f:raw:30-`). Streamed reads
  no longer mark a CRLF line of exactly the line cap as truncated or split a multibyte character.
- `grep`/`glob` scoped inside an added root apply that root's parent `.gitignore` rules, and
  honor `.git/info/exclude` (lowest precedence) like the AST walker already did.
- `edit` refuses a `PUT @name` authored before the same call's `CUT … @name` instead of pasting
  the register's value from an earlier call.
- On Linux and macOS, local edits replace an existing file with an atomic exchange, so a crash
  mid-commit leaves the old or new file in place rather than neither. Filesystems without the
  exchange primitive keep the previous capture-then-install path.
- Settings relationships are owned by configuration and consumed by UI/application saves and
  reviewed resets. Named domain groups replace numeric registry offsets while preserving the
  existing order, metadata and defaults.
- Native invocation receipts distinguish callbacks even when provider IDs are empty. Mutation
  accounting no longer depends on verification JSON; completed callback writes are accounted
  before verification and are not counted again by their containing cell. Successful eval reset
  repairs retry dependencies only for that namespace; uncertain effects still stop continuation.
- Desktop request admission is bounded to 32 queued operations and rejects overload without
  blocking. Caller waits observe cancellation; explicit close has one two-second budget, while
  Drop cancels and detaches without waiting for an uncooperative native operation.
- Session pins reuse the owner-private atomic publisher, retaining their JSON format and locks.
  Failed publication preserves the prior file; unsafe link targets and metadata over 8 MiB are
  refused. Obsolete ratatui/lru advisory exceptions have been removed.
- Eval's core description is shorter and distinguishes embedded JavaScript from Node. Enabled
  interactive questions take priority in the initial tool offer, so optional execution schemas
  do not silently crowd out the human-input channel. Print-mode questions remain disabled.

- Portable eval: shared `dial` helpers for exact text/JSON/byte I/O, admitted commands and batches,
  retained recovery snapshots and source freshness. Binary writes use native no-overwrite staging.
  Batch receipts preserve native execution facts; fallback requires explicit proof no entry ran.
  Actual named check callbacks replace assertion-keyword credit. Scoped shell checks release
  rewrite pressure only for their named files. Runtime guidance favors small known-answer cases,
  persistent computation and preserving damaged sources before recovery.
- Native Anthropic `--effort` is independent of `--thinking`; secret-free `inference_controls`
  journal events report effective request controls for benchmark audits. These changes have not
  yet established a live-model performance improvement.
- The session prompt lists which helper languages the shell `PATH` provides (Python, Node, Perl,
  Ruby, a C compiler, Go, rustc, Java), whether or not `eval` is offered, so helpers are not
  written in a language the image lacks. The base policy asks to inspect or copy damaged inputs as
  raw bytes before opening them with a tool that may rewrite them, and to report lost data as
  missing instead of reconstructing it. Automatic `python AST` checks are skipped when no Python
  interpreter resolves for a file without a project runner, instead of failing with "command not
  found" against a leftover helper.

### Added

- JavaScript `eval` cells run on an embedded QuickJS engine when no operator-installed Node 22+ resolves (`eval.node` unset or missing). The engine ships inside the `dial` binary behind the default-on `embedded-js` feature and runs as a hidden re-exec under the same fork-fence and fd-3 protocol as the Node driver, so containers with only a shell and a C compiler can run JavaScript cells. Node still takes precedence when it resolves. The embedded engine is ECMAScript only (BigInt included): no `require`, `process`, `module` or other Node APIs, and file access goes through `tool.read`/`tool.write`, commands through `tool.bash`. Asynchronous cell completion values are recovered by rewriting a trailing expression statement, a heuristic that can differ from Node's REPL. Slim builds (`--no-default-features`) and non-unix platforms do not include the embedded engine.

### Changed

- Tool cards label `eval` calls `Eval` instead of `Run`, so code cells are no longer confused with
  `bash` commands.

- Fewer bookkeeping and re-check requests: `todo` is described as optional for short tasks and its
  updates ride with the next real call; on hosted and full-tier local routes an accepted `completed`
  answer now closes every pending or in-progress execution step (previously only the in-progress
  one), so no closing `todo done` round is needed. The prompt asks for one-line notes between tool
  calls and reuse of passing evidence until something changes; `bash` asks for scripts that will be
  rerun to be saved to a file once.
- `verify` accepts absolute paths inside the workspace (for example `/app/test_x.py` with root
  `/app`). Paths outside it and non-test files get errors that say which case applies.
- `eval` cells stay in the foreground when `shell.auto_background_secs` is 0 (the default), as
  `bash` commands do. Previously a zero threshold moved every cell that had not answered at once
  to a background job, so even `1 + 1` cost extra `bg` rounds.
- `eval` joins the stable core tool offer after `verify` whenever containment is available and a
  cell runtime resolves (including the embedded JavaScript engine), so hosted runs, unattended
  print runs included, can run code without a `tool_load` round. Local routes admit it only while
  it fits their 12 KiB core budget; the lean local eight-tool cap leaves it loadable.
- Cache-stable tool surfaces: on first-party Anthropic Messages for models that document tool
  search, Dial sends every eligible tool with non-core tools marked `defer_loading`, and `tool_load`
  / `tool_search` with `load` return `tool_reference` blocks instead of changing `tools`, so loading a
  specialist no longer invalidates the cached tools, system prompt and history. A refusal falls back
  to the previous form for that endpoint and model. On OpenAI Responses at api.openai.com, the
  planning gate, single-call narrowing and plan/goal state narrow with `tool_choice: allowed_tools`.
  Other routes, subagents and hermetic runs send the same tools as before.
- Native compaction runs automatically on first-party Anthropic routes whose model documents
  on-demand compaction when `compaction.native_models` is empty (any entry stays authoritative;
  `remote_enabled = false` turns it off). Blocking compaction, background speculation and the
  `/compact` preview choose it the same way, with Soft as the fallback. Soft summaries, like Handoff,
  first try the live request prefix plus one appended instruction so caching routes read the
  conversation from cache; on Anthropic the live-prefix summary keeps the live `tool_choice`. After
  compaction the current text of files edited this turn is re-attached (8 files, 12 KiB each, 32 KiB
  total, at most about 5% of the window).
- Preserved-thinking models (Fable 5.1, Mythos 5.1, Opus 5.5, Sonnet 5.5, Haiku 5.5): history
  projection no longer rewrites sent turns (pressure pruning, `supersede_reads`, `drop_useless`,
  notice placeholders), client-side compaction re-inserts retained turns without their thinking
  blocks, and request notes (mode notice, task context, guidance, background, hook and monitor
  notices) are recorded when a reply arrives and re-sent unchanged at the same place on later
  requests; identical restatements are not sent again. Subagents apply the same history policy.
  The catalog records `native-compaction` and `preserved-thinking` facts.
- Batching: MCP tools whose server declares `readOnlyHint` (and not `destructiveHint`) are scheduled
  as effect-free external reads; `efficiency.batch_result_percent` (default 30, 0 off) sets the
  per-reply result budget, which workers now apply too; Python `eval` cells gain `tool.batch([...])`,
  and worker cells run read-only batches together; while a call waits for an approval prompt, later
  hook-free reads it cannot affect start early and are held until it settles (denied calls never
  run). Fixed: concurrent `web_fetch`/`web_search` calls could clear a Stop meant for a sibling.
- Research checkpoint: one threshold set for the foreground loop and workers; the forced tool-free
  checkpoint stays at 24 rounds on lean local routes, elsewhere an advisory note appears after 60.
  Research, tool-contract, application-watch and peer-wake notices use the `<dial-notice>` envelope;
  the commit workflow's prompts are calmer; skill hints name `skill` only when it is offered;
  unfinished-work detection recognises more first-person phrasings.
- Turn optimizer (experimental): the typed message is sent verbatim as the authoritative request with
  the accepted draft attached as a labeled interpretation; short or already structured messages are
  not drafted; Mermaid diagrams are opt-in via `experimental.turn_drafter_diagrams`.
- New `goal.progress_file` setting (empty by default): in goal mode Dial mirrors the objective,
  budget, todos and working state to the named workspace file after each goal turn, through
  workspace confinement and checkpoints.
- Benchmarks: the `enable-compact_tools` variant is now `ablate-compact_tools`, since the compact
  tool offer ships on.

- Agent execution reports missing eval interpreters before code generation and during tool
  discovery. Custom foreground shell acceptance commands can declare `check: true`; repeated
  unverified edits trigger an earlier checkpoint that withholds further large whole-file writes
  while preserving focused repairs. New algorithms are guided toward minimal known-answer checks.
- Native Anthropic tool inputs use fine-grained streaming, with stable tool definitions on
  nonstreaming replays. No live benchmark improvement is claimed by these controller changes.

- Prompt caching (Anthropic): `system` is sent as one block per prompt band with the breakpoint
  after the stable band; plan mode, the external-thinking instruction and still-connecting MCP
  servers move from the system prompt to a transient request-tail notice; native compaction
  requests carry cache breakpoints; one-hour retention applies to the tool and system breakpoints
  only; cache-warm replays use the documented `max_tokens: 0` form with the live thinking settings.
  No cache-hit improvement has been measured yet.
- Claude requests follow per-model catalog facts: `--thinking off` uses each model's documented
  control, sampling parameters are not sent to models that removed them, first-party requests ask
  for summarized thinking where it is omitted by default, and keep-alives during an open thinking
  block no longer trip the idle timeout. A provider `refusal` ends the turn with its category.
- OpenAI: unprofiled reasoning-model chat requests send `max_completion_tokens`; Responses API-key
  requests to api.openai.com send `prompt_cache_key` by default. Responses with
  `x-should-retry: false` are not resent. Catalog: Haiku 4.5 no longer lists effort levels it
  rejects; Haiku 5.5 and Fable 5 have documented 1M windows.
- Tools: hosted routes get a 24 KiB core budget that includes `bg`, `task`, `message` and `ask` (lean
  and local routes keep 12 KiB; unattended runs without a human input channel still omit `ask`). A command that becomes a background job loads `bg` for the next request
  in every tool mode. `tool_search` accepts `load: true` and `select:a,b` and returns compact
  parameter summaries. Core and multi-action tools keep short parameter descriptions; hosted routes
  keep the full grep/glob/bash descriptions. Notebook rules moved from the `edit` description to
  `.ipynb` reads and failed notebook edits. Specialist descriptions say when to use each tool;
  `bg` and `lsp` gain examples.
- MCP: colliding tool names get a stable hash suffix instead of `_2`; PNG image results reach
  vision models; duplicate `structuredContent` is dropped; input schemas are capped at 16 KiB and
  results at the 48 KiB agent limit.
- Batching: `web_fetch`/`web_search` are effect-free external reads (approved one at a time, then run
  together); each reply shares a result budget of about 30% of the free window, clipping the largest
  artifact-backed results first; `eval` JavaScript cells gain `tool.batch([...])`; streamed early reads
  use read reuse (parent and workers), survive `todo`/`working_state` bookkeeping, drop only
  conflicting results and are cancelled when discarded; in one-call mode a leading read runs and later
  calls are skipped; parent and worker waves share one admission and honour profile overrides; ordinary
  dotfile and `@scope` paths no longer serialize scheduling.
- Completion: check failures not introduced by the turn are accepted with disclosed gaps instead of
  ending `Incomplete`, and a check's first run before the first edit is kept as its baseline even with
  `automatic_checks` off. On hosted and full-tier local routes, open todo steps no longer refuse an
  accepted answer (the single in-progress step is closed, others are listed), and the keyword-based
  investigation nudge, capability-search reminder and forced research checkpoint apply only on lean
  local routes; the capability reminder also requires a first-person denial.
- Subagents get default step limits (scout 30, planner 30, reviewer 40, test-runner 60, worker 80,
  seance 20; `max_steps = 0` is still unlimited). Coach review honours `min_changed_files`. The default
  `compaction.keep_recent_tokens` scales to 10% of large windows.
- Prompting: the base policy is short bullets with reasons, adds git safety, no-extra-scope,
  `path:line` citation, brief progress text and parallel-read rules, and drops the `[INFERENCE]`
  marker; project instruction files (AGENTS.md, CLAUDE.md) are followed for work in the repository,
  below user and operator direction; harness notices share one `<dial-notice kind="…">` envelope
  without attempt counters, capitalized commands or repeated permission disclaimers; a failed edit
  gets one cause-specific line; tool-naming sentences appear only when the tool is offered; recalled
  context comes before the user's text; the `finish` description is rewritten. Effects on model
  outcomes are unmeasured.

- Completion now reconciles plain replies and `finish` against controller obligations and evidence,
  with one shared investigation/check nudge and no repeated-answer acceptance or automatic todo
  closure. Default completion self-review is removed; explicit coach/advisor review remains.
- Ordinary hosted/local requests use a stable core under 12 KiB of schemas. Specialists and MCP
  tools require explicit loading; the cached system policy does not enumerate the offer. Base
  rules now prioritize investigation, coherent action and scoped checks. Slow check repeats are
  flagged, captured pipeline diagnostics are retained, and provider retries have a 60-second
  elapsed allowance after failure (`retry.max_elapsed_secs`, 0 disables it).
- Exact teaching snapshots are opt-in and deduplicate schema arrays across the session. Runtime persistence
  uses boundary snapshots plus incremental recovery frames, empty state patches emit no record,
  and idle memory starts workers only for due work. Failed embeddings report keyword fallback once.
- Substantive task roles no longer implicitly fall back to the small model; read-only auditors can
  attach exact private snapshots of selected session and worker records without directory grants. Added deterministic lifecycle regressions, behavior counters
  and a live benchmark slice; live performance/quality improvement remains unmeasured.
- Per-call write overhead is lower: native edits, checkpoint records, session journals and
  tool-output artifacts use plain `fsync` (macOS no longer issues `F_FULLFSYNC`), and checkpoint snapshot, manifest and
  revision flushes run in the background, drained before the next turn and before `/undo` or
  `/rollback`. Restores refuse truncated snapshots by recorded length or digest. In a fake-model
  release benchmark, a turn of 8 small writes dropped from 340 ms to 103 ms (`docs/tools.md`).
  Worker transcripts and other private records also use plain `fsync`. `dial run` publishes its
  capture at most every 25 ms (base64, unflushed) instead of rewriting a JSON number array with
  two flushes on every output read; older captures still read (`docs/application-monitoring.md`).


- Resource scheduling and read-prefix streaming are enabled by default. Independent native
  calls now include multi-path reads, scoped searches and bounded AST applies; parent progress
  bookkeeping can join groups, and inline verification can close a group after source epochs
  settle. Workers share the resource planner, overlapping preparation, live effect authorization
  and early-read observer. Git inspections join read groups. Extensions can declare trusted
  bounded resources without granting permission. Both dispatch settings remain individually
  disableable; batch approvals remain opt-in. Stop and steer apply between groups.
  Efficiency reports include group sizes, barriers, timing and early-read reuse, with separate
  worker totals. `replay_scheduling` compares recorded call ordering without executing effects;
  these structural opportunities do not predict end-to-end speedup (`docs/async-execution.md`).

- The `check` tool is now `verify`, and `edit`/`write` replace `check: true` with an optional
  `verify: [...]` list (up to 16 test files, `file::test` names or configured check names). Dial
  picks the runner (pytest, unittest, cargo, go, jest/vitest), runs the entries once after the
  last successful mutation of a response, and reports `Verify: Passed N, Failed M` with trimmed
  failure traces on the shared evidence ledger. The tool's `names`/`failed_only` become `targets`
  (including `failed`). A plain Python script runs on exit status only and is labelled weaker
  evidence. Sessions recorded with the old `check` tool or option are not resumable.

- Answer delivery is separate from work completion: `finish` outcome `answered` preserves
  outstanding work. Plain replies with blocked steps or unmet durable deliverables keep their
  report text and use a separate post-turn outstanding-work notice, not a rejected-candidate
  wrapper. Todo `kind=follow_up` records optional future work
  without starting it automatically or making it a completion prerequisite; existing/default
  todos remain steps. Shared full/compact prompts distinguish these from required durable
  deliverables and keep optional or unapproved live checks out of delivery prerequisites.
  Explicit completed claims still enforce blockers and configured/user gates. Live-model effects
  remain unmeasured ([agent loop](docs/agent-loop.md#answer-delivery-and-work-completion)).
- An `eval` cell that moves to the background now returns a normal launch receipt naming its
  `bg` job, like a backgrounded `bash` command. It no longer reports uncertain effects, which
  stopped the turn and refused further calls even though the job was still managed by `bg`.
- `eval` works on macOS with the workspace sandbox on (the default `dev` mode). It used to apply
  its process-fork fence as a second `sandbox-exec` profile inside the workspace one, which
  Seatbelt refuses, so every launch failed with `sandbox_apply: Operation not permitted`. The
  fence is now a rule in the workspace profile itself.
- A fresh `eval` interpreter must answer a no-op request before the cell counts as started, so
  a launch that never ran code is an ordinary "eval interpreter did not start" error instead of
  an uncertain-effects stop.
- Built-in system prompts now set an explicit order of work: frame the outcome, acceptance and unknowns; gather with one planned batch of the most precise offered tools; then decide, act, verify and report. "Evidence suffices" is defined: each project claim (exists, missing, works) rests on a lookup or is marked [INFERENCE], so advice no longer reports features as missing without searching for them. Overlapping wording was removed to pay for it: compact/lean policy is 6,490 bytes (was 6,486), one-call 6,450 (was 6,423), full 9,926 (was 9,957). Effect on model outcomes is unmeasured; see [agent loop](docs/agent-loop.md#efficient-tool-use).
- Everyday UX adds explicit busy-send preferences with visible delivery behavior, separately approved
  tool-compatibility diagnostics, retained Stop outcomes and reviewed saved-input previews. Offline
  journey checks and live-service acceptance remain separate (`docs/ux-journeys.md`).
- Model download guidance distinguishes closing Models from explicitly cancelling session-owned
  downloads, and downloading weights from preparing or assigning a role (`docs/model-downloads.md`).

- Unsent drafts and queued requests have private, bounded, project/session-bound recovery with
  explicit Restore/Discard. Restored queues stay held until explicitly released; recovery never
  executes input (`docs/terminal-composer.md`).
- Changed-file review opens the latest changed turn directly, retains selection/review marks,
  and supports continuous next/previous previews with writer and overlap context while keeping
  stale-content checks and explicit revert consent (`docs/terminal-turns.md`).
- Full-text session search offers selectable bounded matching-message previews, normal conversation
  opening and return to the origin without rewinding history (`docs/terminal-sessions.md`).
- Attachment drafts show a canonical included/truncated/skipped preflight, with inspection/removal
  and explicit acknowledgement of material problems before sending (`docs/terminal-composer.md`).
- Context reports separate last-sent estimates from prospective queued/draft input, usable budget,
  response reserve and compaction triggers. Manual compaction previews method/fallback and history
  boundaries before explicit confirmation (`docs/terminal-composer.md`).
- `/model backups` guides ordered fallback configuration, scope, trigger and return policy using
  cached choices, with effective-chain and data/cost review before saving (`docs/model-routing.md`).
- Automatic language-server installation now finds rustup components such as rust-analyzer through
  `rustup which`, so an installed or newly added component works with Homebrew's rustup, whose
  toolchain bin directory is not on `PATH`. Previously the install reported "installed but no
  program was found".
- Host-side `git` `status` and `diff` no longer refuse repositories with filter drivers configured
  (for example a global git-lfs install). The drivers are disabled for that read, never run, and
  the output names them. Text-conversion drivers are still refused (`docs/extensibility.md`).
- An `edit` section header whose tag is malformed (not four hex characters) now gets the same
  answer as an empty tag: the error quotes the current `[path#TAG]` header when the file is
  unchanged since its latest read. The edit is still never applied with a guessed tag.
- Peer `send` to a session that has ended names the recipient and says to list live peers again.
- A model request ended by a user interrupt is now recorded in stats as `error:interrupted`
  instead of `error:fatal`, so `error:fatal` counts only real provider or request failures.
- `dial gc` has two opt-in sweeps that delete history: `--subagents` removes worker transcript
  directories and `--orphaned-projects` removes other projects' Dial state whose project directory
  no longer exists, each only when nothing in it changed for `--prune-after-days` (default 30).
  Like every sweep they only report until `--apply`; the default selection is unchanged.
- Network outages are retried instead of failing the turn: DNS lookup failures, "can't assign
  requested address" (common after sleep/wake or a network switch), unreachable networks and a
  bare `terminated` stream error are now transient. Stats rows for failed requests also record a
  content-free `failure` kind (`http_400`, `transport_connection`, `stream_error`, ...).
- `grep` searches a retained `artifact://<hash>` output directly (line numbers from the artifact's
  start) instead of failing with "no such file". When one of several `;`-separated scopes is
  missing, the error names that scope and says to keep the others, and "Did you mean" now also
  suggests a sibling whose name contains or is contained in the missing one
  (`tool_dispatch.rs` → `dispatch.rs`).
- The system prompt's "Loadable tools" list no longer names tools that cannot run here: a tool
  whose readiness reports a missing prerequisite or unsupported host is omitted (for example
  `image_gen` when no image route resolves, or `computer` in a build without a desktop backend).
  `tool_search` still finds it and shows the reason.
- Resuming a session no longer fails with "tool task history needs a revision projector" when the
  `task` tool changed between turns. The `verify` action, the `contract` field and the
  config-dependent `work_packet` and `model_profile` fields are now recognized as additive. The
  `task` contract identity also no longer includes the list of configured profile names, so adding
  or renaming a profile mid-session keeps history resumable. History recorded under the old
  identity format is still accepted.
- Long-running turns now distinguish parked background work from model inference, show observed
  wait duration and provider retry attempt/countdown, and explain that ending a wait or interrupting
  the agent does not cancel background jobs (`docs/terminal-turns.md`).
- The busy composer offers four selectable intents: automatic task update, explicit redirect,
  after-turn follow-up and independent work, retaining the existing routes and shortcuts
  (`docs/composer.md`).
- Completion footers surface recorded changes, check freshness/verdicts, outstanding obligations,
  non-checkpointed effects and next actions without replaying work (`docs/terminal-turns.md`).
- Approval prompts preview the policy-owned reusable matcher and session/project lifetime before
  consent; commands without a safe reusable scope offer one-time approval only
  (`docs/terminal-turns.md`).
- First launch explicitly offers hosted-provider or local-model setup. Doctor leads with core model
  readiness and separates saved configuration, cached catalog reachability and an opt-in, bounded
  text-response test, with network/cost disclosure (`docs/setup.md`).
- The README is now a short landing page: install, what Dial does, basic usage and doc links.
  Build-identity details moved to `docs/release-and-install.md`, known limits to `docs/README.md`,
  and the add-a-tool note to `CONTRIBUTING.md`.

## [0.3.0] - 2026-11-05

### Changed

- LM Studio metadata prefers the native v1 model API with bounded legacy fallback and loaded-instance
  context/capability matching. Exact-model profiles can opt into typed OpenRouter routing/privacy/price
  preferences or managed Gemini explicit prefix caching. Explicit Gemini cache storage is billed and
  off by default; provider-side agents/tools remain disabled (`docs/llm-transport.md`,
  `docs/provider-catalog.md`).

- Osaurus model discovery now uses bounded `/api/show` metadata enrichment for declared tools,
  vision, reasoning, purpose and context. Inference keeps Dial's tool/approval loop, with regression
  coverage for streamed tool calls, stable request prefixes and cancellation; optional live checks
  are documented in `docs/provider-catalog.md`.

- Native Anthropic tool calls that take longer than the 60 s idle limit to generate are no longer cut
  off and replayed identically: Anthropic may buffer a large call's arguments and send only `ping`
  keep-alives, which now reset the idle clock while a tool call is open (for at most 10 minutes
  after real progress). A retried attempt's streamed text is closed as interrupted instead of being
  continued by the next attempt. Docs now state the actual 60 s default (`docs/llm-transport.md`).
- Built-in full and compact prompts now ask the agent to check once after all edits settle, not
  after each edit, and to fix every reported error before rerunning (`docs/agent-loop.md`).
- Automatic backgrounding is off by default: `[shell] auto_background_secs` now defaults to `0`, so
  foreground `bash` and `eval` calls return their result when they finish or reach `timeout`
  instead of costing an adoption notice plus `bg wait` rounds. Set a positive value to opt back in
  (`docs/tools.md`, `docs/execution-and-web.md`).
- Overwriting a 40+ line file with `write` while keeping at least half of its lines now adds a
  rewrite advisory recommending `edit`; the `write` descriptions reserve it for new or almost
  entirely changed files.
- The hashline `edit` description and `input` parameter show one call carrying several operations
  and several file sections, and ask for every planned change in one call.
- `grep` preview excerpts now count as read like `glob`/`find` previews, so rereading rows they
  showed returns a pointer; the `read` description asks for `grep`/`:outline` and targeted ranges
  instead of paging through large files.
- A command that moves to the background loads `bg` for the next request when it is loadable but
  not offered, and the adoption notice says `bg wait` instead of asking for `tool_load`. A clean
  non-service exit before a `bg wait` `until` condition is reported as finished, not as a failed
  wait (`docs/tools.md`).
- `eval` is offered automatically only when containment is available and at least one
  operator-installed runtime (`eval.node`, `kernel.python`) resolves; when only one does, its
  description names the missing language.
- Foreground `bash` results name entries the command created directly in its working directory,
  so scratch build outputs left in a deliverable directory are visible (`docs/tools.md`).
- `inference_latency` reports `generation_stall_ms`, the silent-stream part of generation time
  (`docs/configuration.md`).
- A fully covered repeat `read` is answered with a reuse pointer only when the pointer is smaller
  than the rows it replaces; tiny ranges are read normally instead of costing more context.

- The serial-lookup nudge now fires after three single-lookup rounds instead of five, repeats
  every three rounds while the alternation continues instead of once per streak, ignores a
  `todo`/`working_state` call beside the lone lookup, and tells the model to read every relevant
  search hit and the next searches in the same response (`docs/agent-loop.md`).

- Explicit native text reads reuse still-visible, complete source ranges after full SHA-256
  revalidation. Covered selections return evidence links; partial overlaps read only missing
  ranges and retain structured coverage receipts. Changed sources, incomplete coverage,
  unsupported representations and hooked calls preserve normal execution and authorization.

- Built-in full and compact prompts now explicitly encourage task-appropriate tools, batching
  independent reads and searches, and reusing results instead of repeating discovery. Dependent
  operations and one-call restrictions remain sequential (`docs/agent-loop.md`).

- The top-level `message` tool now also reaches other Dial sessions in this checkout: a 32-hex
  peer id is sent like `peers` `send` instead of being refused, and `message` without `to` lists
  live peers beside workers. Worker ids and labels resolve first, `all` stays within the session,
  workers still cannot address peers, and `peers` keeps list/claim/clear. The schema change is
  description-only, so recorded sessions resume unchanged.

- The `/watch` pane's Peers tab now refreshes during a turn from the turn's registry handle.
  Before, a pane opened while the agent was working showed "No other Dial sessions" until the
  turn ended, even with peers present.

- Python `eval` tool-call results now tolerate an optional `await` without failing after a
  successful write. Callbacks remain synchronous and authorization is unchanged; awaiting a
  completed result returns the same dictionary without dispatching the tool again
  (`docs/execution-and-web.md`).

- Tool fixes from hands-on testing:
  - `check` no longer reports a check as both `passed` and `stale` in one receipt: registered
    reproductions now run before the other selected checks, so their shell effects precede those
    observations.
  - `check` `names` accepts the short name a reproduction was registered with (`bug` as well as
    `repro: bug`). Unknown names are still refused.
  - Anchored reads and `find` excerpts clip an oversized first line instead of reporting
    "no lines in that range". If even a clipped row cannot fit, the result names the line and
    the budget.
  - `git` `log`, `reflog`, `show` and `branches` run in repositories with filter drivers such as
    git-lfs. These commands only read objects, so filters cannot run. `status`, `diff` and
    `blame` still refuse configured filters, and textconv configuration is still refused for every
    host-side read (`docs/extensibility.md`).
  - `edit` with `check: true` keeps the anchored result for the model instead of repeating the
    change as a diff. Appended check text and mutation advice now update the projection digest.
  - `ast_search` signature-style patterns written without a body, such as
    `fn $NAME($$$ARGS) -> Result<$T>`, now also match definitions: a pattern that becomes a
    function, method or declaration when ` { $$$ }` is appended also searches that completed form.
    Matches are de-duplicated and the limit still applies. Rewrites never use the completed form,
    so `ast_edit` cannot drop an unwritten body. `$$$` captures now show the original source span
    instead of comma-joined fragments, so macro arguments keep their punctuation.
  - `todo` says when archiving older closed tasks renumbers the list (`Archived 1 oldest closed
    task(s) … remaining tasks are renumbered`), instead of silently shifting indices.
  - `git` filter refusals name the active configuration keys (never their commands) and point to
    the object-only reads that still work.
  - `glob` with `preview` no longer labels blank separator lines as the workspace root (`@fN()`).
  - `tool_search` and `tool_load` report `already_offered` for tools already in the current offer
    instead of `loaded_next_request`; read-only `*` listings report `offered`
    (`docs/agent-loop.md`).
  - JavaScript `eval` explains failures caused by the cell namespace itself: a missing `process`
    or `require` names the `await import('node:…')` replacement, and redeclaring a top-level
    binding explains that declarations persist across cells.
  - `edit` diff previews omit the final-newline note when a file is emptied, deleted or created.
  - `lsp` `hover` that returns nothing while the server reports indexing progress says the server
    is still indexing instead of "no hover information".

- Generated-data guidance makes the existing foreground `eval` workflow explicit: carry computed
  values into serialization and an approved write, validate the saved artifact in the same cell,
  and return a short summary instead of transcribing results or displaying them again. Initial
  inspections favor bounded samples; source/config checks and access controls remain unchanged
  (`docs/execution-and-web.md`). The efficiency suite covers generated CSV/JSON outputs alongside
  existing source-edit holdouts; live performance remains workload-dependent.
- Generated CSV/JSON transformations now use the same intent-based routing in interactive and
  print runs, rather than excluding `data` from all unattended work. Explicit analysis, inspection,
  workbook and large-data requests still route the specialist. The producing `eval` example reads
  original input files, preserves their bytes and serializes computed values without transcription.
  Default completion self-review names concrete evidence gaps, reuses current declared coverage,
  and avoids a completion-only model request for unassessed coverage alone. Explicit coach review
  and existing verification/permission gates are unchanged (`docs/agent-loop.md`,
  `docs/requirement-verification.md`).

- `codebase_memory`, `experience` and `edit_preview` are no longer offered on every request; they stay in the catalog and load on demand with `tool_load`. The ambient "Saved codebase understanding" brief is now off by default (`codebase.ambient_context = true` opts back in) (`docs/tools.md`, `docs/agent-loop.md`).

### Added

- `image_gen` and `/image gif <prompt>` write animated GIFs with OpenAI GPT Image 2.6
  (`gpt-image-2.6-flare`/`-sunburst`) when the output ends in `.gif`, with optional `frame_count`
  and `frame_duration_ms`, on the OpenAI Images API and the Responses hosted tool. Both 2.6 models
  are recognized as Images API models (`docs/image-generation.md`).

- Opt-in tool bundles avoid intermediate observation requests: `grep(preview=true)` returns
  bounded editable enclosing-source excerpts; `write(check=true)` shares foreground focused
  verification with `edit`; browser navigation/input actions accept an `observe` bundle for
  snapshots, screenshots and new error events; `web_search(fetch_top=1..3)` fetches leading
  result pages through existing URL guards. Defaults and approval boundaries stay unchanged;
  follow-up failures retain the original successful action (`docs/tools.md`).
- `edit` accepts optional `check: true` to run foreground focused checks after a successful edit without another model request. Checks retain their approvals and verification evidence; failures are reported separately and never undo or misclassify the applied edit (`docs/tools.md`).
- `find` automatically expands literal `name` methods and string-array constants into bounded,
  directory-diverse contract references, helping expose string-based tool-name consumers across
  layers in one call. References remain lexical candidates, not confirmed ownership or exhaustive
  impact analysis (`docs/tools.md`).
- `bg start` accepts structured tool calls alongside shell commands for explicitly opted-in finite
  read operations. Nested calls retain schema/rule/approval checks; results preserve structured
  data and failures, with bounded artifact delivery and cooperative cancellation. Initial support
  covers data, web fetch/search and non-PDF document operations; unsupported hooks/controllers and
  shell-only options fail closed (`docs/background-services.md`).
- The in-context completion self-review reminder now waits for the first round that produces an
  artifact: a checkpointed edit, or a workspace file modified during the turn after an opaque shell
  command (bounded modification-time scan, so script-written files count too). It previously fired after read-only exploration whose shell pipeline
  counted as an uncheckpointed effect, which primed a separate analyse/review/recheck cycle (extra
  requests in the native OMP comparison). Up to three small produced files (4 KiB total) now ride
  along with the reminder so reconciling them needs no reread, and it asks for a brief final reply.
  Effect-only turns keep the completion fallback (`docs/requirement-verification.md`).
- Offered tool schemas are bare: every parameter keeps its name, type, enum and required status,
  but Dial-owned tools no longer send per-parameter explanations (core tools about 19.6 KB to
  11.6 KB per request). A call whose arguments do not fit gets that tool's full parameter help in
  the error. Core tool descriptions are shorter (`name: purpose`, then only rules the schema cannot
  express). Contract identities ignore descriptions, so recorded sessions still resume.
- Subagents get the same bare schemas and the same full parameter help on argument errors as the
  main agent. Simpler parameter names: `grep` `fixed_string` → `literal` and `max_results` →
  `limit`, `find` `grep_keywords` → `keywords`; the `bash` `async` alias of `background` is
  removed. Sessions recorded with the old names resume (their calls are projected onto the new
  names), and calls that still use an old name are mapped onto the new one.
- The split memory tools (`recall`, `retain`, `memory_update`, `forget`, `memory_history`,
  `memory_import`, and the backend-specific `reflect`/`learn`) are merged into one `memory` tool
  with a required `action` (`recall`, `retain`, `update`, `forget`, `history`, `import`, `reflect`,
  `learn`, depending on the backend), for both provenance and the selectable memory backends. The
  split names are removed. Configured tool selections that name them are migrated to `memory`, and
  recorded sessions that called them resume (each call is projected onto `memory` with its
  action). Sessions recorded with the earlier monolithic `memory` tool are refused on resume. Print
  runs defer `memory`; it stays loadable with `tool_load` (`docs/tools.md`,
  `docs/memory-and-learning.md`, `docs/rpc-and-acp.md`).
- `artifact_read` is merged into `read`: `read` on an `artifact://H` path returns retained lines
  (`:50-60`, `:50+20`, `:50-`, `:7`; default from line 1), `:info` lifecycle records,
  `:annotations` human feedback or `:bytes=O+N` exact base64 bytes. The separate tool is removed,
  so every request carries one fewer schema. Configured tool selections that name `artifact_read`
  drop it, and recorded sessions that called it resume (each call is projected onto the
  equivalent `read` path selector). Reading an `artifact://` path is never pruned from context or
  counted as a duplicate observation (`docs/tools.md`).
- Print runs (`dial -p`, `dial review`) also defer `bg`, `finish`, `check` and `git`; each stays
  loadable with `tool_load`. In native Harbor print runs (94 requests) the model never called any
  of them: `bash` backgrounds long commands itself, a plain answer ends the turn, and the
  completion gate still runs its checks (`docs/agent-loop.md`).
- A successful `write` now reports the line count with its byte count and on-disk SHA-256, and
  says that rereading the file adds nothing (it confirms the write, not that the content is
  correct). Benchmark print runs spent a whole request reading back a file they had just written.
- Print runs no longer pay for tools a request did not ask for. "check the result" (and other
  ordinary words such as find, finish and memory) routes a tool only when it is named as a tool, so
  the `check` deferral holds. `evidence`/`evidence_search` route only for citation intent, not with
  every format tool. Generated tabular transformations favor `eval`, while actual analysis still
  routes `data` in print runs as well as interactive runs. A matched
  workflow benchmark spent requests on `data profile`/`aggregate` before the same `eval`, and on
  `evidence capture` for a plain CSV (`docs/agent-loop.md`).
- `check` with a new `repro` after a change also runs the remaining applicable checks, instead of
  reporting them as lacking a pass and prompting a second plain `check` call.
- The generated-data rule and the `eval` description now say to write the file in the same cell
  that computes it and print only a summary. Benchmark runs printed counts in one cell, then
  retyped them into a second cell that wrote the file. The `eval` description also asks for a
  minimal cell (no banner comments or duplicate asserts) and says its asserts already cover the
  saved bytes and unchanged inputs, so later calls need not re-hash or reread them. The completion
  self-review asks for a few plain closing lines instead of a per-requirement checklist or table.
  Benchmark runs spent about 800-1200 extra output tokens (9-14 s) on these.
- Tests are no longer mandatory. The completion gate discloses edits that have no passing check
  instead of sending a "run the relevant check now" reminder. Only a failure the gate observes
  itself earns one reminder. A successful `eval` cell that asserts its results counts as a passing
  check. The in-context self-review is skipped when a passing check already covers the delivery,
  and it ignores `.tmp` scratch files such as cargo lease files. The system prompt's Prove step no
  longer asks for a rerun once a passing check covers the final edit. The turn report says
  `no recorded checks` / `Checks: none recorded` instead of `no checks` / `none run`.
- Request-efficient guidance combines related inspections and artifact computation/serialization/
  checking, and stops after requirements and owning checks pass without ceremonial rereads or
  version probes. Duplicate-shell feedback now asks for recorded outcomes and a concrete reason
  to rerun, rather than asserting a check is still current from command identity alone.
- Shell pace feedback compares whole commands and execution locations, not shared pipeline heads
  or separators. Different output projections no longer receive false duplicate-work advice;
  filtered output is not represented as recoverable complete producer output.
- Python `eval` tool shorthands accept an argument dictionary as well as existing keyword calls,
  using the same validated controller dispatch. Generated-data guidance gives synchronous Python
  and awaited JavaScript write-callback examples to avoid model-transcribed output handoffs.
- Source/config tool preferences no longer compete with the generated-data single-run rule.
  Foreground shell previews keep 4 KiB each of head and tail, with bounded middle diagnostics
  and explicit omissions; complete artifact capture is unchanged.
- Normal nonminimal offers include supported `eval` only after preparing its mandatory containment
  gate; cached preparation failure also removes retained automatic offers. Passive readiness does
  not spawn a probe. Explicit loads retain control operations; execution still fails closed.
  Repository sessions offer `git` up front. Explicit harness/model selections, small-context
  modes and authorization remain in force.
- Python checks that exit zero while reporting an unawaited coroutine no longer receive passing
  verification or repair credit, including diagnostics retained in clipped-output artifacts.
  Compact testing guidance covers queued work, awaited cancellation cleanup, final file sets and
  the expected caller/environment rather than test-only overrides. Generated-data guidance combines
  computation, writing and assertions without relaxing source-edit boundaries.
- System Bash failures for known Python, pip and Node names can report an executable alternative
  from that command's actual PATH/cwd, using builtin lookup without running the alternative.
  Operator handlers/traps are preserved. Embedded/PowerShell backends are unchanged; Bash 3's
  conservative fallback can omit hints for conditional or complex commands. Compatibility is
  not established by availability; recovery guidance reuses supplied evidence instead of probing again.
- Successful non-check shell work resumes the phase before Recovery. A stale Recovery label no
  longer counts as another failed tool round; actual failures still drive escalation.
- Repeatable `--read-only-dir DIR` grants native read/search access outside the workspace for
  the current launch only. It does not grant writes, persist writable session roots, widen shell
  permissions or bypass private storage and symlink boundaries; filesystem roots are refused.
- Linux x86_64/aarch64 `eval` uses a probed kernel seccomp fence for Python and JavaScript,
  including explicit filesystem-sandbox opt-out. Process creation and process-group/session
  escape are denied while checked runtime threads remain usable. Bubblewrap/proxy setup retains
  its own trusted startup; the final interpreter receives the mandatory fence. Installation
  failures refuse execution, with no uncontained fallback. macOS and Windows contracts remain.

- `find` uses its existing source-preview budget for the top hit plus already discovered
  conventional test/configuration candidates before additional ranked hits. Fresh screened
  excerpts retain their ordinary edit-witness rules; lexical references do not prove ownership.
- Focused `check` receipts now combine selected results with revision-scoped outstanding
  applicable checks, including no-execution calls. Bounded summaries disclose stale, failed and
  unobserved checks without rerunning unselected work or treating an empty scope as verification.
- Shell results flag a command that only prints the model's own notes (comments, literal
  `echo`/`printf`, a `cat <<EOF` heredoc to stdout) with a one-line hint to keep reasoning in
  the reply instead of spending a tool round trip on it.
- Local benchmark workspaces now live under `benchmarks/`, outside Cargo's disposable
  `target/` tree; private artifacts remain excluded from Git and Docker build contexts.
- Harbor pilot reports now attach offline per-request/tool diagnostics, source identities,
  stage timings, official verifier cases and explicit trace-coverage/reconciliation gaps.
  Raw prompts, arguments and output bodies remain in private logs; derived metadata is not
  an anonymized public export. Historical and comparison batches remain separate.
- Reconcile Harbor diagnostics with Dial's final execution ledger, including automatic focused
  checks absent from model-message journals, without double-counting or inventing output bodies.
- Harbor summaries now expose task/agent outcomes, input-checksum compatibility, job/retry layers,
  inference/storage/projection counters and explicit per-measurement coverage, with a readable
  Markdown companion. Per-trial trace reconciliation prevents pooled counts hiding missing latency.
- **Osaurus** is discovered as the `osaurus` provider at `http://127.0.0.1:1337/v1`, its
  OpenAI-compatible API (`OSAURUS_BASE_URL` overrides it). Its models appear in Models next to
  LM Studio, Ollama and llama.cpp.
- A local server that rejects Dial's constrained tool-call retry (its `oneOf` schema over the
  offered tools) no longer ends the turn with "inference failed". The retry is dropped for the
  rest of the turn and an ordinary request is sent instead.
- An OpenAI-compatible local server (Osaurus, MLX servers) is no longer asked llama.cpp's
  `/props`, Ollama's `/api/ps` and LM Studio's `/api/v0/models` before every request. A native
  route that answers 404 is remembered for five minutes per server.
- Osaurus models now get their real context window (from Osaurus's `/api/show`, e.g. 262K for
  Gemma 4) instead of Dial's 32K default. Before, an ordinary turn crossed the 32K budget and
  Dial kept compacting the conversation through the local model, which looked like endless
  "generating" with no answer.
- Picking a model in Models applies it immediately; the confirmation screen after selection is
  gone. Clearing a role assignment still asks for confirmation.
- Built-in tool descriptions are shorter (default tool schemas about 34.9 KB → 31.0 KB; `task`,
  `bg`, `working_state`, `todo`, `grep`, `glob`, `bash`, `find` and `finish`). Parameter shapes and
  contract identities are unchanged.
- A failed shell command whose cause lies outside the checkout (missing headers, packages) can be
  rerun after a successful state-changing shell command such as a package install. Previously the
  failed-operation retry barrier refused it because the checkout tree had not changed. Barriers on
  known missing files and access refusals are unchanged.
- The system prompt now says a successful write/edit result is current (no confirming reread), and
  that a package install failing on missing archives should refresh the index and retry rather
  than force-install or skip dependencies.
- A shell command that fails with "command not found" for an ordinary program now gets a short
  hint to check all alternatives in one `command -v` call, instead of probing one at a time.
- The built-in system prompt is plainer and shorter (full policy about 15.3 KB → 12.6 KB,
  compact about 9.0 KB → 8.3 KB). It drops the RFC 2119 preamble and blanket MUST wording,
  reserving NEVER for hard safety and honesty limits. Each rule now appears once, batching and
  todo pairing state their reason, and lines that suppressed progress narration are gone. Testing,
  reporting, monitoring and delegation sections are rewritten. A new rule says which guidance wins
  a conflict (user, then operator, then project guidance such as AGENTS.md and skills, none
  widening permissions). The data, document and media rule is included only when such a tool is
  offered. Approval, sandbox, trust and verification-honesty contracts are unchanged.

### Reliability corrections

- Context-overflow fallback no longer expands an already compact deferred-tool inventory into
  longer capability previews. It retains exact tool names and normal local-model hints outside
  overflow, without replacing operator prompt templates or narrowing discovery.
- Runtime containment compares canonical workspace and executable paths, so symlinked
  workspace ancestors cannot disguise a workspace-planted eval interpreter. Search scope
  checks stop at authorized read roots: OS aliases above a root no longer cause false
  refusals, while symlink scopes and escapes remain blocked.
- Shell results no longer repeat the launch receipt line, and omit the `[complete output: …]`
  link when the whole output is already inline; launch facts and the artifact stay in the
  structured execution record. A clean first failure (known effects, no committed paths,
  complete capture, correct-then-retry) gets a one-line recovery note instead of the full
  state block; repeats and uncertain cases keep it. Final reports are asked to stay terse.
- `apt-get`/`apt` install, update, upgrade and remove output is filtered like other package
  installs: per-package fetch/unpack/setup progress is dropped, while the install summary and
  every diagnostic line stay and the complete output remains in the artifact. A `2>&1` stream
  merge no longer stops a compound command from selecting its output filter.
- Shell hints are quieter: the `| head`/`| tail` trim hint no longer fires for selections of
  five lines or fewer (`gcc --version | head -1`), and `read`/`grep`/`glob` hints are withheld
  when the command names an absolute path outside the workspace, where those tools would refuse.
- Keep generated Python syntax checks from writing standard-library bytecode during cold imports,
  preventing self-induced runtime-freshness failures without executing the checked source or
  weakening identity checks. Report unavailable verification evidence as unverified, with the
  input-capture stage/cause rather than a code-failure label.
- Preserve requested targets and acceptance conditions through diagnostic workarounds, and
  keep routine completion reports concise without hiding checks, blockers or coverage gaps.
- Reconcile requested artifact layouts and derived values before completion, including small
  changes lacking observed declared requirement coverage. Reuse the permitted coach, or one
  instruction-bounded in-context self-review without remote disclosure; neither certifies correctness.
  Piggyback its compact reminder on an already-needed request after an effectful tool round,
  preserving the completion fallback without routinely adding a review-only inference.
- Distinguish hand-authored source edits from generated artifacts in full and compact tool guidance:
  permitted generators may compute and serialize requested outputs directly, without transcription
  or any expansion of path access, approvals or source-edit authority.
- Avoid reissuing a still-visible sole user request after each tool round, while preserving
  correction/steering precedence and request restoration after compaction. Expire stale
  duplicate-shell recovery feedback when its Git source-tree identity changes, without
  relaxing permissions, verification requirements or unchanged-source retry barriers.
- Cut per-request overhead seen in Harbor pilots: a first ordinary shell exit gets a one-line
  recovery note instead of the full recovery block (repeats, uncertain effects, commits and
  partial capture keep it), the retry-barrier note is shorter, and incidental task wording
  ("each row should report", "source from Debian packages") no longer offers `document`,
  `evidence`, `evidence_search` or `dep_source` schemas. A rerun after a possibly writing
  command (`apt-get update && apt-get install …`), or a new loop sharing only `done`, is no
  longer called "already ran". Lean workflow guidance now asks to regenerate stale outputs,
  check delivered artifacts against every requirement and prefer platform package tooling.
- Local model assistance (`assistance.enabled`) now defaults off: the syntax guard, related-test
  selection and learned-experience recall stay inactive unless it is turned on in
  `/improve settings` or config (`docs/memory-and-learning.md`). Lean local profiles are unchanged.
- Exclude durable undo/checkpoint links from canonical inference and training messages while
  retaining them in session storage. Accepted request snapshots can be harvested without
  weakening strict message validation or losing earlier conversation context.
- Enforce benchmark RPC case deadlines before dispatch and before accepting queued replies;
  preserve operation/request-ID diagnostics without exposing request parameters.
- Point review-revert recovery to `/undo files`, keeping bare `/undo` conversation-rewind
  safety checks intact. Exercise refusal and file-only recovery in the real-terminal fixture.
- Clear inline terminal rows without a display-wide erase, preventing tmux's default
  `scroll-on-clear` behavior from copying a live streaming viewport into scrollback twice.
- Align delegation integration fixtures with the existing planner-agent-to-coach model routing.
- Select the actual binary target for related Rust tests when library and binary roots share
  a source directory. Parse top-level external module declarations, exclude path-remapped
  declarations from conventional ownership, and report unowned modules as coverage gaps.
- Interpolate missing-witness recovery paths and retain refreshed-read retry guidance in
  targeted edit refusals without weakening witness validation.

### Bounded duplicate-read output

- Use the same compact, size-checked pointer for identical reads and covered source ranges.
  Compare model-visible sizes after file-handle projection and omit redundant file labels on pointers.
  Keep tiny successful read results inline instead of expanding them into duplicate coaching;
  preserve raw evidence, freshness checks, duplicate accounting and retry barriers.

### Release preparation

- Separate metadata preparation from explicit CI-gated tagging; preparation no longer builds,
  stages, commits or publishes. Add `make release-check` to report cheap metadata, packaging,
  shell and tooling failures together before compilation. Gate CI builds and release signing
  readiness early, retain exact-commit publication checks, and preserve failed release build caches.
- Remove accidental source backup copies, exclude patch/editor recovery files from Git and
  container build contexts, and replace temporary release-preparation status notes with durable
  platform, verification and maintainer guidance.
- Ignore non-directory matches when expanding Cargo workspace members in the no-build release
  gate, while still refusing matched crate directories with missing or invalid manifests.
- Compile Unix-only weights-test helpers only where their owning tests run, keeping Windows
  Clippy's warnings-as-errors gate intact without suppressions or dropping portable tests.
- Reduce push/PR Linux feature compilation from 32 combinations to eight representative
  selections, sharing the same compile job with weekly and manually triggered exhaustive
  coverage. Limit feature-job concurrency while preserving all other platform/release gates.
- Scope Windows contract filters to their owning crates and preserve their build cache after
  failures. Stage the already-built Unix CI executable for smoke checks instead of rebuilding
  it with `cargo install`; keep artifact and recovery checks intact.
- Check migrated-record privacy with native owner-only permissions on Windows and Unix. Fix
  real-terminal fixtures to use the visible session-tree command and the configured interrupt,
  retaining draft, branch-history, file-safety and partial-write cancellation assertions.

### Shorter turn reports and final answers

- The end-of-turn report is shorter: its collapsed line names the check state (`checks passed`,
  `checks failed`, `checks stale`, `checks unverified`) instead of `checks current`, and the expanded
  view omits empty sections and drops the fixed undo/resume/timing boilerplate.
- The system prompt's delivery rules are shorter and ask for a few-line final answer (outcome,
  checks, gaps) without chronology, headings or a restated plan, reducing per-request prompt tokens.
- Test and build commands (`cargo test`/`clippy`/`check`, `make test-*`, `pytest`, `jest`/`vitest`,
  `go test`, `tsc`, `npm run build`, …) render as a compact **Test**/**Build** card: pass/fail/ignored
  or error/warning counts in the header, then only the failing tests or the first errors with their
  locations. A clean pass is one row; the expand key shows the full log. Unrecognized or unattributed
  failures keep the ordinary output card.
- A fully passing test run (`cargo test`/`nextest`, `pytest`, `jest`/`vitest`, `go test`) with no
  warning or other diagnostic now reaches the model as one `test result: ok. N passed; 0 failed; …`
  line instead of its filtered log, saving context tokens; the complete output stays in its artifact.
  Runs with a failure, warning or error keep the existing line filter and failure digest.
- A failing test run's digest now names each failing test with its panic or assertion detail
  (`cargo test`/`nextest`, `pytest`, `go test`, `jest`/`vitest`), so the model rarely needs to read
  the output artifact. The digest is skipped only when every line is already visible near the top.
- Edit results no longer repeat the changed lines as a second diff preview when the numbered excerpt
  was only row-truncated, and the "removed lines" report is skipped for whole-block rewrites.
- The saved codebase brief is capped to six rows, and common words no longer match unrelated topics.
- Search and glob footers are shorter: a complete walk prints `walk complete` plus only the
  non-default flags, and the `[page:]` line appears only when there is a next page. The `| head`
  hint and the auto-background notice are shorter.
- `bash` with an explicit `timeout` longer than `[shell] auto_background_secs` now stays in the
  foreground until that timeout, so the result returns in the same call instead of needing a
  `bg wait`; a command still running then moves to `bg` rather than being killed.
- The latest-request reminder is no longer re-sent at every tool boundary while that request is
  still visible in history; it returns after compaction removes it. Steering keeps its reminder.
- The prompt asks the model to start large files with a `:outline` read.
- Removed two noisy notices: the `Background` note when a job starts or a foreground command is
  moved to the background (the launching card already shows it), and the one-time Claude
  subscription compatibility notice.

### Bounded discovery and evidence reuse

- Prefer one conventionally named test in opt-in glob previews without increasing the existing
  three-file, forty-line or 6 KiB limits, changing sorted path pages, or adding prompt/schema text.
- Reuse still-visible complete source rows from successful edits and discovery previews when a
  fresh read returns the same snapshot and range. Keep raw evidence, freshness checks, missing-line
  recovery and verification gates intact; compact range pointers only when smaller than the source.

### Centralized configuration and session controls

- Regroup Settings by purpose with horizontal category tabs and a stable scope/search header,
  explicit F4 Appearance preview instead of expanding samples while browsing, named scope, effective-source
  explanations, goal synonyms, Changed-only filtering and confirmed exact bulk override removal.
  Preserve canonical validation, stale-review checks, credential masking and embedding-pair review.
- Add safe named setup capture and an outcome-oriented local-model checklist using existing
  explicitly reviewed installers, model selection and provider tests. Show known prerequisite
  readiness beside memory/voice controls without inventing running-service or hardware health.
- Separate active versus selected model tuning and review assignment effects, including actual
  project clears. Record main and worker model-route reasons without reconstructing historical
  decisions from current defaults. Add typed masked collection creation/editing with Advanced TOML
  retained, and a Memory landing page with direct role-picker detours and retained return navigation.
- Add a rebindable F10 session-control doorway and compact composer controls; distinguish active
  facts from saved defaults awaiting adoption. Group Context/history settings with recorded limits.
- Retain linked-panel navigation while refreshing configuration values, keep downloads owned by
  the session rather than a browser view, and expose existing worker inspection/recovery owners
  through Activity. Add optional grouped action/settings/session/help palette search.
- Make role Headers and reasoning/inheritance discoverable in details, retain permanent named
  scope in Models/Providers/tuning, and offer per-notice preference destinations without marking
  notifications read. Retain typed historical download failure evidence for Attention inspection.
- Add read-only modal Attention inspection without answering the pending approval/question;
  pause timed question deadlines during inspection. Let small-terminal Appearance switch between
  its retained editor and a scrollable, unsaved representative preview.
- Track the forty-item scope and remaining acceptance evidence in
  [the control-surface ledger](docs/control-surface-acceptance.md); sample rendering and local
  tests do not certify live services or complete interactive-terminal behavior.

### Model role consolidation

- Remove separate planner, grounding and image-embedding role rows without removing their features.
  Plan mode and planner agents use coach/main or their explicit launch routes; vision tools retain
  explicit models in `vision.grounding` and `vision.image_embedding`, including setup support.
- Consolidate judge into `decision` for typed judgments through chat models or Laya. Store Laya
  checkpoints in `laya.checkpoint`, separately from the selected decision backend. Migrate former
  role assignments, references and fallback-chain keys, preserving explicit destination settings.

### Harness effectiveness and recovery

- Make fault witnessing opt-in and limit it to explicitly declared behavioral commands, not
  automatically discovered related suites. Permit one attempt per user turn with a 60-second
  command cap, remove per-suite notices, and keep comparisons diagnostic: passing without the
  edits neither invalidates normal verification nor demands more tests. Normal verification,
  registered fail-then-pass reproductions and edit bisection retain their existing behavior.

- Cross-agent reports add failure-inclusive time/tokens per verified success, nearest-rank p95
  latency, and explicit installed/controlled setup declarations. Unknown usage stays unknown;
  configuration notes do not certify matched setups or establish a live-model quality win.
- Add opt-in private cross-agent session traces (`--capture-traces`) with bounded isolated-home
  lookup, per-turn availability and separate export timing, retaining native calls/results for
  diagnosing repeated interactions without changing independent outcome scores.
- Target verification guidance at the changed entry point instead of unrelated suites, while
  retaining broader checks for affected interfaces, uncertainty, requests and required gates.
- Clarify that unchanged rereads succeeded and can reuse visible source; distinguish passing
  post-fix checks from missing pre-fix reproduction without changing evidence credit or gates.
- Recommend opt-in source previews only for code inspection when `glob` is offered, and compress
  equivalent policy prose to meet the unchanged 9,000-byte compact-prompt budget.
- Remove the duplicate callable-tool name inventory from efficiency-mode built-in environment
  metadata; retain all schemas, deferred discovery, operator-owned prompts and verification gates.
- Attribute request-prefix serialization/hash preparation in microseconds through the existing
  request journal and operation ledger, without caching away hook-driven history changes.
- Release repeated directory-read recovery feedback when the existing visible listing changes,
  including nested additions/removals, without treating directory hashes as edit witnesses.
- Show worker settings revisions and queued/applied/failed receipts in the inspector, keeping
  requested models separate from the active model.
- Identify extension/tool/command registration errors with SDK version and ownership context;
  keep rejected registrations atomic and permit corrected same-name installation.

### Discovery source previews

- Add opt-in `glob` source previews (`preview=true`) using the existing confined, secret-screened
  navigation reader. Returned rows can support edits without a separate read; ordinary path listings,
  pagination and verification requirements are unchanged. Historical tool contracts remain compatible.

### Highlights

- **Everyday workflow:** a simpler command surface, model/settings discovery, recent sessions,
  compact turn reports and clearer steer-versus-queue choices. Existing command aliases remain available.
- **Safer recovery:** tighter credential bindings, session-switch permission boundaries, durable undo
  and explicit uncertainty when remote writes or worker integration cannot be confirmed.
- **More dependable execution:** bounded streaming, discovery and background processing; verification
  failures remain visible rather than becoming successful completion claims.
- **Local model tools:** optional inspection and evaluation-gated weight editing, with user-controlled
  promotion. These capabilities do not establish a general model-quality improvement.
- **Cleaner source distribution:** remove obsolete reports, clarify documentation and benchmark
  entry points, and keep local state out of release inputs.

The detailed changes below retain compatibility notes, measurement limitations and technical context.
Historical release sections remain unchanged.

### Repository hygiene

- Remove historical audit/review reports and generated benchmark output from the release source
  scaffold. Keep benchmark fixtures, runtime assets, release tooling and unfinished plans;
  exclude disposable reports, experiments and local tooling from Git or Docker build contexts.
  Release readiness remains subject to the checklist in `docs/release-and-install.md`.
- Point the root README at the grouped documentation index and add examples/benchmark navigation
  without moving consumed fixtures. Share packaging, installer and build-check Make targets between
  CI and preflight; `make preflight-worktree` checks uncommitted work without creating a commit.
- Pin portable CI tooling transitively with a generated hashed lock and pin nixpkgs to an immutable
  commit. Native Nix locking/builds, hosted protection configuration and release signing remain
  explicit acceptance gaps; no new live-model quality or performance improvement is claimed.

### Audit hardening: credentials, sessions and request fidelity

- Bind stored MCP OAuth grants to the exact effective resource URL; require sign-in again for
  missing or changed bindings. MCP-discovered credential endpoints and refresh URLs require
  HTTPS except genuine loopback HTTP (`docs/extensibility.md`).
- Gate implicit ACP session activation before teardown and prepare incoming session directory,
  checkpoint and Cargo bindings before publishing it. Outgoing directory and sensitive grants
  do not carry over (`docs/directory-access.md`, `docs/rpc-and-acp.md`).
- Preserve configured endpoints and URL templates for cross-provider fallback/promotion; apply
  role thinking even when an auxiliary request reuses the main backend (`docs/model-routing.md`).
- Protect Cargo cache adoption/pruning with session leases and held native/cross-target profile
  locks; conservatively rescan scheduled watches after structural directory events
  (`docs/shell.md`, `docs/extensibility.md`).
- Preserve incomplete/refused native batch output and reasons instead of reporting success;
  reject new paid batches whose conservative output envelope exceeds bounded collection capacity
  before upload. Legacy saved jobs remain readable (`docs/provider-batches.md`).

### Simpler terminal UX

- Commands: `/new`, `/sessions`, `/undo`, `/review` and `/help` absorb overlapping commands; `/clear`,
  `/resume`, `/palette`, `/commands`, `/tree`, `/branch`, `/rollback` and `/review-file` still work but
  are hidden from menus. New subactions: `/help commands`, `/undo files`, `/review files`,
  `/sessions this|branch|label|fork`; `Ctrl+T` in `/sessions` opens the current session tree.
- Bare `/review` offers AI review or keep/revert files; bare `/btw` opens a side-thread picker;
  `/todo` has Plan and Progress tabs.
- The busy chooser offers **Steer** or **After this turn**; `r` hard-redirects, `w` (or `/worker <message>`)
  starts an independent worker.
- Keys: work-panel tab defaults to `Ctrl+X` (`Alt+W`, `F12` kept); `/models` filter, sort and header
  editor move to `Shift+F2`/`Shift+F3`/`Shift+F8`; hints show one primary shortcut; Esc closes, Ctrl+C
  stops (Esc no longer cancels compute waits); Enter runs a slash suggestion only after you navigate to it;
  `Alt+S`/`F11` expands the adaptive status line.
- Home shows recent sessions inline and a dismissible first-run tour; turn reports are one compact line
  with a next-step hint (`Ctrl+O` expands); new [Everyday Dial](docs/everyday.md) guide.
- Plain approval labels (Ask first, Edit freely, Full auto, Read-only; config values unchanged), settings
  grouped under Common/Model/Safety/Appearance/Keys/Advanced, a plan card at 60–89 columns, and a single
  progress row.
- One shared command registry holds descriptions, aliases, categories and turn scheduling. `/help
  <command>` shows the matching `dial …` CLI command, and commands that need Git or a session stay
  listed but dimmed with the reason.
- Settings show where each value comes from (default, global, project, environment, CLI);
  `/set <key> [value] [--project]` shows or saves a validated value. Shortcut overrides live in
  `config.toml [keys]`, with `keybindings.toml` kept as a per-binding fallback.
- `/accounts` lists every saved login without credentials; small auth, state and cache records are
  copied lazily into `auth/`, `state/` and `cache/`, keeping the originals as fallback.
- Panels share `?` key help; `/inbox` collects session notifications with unread status; `/doctor`
  lists diagnostics and reviews a fix before applying it.
- New hubs `/mode`, `/dirs`, `/collab`, `/learn`, `/share`; tabbed `/git` (Status/Diff/Commit/Log)
  and `/activity` (Workers/Processes/Compute/Schedule/Queue); `/extensions` gains Tool groups and
  Hooks tabs; bare `/stats` opens Usage/cost, Tokens, Sessions and Tools tabs. Older spellings
  still run ([Commands and help](docs/terminal-commands.md#hubs-and-panels)).
- Docs have a three-tier [index](docs/README.md) and focused terminal topic pages; `/help <topic>`
  points to them.

### Discovery efficiency and request diagnostics

- Record each foreground request's batching/one-call policy and reasons in workflow events and
  session request-policy history; `/reliability` exposes the active constraint.
- Reuse empty-search memory to offer one policy-aware declaration-read correction after distinct
  unsuccessful probes against one file. Intentional absence checks remain valid; ambiguous shell
  scripts are not guessed. Remove policy-blind batching advice from shell trimming hints.
- Add a tuple-registry discovery regression evaluation with independent documentation, settings
  and keybinding evidence, batch and one-call scenarios, and offline trace/round-trip grading.
  See `docs/agent-loop.md`; live-model performance improvement requires a separate measurement.

### Recovery, permission scope and source fidelity

- Strict worker contracts retain controller-owned integration uncertainty instead of allowing
  retries after incomplete rollback. Nonisolated advisory contracts are rejected before durable
  intent, and dependency authority is revalidated inside promotion before a candidate can land
  (`docs/subagents.md`).
- `/undo` selects durable conversation-linked checkpoints, including read-only turns, instead of
  counting file-changing turns. Partial restoration reports warnings and keeps the conversation;
  durable restoration progress permits an inspected retry. Branch gates run before restoration.
  Standalone `/rollback` keeps its file-changing-turn semantics (`docs/tools.md`).
- ACP editor writes distinguish definite pre-delivery refusal from unconfirmed completion,
  retain request identity and unknown effects, and require acknowledgement followed by a fresh
  read before retrying an uncertain write on the same connection (`docs/rpc-and-acp.md`).
- Browser clipboard access uses a disposable, script-free controller context with only the
  requested permission, never grants clipboard access to the page, and revokes/closes the context
  on exit. SSH writes use an exclusively created private sibling staging directory rather than a
  predictable symlink-following temporary file (`docs/execution-and-web.md`, `docs/ssh.md`).
- Alternate Markdown reports the actual final resource URL and its relationship to the original
  page. Optional HTML discovery shares a deadline, request and aggregate download budget, with
  bounded deduplicated candidates (`docs/execution-and-web.md`).

### Bounded execution and discovery

- Model streaming uses a byte-bounded delivery queue with backpressure, discards cancelled
  payloads, and releases blocked producers before joining; terminal replies remain separately owned.
- Model discovery coalesces superseded refreshes, bounds optional Ollama capability enrichment,
  follows bounded Gemini/Anthropic pagination without caching partial listings, and shares immutable
  parsed-cache snapshots for targeted metadata lookups (`docs/provider-catalog.md`).
- MCP lifecycle notifications coalesce in bounded storage; stale catalogs can retry validation
  with cancellation and cooldown while old definitions remain refused. Startup and refresh share
  bounded tool listing, and supporting streamable-HTTP servers receive bounded SSE replay cursors
  on reconnect (`docs/extensibility.md`).
- Failure in either parallel split-summary fold cancels only its sibling inference and joins both
  folds, retaining user-stop and fallback behavior (`docs/agent-loop.md`).
- In-process shell results distinguish stopped, abandoned and uncertain descendant cleanup;
  discovery shares a deadline/output budget. Readiness observes new output with explicit loss and
  observation-limit diagnostics while preserving bounded contiguous regex input (`docs/shell.md`,
  `docs/execution-and-web.md`).
- Configuration rejects malformed hook overlays and validates standalone global settings before
  saves/previews. Grep reuses anchored traversal prefixes and reports the actual continuation cap;
  directory attachments bound discovery and report errors/interruption (`docs/configuration.md`,
  `docs/tools.md`).
- Scheduler launch failures retain pending triggers with capped retry backoff; filesystem events
  use bounded coalescing with conservative overflow rescans, and content fingerprints detect
  same-size edits with restored timestamps (`docs/extensibility.md`).
- Failed stream-journal copies settle the writer and clean up their own destination, including
  snapshot refusal; cleanup failure preserves both diagnostics and uncertainty.

### Session recovery and search

- Session moves settle copied journals and transfer-ledger state with the workspace switch, and
  report uncertain publication or cleanup without claiming rollback.
- Workspace-root header replacement syncs the parent directory on Unix and retains the replacement
  writer and roots when a post-rename durability acknowledgement fails.
- Interactive history search runs index maintenance and SQLite waits on a background worker,
  coalesces pending queries, and discards superseded or previous-session/workspace results.
- The full-text search index detects in-place journal rewrites, including changed prefixes before
  appended content, and rebuilds affected messages transactionally (`docs/terminal-interface.md`).
  No measured search-latency improvement or power-loss test result is claimed.

### Request execution and fidelity

- Immediate credential rotation, OAuth refresh and reasoning-parameter recovery now honor the
  same per-error-class attempt budget as provider retries, so rotation cannot bypass a ready
  fallback's short retry limit. Ordinary no-fallback retry limits are unchanged.
- The experimental turn optimizer rejects drafts that omit explicit verbatim code/quoted spans,
  path-like tokens, references or numeric tokens before review or main inference. Rejection
  retains fail-closed delivery; semantic equivalence and negation still require human review.
  No live-model quality improvement is claimed (`docs/agent-loop.md`).
- Named tool-concurrency policy preserves existing valid worker/read limits (1..=64) and the
  dependency-wave ceiling (32), without new settings or wider mutation concurrency.
- Request-prefix tracking reuses request-local serialization capacity and stores fixed-size
  hashes, without retaining message text or caching potentially stale projected fingerprints.
  A matched offline 4.7 MB debug-build microbenchmark showed no meaningful latency improvement
  (median approximately 82 ms before and after); provider latency and model outcomes are unmeasured.

### Added

- Opt-in named task contracts persist bounded workflow intent and dependency bindings, require
  current controller-observed acceptance before successful completion, and recover without
  replaying workers or trusting saved verdicts (`docs/subagents.md`). Ordinary tasks are unchanged.
- A compact work panel above the composer on terminals 90 or more columns wide: one rounded card,
  at most 100 columns wide and as tall as its content, with the plan's steps and the `plan`,
  `evidence` and `approvals` tabs (`Ctrl+F`; `Alt+W` and `F12` remain aliases; `app.work.tab`).
  The launch input is a rounded box, the default status line leads with a colored approval dot
  (`● ask`), in-progress steps use the warning color, and active tabs are drawn as filled pills
  (`docs/terminal-interface.md`).
- Delegated results distinguish candidate completion from controller acceptance in synchronous and
  live task metadata. Built-in handoffs ask for observable criteria, configured checks when
  available and explicit uncovered requirements (`docs/subagents.md`).
- `/todo progress` exposes persisted deliverable claims alongside current linked-check status,
  acceptance, unverified references and worker outcomes. Existing input witnesses are revalidated
  when opened idle; busy views report unavailable rather than stale-as-current evidence
  (`docs/terminal-interface.md`).
- Explicit improvement cycles retain bounded, deduplicated recurring-failure lesson proposals with
  trajectory references, displayed in `/improve status` for review. Proposals never automatically
  activate rules or write project instructions, and require no extra inference (`docs/improvement.md`).
- Per-role thinking levels: `model_roles.<role>.thinking` (`off`, `minimal`..`max`) applies to
  every request of that role, including subagents. Set it with `/effort <role> <level>` and clear
  it with `/effort <role> default` (`docs/model-routing.md`).
- The stats ledger records each request's time to first streamed output (`first_output_ms`) and
  requested reasoning effort (`effort`) (`docs/llm-transport.md`).
- The stats ledger's `requests.changed_part` names the request section (`system`, `tools`,
  `messages`, ...) where a `prompt_changed` request first differed from the previous one, so
  prompt-cache breaks can be attributed (`docs/llm-transport.md`).
- Read-only weight and activation inspection for Dial-owned local MLX models (`docs/improvement.md`).
  `dial weights map|stats|activations|trace` and `/weights` report per-layer parameter counts,
  weight norms, activation contrast for matched response pairs, and answer-loss sensitivity to
  skipping each layer or removing each attention head. `/weights open` serves a 127.0.0.1
  dashboard with heatmaps and a head grid, and `/improve` gains a Weights tab. The approval-gated
  `model_weights` agent tool (off unless listed in `integrations`) runs the same operations. Model
  files are never written; GGUF and hosted models are refused.
- `model_weights` `edit` accepts `method = "scale"` (`dial weights edit --changes FILE`): the model
  names exact tensors, factors and optional rows or columns, so it can edit weights directly
  instead of through a measured direction. The copy, evaluation gate and user-only promotion are
  unchanged.
- Weight edits are easier to get right. `attribute` ranks attention heads and MLP neurons by estimated
  support for given answers (optionally against `wanted` answers), `try` scores an edit in memory at
  several strengths with neutral-answer drift and writes no files, scale changes can name a layer's
  `heads` or `neurons`, selectors are validated against tensor shapes, worker contract errors reach
  the caller instead of being withheld, quantized edits whose requantization error exceeds half the edit
  are refused unless `allow_noisy`, and `status` lists earlier edits with their parameters.
- `/weights improve GOAL` (or a plain request, with `model_weights` enabled) has the model run the
  whole weight-edit loop for a goal in your words; approvals and user-only promotion are unchanged.
  It edits the current model's weights but can be driven by another model: a picker over the
  configured chat models, or `/weights improve --with MODEL GOAL`, for that request only.
  `attribute` gained `focus: repeats`, `neutral` answers (components generally important rank low),
  per-component `agreement`, neuron promote/suppress tokens and head induction attention.
- `dial weights analyze` probes the local model with built-in text and records what its parts do:
  layer importance, attention-head roles (induction, previous-token, attention sink) and the
  busiest MLP neurons with the tokens they promote. `dial weights improve GOAL` starts a session
  with the goal and the weight-editing playbook and enables `model_weights` for it; the playbook now
  runs `analyze` first. Approvals and user-only promotion are unchanged.
- `dial weights`, `/weights` and `model_weights` no longer fail with "improvement stopped by operator"
  when background improvement was stopped for the model: that switch pauses automatic work, not
  weight inspection you ask for.
- `[weights] model` names the local MLX model that weight inspection and edits act on, so a hosted
  frontier main model can drive `model_weights` edits to a different local model. Promotion and
  revert then change that setting, not the main model.
- Evaluation-gated weight edits for Dial-owned local MLX models (`docs/improvement.md`), following
  the "Sparks of Weight-Space Programming" reasoning-loop fix. `dial weights experiment` sets a
  forward-pass budget, which `generate` (token-limit hits, repetition and response checks) and
  `direction` (per-layer unwanted-vs-wanted MLP direction) are charged against. `edit` writes a new
  hard-linked model copy with that direction projected out of chosen layers' `down_proj` columns,
  reporting requantization error on quantized models. `evaluate` compares base and edit on
  held-out cases the experiment never saw (sign test), and `promote`/`revert` switch the
  project's model only to a copy that passed. `/weights` and the `model_weights` tool gain the
  same operations; the tool cannot promote or revert.
- `dial weights` grew into a fuller workflow (`docs/improvement.md`). Qwen3.5/3.6 hybrid
  mixture-of-experts models are supported: `analyze` classifies gated full-attention heads and
  reports expert routing and linear-attention output, `attribute` ranks routed and shared experts,
  and scale changes may select `experts` or `shared_expert`. Every run is kept with a `run_id`
  (`history`, `show`, `compare` of two `analyze` runs, `prune`) and linked from its experiment.
  Evaluations keep per-case rows per edit and refuse edits that raise loss on general probe text by
  more than 5%. New edits: steering bundles folded into weights (`edit --bundle`) and `combine` of
  2–4 edits. `cases --session` builds cases from a past session, `next` suggests the next step,
  `improve` accepts presets (`loops`, `concise`, `rambling`, `format`), and the dashboard shows
  edits, evaluations, dose curves, rankings, head grid, routing, experiments and recent runs.

### Fixed

- Weights jobs explicitly release their project lock when the operation ends, including error
  returns, so a descriptor inherited during concurrent process spawning cannot leave a completed
  job appearing active. Live jobs and ledger changes remain mutually exclusive (`docs/improvement.md`).

- Skill tool cards show the bundled reference-file path as well as the skill name, distinguishing
  chapter reads from repeated core-skill loads (`docs/terminal-interface.md`).

- Local subagents now share the parent's generation-silence watchdog and bounded same-request
  recovery, rather than waiting without it. Remote stalls are handled only by transport retries,
  avoiding a competing watchdog that could cancel recovery (`docs/llm-transport.md`, `docs/subagents.md`).
- `check` rejects an empty named selection instead of reporting verification success and a
  matched pass prediction when no checks ran. Explicit reproduction registration still runs
  its initial attempt (`docs/agent-loop.md`).
- Automatic verification repair stops early when the same confirmed diagnosis recurs without
  changed declared inputs; no-op/reverted edits cannot reset that bound when complete input
  contracts are available. Undeclared checks retain an effect-revision fallback. Failed checks
  and infrastructure blockers remain visible (`docs/agent-loop.md`).
- Streaming answers reuse cached markdown rows when validating stable text for scrollback, avoiding
  repeated full-prefix parsing while retaining exact timestamp and styled-row checks
  (`docs/tui-rendering.md`).
- Rejected `check` selections no longer register or replace reproductions and erase their prior
  failure evidence. `repro` with `failed_only` is refused before registration, with instructions
  to register/run first and rerun afterwards (`docs/agent-loop.md`).
- Anthropic prompt-cache misses after a new user turn: tools routed in by an earlier request of
  the session stay offered, so the tool list in the cached prefix no longer shrinks and rewrites
  the cache when the next message matches different tools.
- The terminal stays responsive while the main agent waits on subagents, side questions or
  background work: those waits now hand the UI a heartbeat every 80 ms instead of every 250 ms.
- `dial weights analyze` no longer fails with "subprocess failed (exit status: 1)" on models whose
  forward pass reads attention attributes, such as mlx-vlm Qwen3.5/3.6: the attention recorder now
  forwards attribute reads to the module it wraps.

- Tools that automatic routing offers for one turn stay offered on later turns (bounded, oldest
  dropped first), so the system prompt and tool array no longer change between turns and a
  provider's prompt cache, history included, is no longer invalidated by a routed turn
  (`docs/agent-loop.md`).
- Compaction no longer fails with an error when there is history but nothing can shrink it:
  manual and RPC compaction report an unchanged result, summaries are not requested for under 512
  tokens of replaceable history, Snapcompact is skipped for models without image-token pricing
  (it was billed at one token per pixel, inflating a few short turns to about 185k tokens), and a
  failed automatic attempt is not rerun at the start of every new turn. The `continued-session`
  efficiency benchmark now has enough earlier history for its forced compaction to shrink.
- Background job output no longer splits a multi-byte character across two reads.
- A background job that fails shows a digest of its first error lines when its earliest output
  has scrolled out of the capped read (`docs/shell.md`).
- Child workers get the same repeated-call and call-cycle redirect as the main loop, and the
  `repeated_calls` workflow metric now also counts unchanged re-reads and loop-guard short circuits.

### Changed

- Shorten shared full/compact prompt policy while preserving tool, trust, approval and verification
  contracts. The owning rendered fixture measures policy bytes **12,805 → 9,528** (full) and
  **8,535 → 6,245** (compact); lean/one-call and total message-text sizes are documented in
  [agent-loop.md](docs/agent-loop.md). These are byte measurements, not live-model outcomes:
  effects on correctness, requests, tokens and elapsed time remain unmeasured.

- Artifact generators may serialize and verify their delivered output in the same invocation,
  avoiding redundant check/readback calls without bypassing configured verification or approvals.
- The Git tool exposes bounded, read-only HEAD reflog inspection so lost-work recovery can
  identify recent abandoned commits without guessing from unrelated dangling history.

- The shell output minimizer also filters outputs longer than the inline head-and-tail view, using
  the complete copy in the output artifact, and recognises `cd DIR && <command>` sequences,
  `pip`/`uv` installs and `jest`/`vitest` runs (`docs/shell.md`).
- A new session takes over the most recently used Cargo target directory idle for 30 minutes, so
  its first build is incremental, and directories unused for seven days are deleted in the
  background; directories locked by a running Cargo build are left alone (`docs/shell.md`).
- The single-task `acceptance_checks` description of the `task` tool no longer repeats the batch
  description, trimming every request's tool schemas.
- Unhinted provider retry backoff doubles up to 60 seconds per attempt instead of up to
  `retry.max_delay_secs`; server hints and quota waits keep the configured bound
  (`docs/llm-transport.md`).

## [0.2.11] - 2026-11-03

### Security

- Refuse unquoted shell expansion in inspection exemptions, native writes into Git metadata,
  and host Git reads with executable filters or text-conversion drivers. Git history inspection
  cannot invoke repository-configured signature verifiers. Remote SSH wrappers no longer depend
  on the login shell's path-quoting grammar.
- Authenticate collaboration hosts and writable guest actions without transmitting the write
  token. Protocol 1004 requires newly generated links and updated clients (`docs/collab.md`).
- Restrict text-recovered tool calls to offered tools on local/private endpoints and require
  one-shot human approval throughout parent and worker execution. Saved grants, speculative
  reads and batch shortcuts cannot authorize recovered calls (`docs/llm-transport.md`).
- Allowlist inherited child environments, redact secret debug output, zeroize tokens, and
  warn about the account-policy risk of Anthropic subscription client identity. Existing
  subscription sign-in remains available without an additional opt-in.
- The release signing job also removes its private-key file in an `if: always()` step, so the
  key is shredded even when the signing step is killed before its exit trap runs. The publish
  job checks every artifact against `SHA256SUMS` and the signature against the public key before
  creating the release (`docs/release-and-install.md`).

### Fixed

- Local image generation no longer times out before slow diffusion models finish: `image.timeout_secs` now defaults to 900 (was 180; MLX-Serve's MOSS-Image takes about five minutes per 1024x1024 image, and Dial hung up first, so the server cancelled the job). A timeout error now names the limit and the setting to raise, and a transport timeout caused by that limit is reported as such instead of a bare transport error.
- `scripts/release.sh` and the release workflow's CI gate list every failed job of the latest `CI` run (with links) when they refuse to release, instead of only reporting that no successful run exists.
- Windows native edits no longer fail Git-metadata protection on `\\?\C:\…` paths (the macOS profile's backslash refusal now applies only where a quoted sandbox profile is generated), the collaboration relay treats a peer that drops its socket (broken pipe or reset) as a normal disconnect rather than a relay error, and CI compiles the Wayland/PipeWire feature combinations on Ubuntu 24.04, whose PipeWire headers match `libspa` 0.9.
- Closing the terminal Dial runs in (a killed tmux server, a closed SSH session) no longer leaves Dial spinning at full CPU: the input reader treats a hung-up terminal as closed and exits instead of retrying its end-of-file forever.
- Ctrl+O and Ctrl+L on a long, tool-heavy session no longer end it with "terminal frame exceeds the byte limit": replayed scrollback is painted in bounded slices across frames. Leaving the terminal writes its mode resets (bracketed paste, keyboard protocol, cursor, autowrap) as a separate frame that is always attempted, so a refused scrollback write can no longer leave the shell in those modes.
- `auto` worker isolation falls back to the next backend when one passes its probe but fails to set up (an unprivileged or containerized kernel overlay mount without `fuse-overlayfs`), instead of failing every isolated task; the setup error is shown as that backend's skip reason. The worktree backend now gets the dependency copy, so workers there can run tests that need `node_modules`, virtual environments or `vendor`.
- A worker's private git snapshot excludes every build-output and dependency directory the copy backends exclude (`target`, `build`, `dist`, `.cache`, `__pycache__`, …); overlay isolation previously added a project's whole `target/` (tens of gigabytes) to a temp-dir repository for every isolated worker.
- Background workers stopped because their owner went away (quitting, or a settings change that rebuilds the agent) are recorded as `interrupted`, so `--restore` resumes them; only an explicit cancel records `cancelled`.
- One-shot notes (background job finished, monitor deliveries, hook context, the 80% wrap-up notice) are no longer lost when a reply is discarded after it arrives: a reply that never reaches history (truncation raise, rejected edit preview, repair, guidance regeneration) leaves its notes queued for the retry.
- A model that keeps answering in text while an investigation checkpoint is open gets two "call rewind" reminders, then the turn stops as incomplete; previously the reminder repeated without limit (each one a paid request, with the default unlimited step budget).
- Goal mode counts batched and streamed-reuse tool calls as activity; a continuation turn made of parallel reads is no longer treated as stalled.
- The prewalk and plan-yolo model handoffs install the new model through the shared route installer (authentication config, context cap, cache identity, role caches) and resize the turn's output budget, so a handoff to a smaller model no longer keeps the planner's context window and compaction thresholds.
- The patch engine no longer edits the first of several identical blocks when the hunk meant another: exact and whitespace-tolerant passes report how many places match, so a hunk's line number selects the right occurrence and a context-stripped fallback that matches more than one place is refused instead of applied.
- Patch re-indentation keeps a line the model deliberately re-indented (a dedent, or wrapping lines in a new block) instead of snapping it back to the file's previous indentation when the hunk was matched with different whitespace.
- A shell command that reaches the automatic background threshold keeps running in the foreground under its own timeout when no background owner can take it (`bg` not enabled, or all eight slots in use); previously the refused hand-off stopped the command and discarded its output.
- Captured shell output never splits a multi-byte character at the head/tail cut (box-drawing `tree` output no longer shows replacement characters when nothing was omitted).
- A line ending in `\r\r\n` keeps its own carriage return when an edit changes a different line.
- `grep` and `glob` scoped to a subdirectory honour the `.gitignore` files of its parent directories (build output such as `dist/` no longer consumes the page budget), while an explicitly requested ignored directory is still searched.
- A wide `grep` page no longer evicts the edit witnesses of files the model read or edited: new search-only witnesses enter at the old end of the store, and superseded snapshots are trimmed before whole files are evicted.
- A session whose last append was torn by a crash or full disk no longer reports corruption on every later resume: the writer removes the unloaded partial line when it claims the journal, instead of sealing it into a malformed interior line with the next append. A complete final entry that only lost its newline is kept.
- Checkpoint review diffs are bounded to 500 ms of edit-script search (a coarser but correct diff past that), and compare-before-restore checks the review token without rebuilding the diff while holding the checkpoint lock.
- Recheck session-writer identity after locking, retain locks through removal, version SQLite
  schemas transactionally, and preserve process-group ownership when waiting for child exit.
- Load Git-overlay status/diffs off the input thread, reject stale replies and unsafe hunk
  fallbacks, and route Unix termination signals through normal terminal cleanup without
  erasing scrollback. Bound relay admission and report actor panics to RPC clients.
- Honour bounded provider retry hints with exponential backoff/jitter; handle CR-only SSE
  framing and checked configuration deadlines. Clarify Linux test prerequisites before running
  the Rust suite, while continuing independent test binaries after failures.
- A refreshed OAuth token, a rotated pooled API key, an account or model switch no longer breaks Responses, Codex WebSocket and native-compaction sessions with "start a clean chat": native items owned by another account, endpoint, credential or model are never replayed, and those turns are sent in portable form instead. Gemini tool history from another model, route or credential is replayed as plain text rather than refused, without inventing function-call signatures.
- Anthropic signed blocks (thinking, server tool results) are recorded with the host that produced them (`dial_provider_origin`) and replay only there; a turn from another Anthropic-protocol provider (z.ai, MiniMax, Vercel, Cloudflare) is sent as portable text and tool calls. Claude subscription tokens are re-read on every retry attempt, so a retry that outlives the token no longer fails with an authentication error.
- A streamed Responses `context_length_exceeded` failure is classified as a context overflow (compaction) instead of a transient failure retried with the same prompt; `usage_not_included` and `usage_limit_reached` are quota exhaustion. The Anthropic "input length and `max_tokens` exceed context limit" refusal is recognized as an overflow.
- Chat-completions finish reasons are normalized: `max_tokens`, `model_length`, `model_context_window_exceeded` and `max_output_tokens` mean `length`, and `error`/`network_error` are incomplete streams, so truncated or aborted replies are no longer accepted as complete answers. A tool call cut off by the output limit on Gemini, Responses or Ollama is dropped and reported as `length` (raising the budget) instead of failing the turn.
- Idle cache warming stops once the combined cost of its replays would exceed the single cache miss they prevent (previously each replay was judged alone, so long idle windows cost more than the miss).
- A tool-call delta whose index is reused by a different call id starts a new call instead of concatenating two calls.
- The steering runtime probe no longer routes loopback requests through an environment proxy.
- Oversized images are resized once and reused on later requests (bounded cache) instead of being decoded, Lanczos-resized and re-encoded on every request; the saved model catalog is parsed once per file version; and request fingerprinting reuses the built request body instead of re-parsing the serialized request.
- A running tool's live output no longer grows its runtime body without bound: the body keeps the first 1 MiB (prefix-stable, so RPC byte deltas stay valid) and marks the cut once. A single runtime update too large for the stream journal's replay row limit is now published in a bounded form instead of latching the journal, which previously stopped all session persistence until restart; copying a journal refuses rather than silently dropping such a node.
- Stream-journal reclamation no longer rescans and rewrites the whole stream on every flush once retained live baselines exceed 4 MiB; it waits for another 4 MiB of growth, as after a deferred attempt.
- Editing one workspace crate no longer recompiles unrelated crates beneath it: `dial-tools` and `dial-agent` read the executable's build identity registered at startup (`dial_core::build_identity`) instead of linking `dial-build-info`, whose revision/dirty/time values change on every source edit. Touching a `dial-ui` file rebuilt in 27 s before and 11 s after on a 4-core dev build.

### Changed

- A streaming `/btw` side answer is saved (and shown) at most every 500 ms or 8 KiB after its first text, instead of rewriting and fsyncing the side-thread file on every refresh tick of the UI thread; only text the private store accepted is ever displayed. The periodic 30 s project rescan no longer hands an unchanged listing to the composer (which restarted a scan of up to 32 MB of source and repeated the "File completion is partial" notice), and the vocabulary scan is skipped when word completion is off. RPC republishes its query projection for runtime-only (worker) changes at most every 500 ms instead of copying the whole execution tree on every idle tick.
- Streaming markdown keeps its incremental cache for replies containing `[` or `<` (links, generics, comparisons, inline HTML) by cutting only at the parser's own top-level block boundaries; previously any such character made every later frame re-render the whole remaining reply. A 20 KB reply with one `` `Vec<T>` `` early rendered in 96 ms instead of 2.57 s across its deltas (debug build). The task card no longer re-renders every frame while a worker runs (live elapsed time is compared at the one-second resolution it is shown in).
- Request preparation validates the tool offer without rebuilding every tool's contract identity (previously about 12% of CPU in a tool-heavy local turn), checks for Snapcompact layers and user-supplied image references without copying the whole session history, compiles each cleanse parser pattern once, and keeps only the newest 256 truncated-artifact URIs (written only when one is added) instead of re-journaling an ever-growing list.
- `find` reuses its parsed-source evidence across calls on repositories larger than its cache: entries are indexed by path, the cache holds up to 4096 files (within the existing 32 MiB bound), and eviction never displaces an entry the current retrieval already used, so repeated sweeps hit instead of re-tokenizing every file. Fuzzy edit matching skips windows whose length-based upper bound cannot affect the result and normalizes each file line once instead of once per window. Idle background-job readers back off from 10 ms to 100 ms polling.
- Isolate release signing behind a protected environment, require exact-SHA push-to-main CI,
  publish curated version-specific changelog notes, and broaden platform lint/feature coverage.
  Container digest signing uses pinned Cosign with GitHub OIDC. Repository-owner protection
  settings and release verification limits are documented in `docs/release-and-install.md`.
- Replace direct `fs2` locking with standard file locks, consolidate filesystem/HTTP/OAuth
  and UTF-8 boundary owners, extract orchestration phases, and enforce documented unsafe
  invariants through the workspace Clippy lint.
- `verification.review.enabled` is now unset by default: completion review runs when a `coach`
  model role is assigned and its route passes the escalation privacy gate (local, or
  `escalation.allow_remote`). Explicit `true`/`false` keep their meaning; `strict_requirements`
  stays off (`docs/requirement-verification.md`).
- Dependency updates: ratatui 0.30 (on crossterm 0.29, so one crossterm version is built),
  quick-xml 0.42 (document XML is read as UTF-8 text; DTD refusal and entity rules unchanged),
  ast-grep 0.51, fontdue 0.9.5, unicode-normalization 0.1.26, and the release workflow's
  checkout v7, upload-artifact v7, download-artifact v8 (digest mismatches now fail), and
  Docker build-push v7 / login v4 actions.
- CI's Linux feature-powerset jobs share one Rust cache per runner image instead of one per
  combination (32 entries, ~6.5 GB of the 10 GB quota), so the macOS test caches are no longer
  evicted and the macos-15-intel job stops rebuilding every dependency from scratch. The Intel
  job now runs format, build and the full test suite only; installed-binary smoke, terminal
  lifecycle, slim build and runtime-overhead steps run on macos-15 and Ubuntu.

### Added

- GPT-, Claude-, Gemini- and DeepSeek-inspired reasoning workflow presets, adapted from public
  guidance/research. Existing strategies and the unchanged default remain available; presets do
  not select models or change provider thinking/effort controls (`docs/agent-loop.md`).
- Advisory edit-churn note: six committed edits to one file without a passing check prompt the
  model to verify or re-plan. It never stops the turn (`docs/agent-loop.md`).
- `/contract <task>` starts a turn that records goal, constraints and checkable "done when"
  deliverables in `working_state` first, and stops with a recorded `blocked` state when stuck. It
  reuses the existing completion gates (`docs/agent-loop.md`).

## [0.2.10] - 2026-11-01

### Added

- Experimental **Reasoning strategy** settings offer eight selectable prompt-guidance presets plus an unchanged default. Built-in main-agent prompts and new/resumed workers share the selected approach; custom main prompt replacements/templates opt out. This does not edit private chain of thought, change thinking controls, or relax role, approval, budget or verification boundaries. Outcome and token-cost effects are unmeasured (`docs/agent-loop.md`).
- `bash` accepts opt-in `pty: true`, launching through the existing managed terminal runner and returning a `bg` job handle for input, output, waiting, resizing and termination. Ordinary calls remain pipe-based. PTY mode rejects explicit `timeout` and named shell sessions, retains approvals and confinement, honors `--no-pty`, and never retries a pipe command automatically (`docs/shell.md`).
- Opt-in Snapcompact colored overlays preserve each compaction as a distinct 50%-opacity layer at the same coordinates. The existing `session_search` tool recovers preserved layer sources and generates clean text/image views; only the two latest successful views stay expanded in request projections. Existing compaction defaults are unchanged. Blended pixels are not an exact recovery channel; readability, model-recall and token-saving gains are unmeasured (`docs/agent-loop.md`).
- **Docker support.** The image now declares `DIAL_HOME` and a state volume, includes Git, creates `/workspace` and the state directory for the `dial` user, and uses BuildKit cache mounts. New `docker-compose.yml` and `docs/docker.md` cover `docker run`/Compose recipes (workspace mount, state volume, API-key passthrough, TTY), and the sandbox options Bubblewrap needs in a container (`seccomp=unconfined`, `systempaths=unconfined`). CI builds the image, runs `--version`/`--help` from it and validates the compose file.
- **Docker Model Runner ("Docker AI")** is discovered as the `docker-model-runner` provider at `http://127.0.0.1:12434/engines/v1` (`DOCKER_MODEL_RUNNER_BASE_URL` overrides it; containers use `http://model-runner.docker.internal/engines/v1`). `*.docker.internal` hosts (`host.docker.internal`, `model-runner.docker.internal`) now count as local hosts: plain `http` is accepted and requests bypass the proxy and are treated as local inference.
- **Requested run targets.** When the latest user request starts with a run verb (`run`, `launch`, `start`, `execute` or `play`, after filler such as "please" or "can you") and names an existing workspace path, completion is refused until a `bash` or `bg` run that names that path was observed to stay running, reach readiness or exit zero with no later edit, opaque shell command or modifying extension call after its launch. Both plain replies and `finish` are covered. Passing checks on another program, plain inspections such as `ls` or `cat`, and an unobserved `bg start` do not count. A refusal after a failed run without a terminal states the exit status and launch facts, and notes when a `bg` launch with `pty: true` is available but was not tried. `completion_evidence.requested_targets` records the runs. This is process evidence only; behavior is reported as `not_assessed`. `bg start` results now carry structured `job` data (`name`, `pid`, `terminal`), including when readiness fails. Disable with `verification.target_evidence = false`. Request matching is lexical and deliberately narrow. Scripted offline regressions replay both observed failures; live-model effects are unmeasured (`docs/requirement-verification.md`).
- **Tool discovery for local models.** A call rejected for an argument its tool does not declare now loads the unoffered catalog tool that declares it (at most three, never an unloaded one) for the next request and names it in the refusal; constrained repair is skipped for that call. A final answer that says a capability is unavailable, given before any `tool_search`/`tool_load` in the instruction, is sent back once with a request to search the catalog. Lean and one-call prompts list each native loadable tool with a one-line description (bounded), keeping MCP tools, placeholder families and overflow on the `Names:` line. Running `bg` through `bash` ("no job control") now gets the existing tool-not-shell guidance, and the `bash` description and automatic-background notice say to load `bg` when it is not offered. The denial check is lexical; live-model effects are unmeasured (`docs/agent-loop.md`).

### Changed

- Named embedded-shell sessions save only exported changes from the inherited host environment, preventing large CI environments from exhausting the bounded state exchange. The 128-variable and 256 KiB limits and protected-variable filtering remain in force (`docs/shell.md`).
- Recovery guidance for local-model mistakes. Calls refused for their own arguments (a missing required argument, an unknown tool) are classified `call_contract` and say to fix the arguments and retry, instead of the boundary's "do not retry or bypass"; the result status is unchanged. Replace-mode misses lead with the line where `old_string`'s first line occurs; identical `old_string`/`new_string` absent from the file is refused before searching as code from an earlier refused edit; an `old_string` covering most of a 40+ line file points to `write`. Syntax refusals say nothing from the edit landed, name the identifiers it would have added, and suggest smaller edits or `write` for large changes. `todo` accepts `done: true` and `task: N`, its unknown-key correction keeps the given number, and a `done` after a refused `edit`/`write` in the same turn carries a note. A local route that replaces an explicit `[edit] mode` shows a one-time notice with the `assistance.local.edit_mode = "inherit"` switch; precedence is unchanged. Live-model effects are unmeasured (`docs/tools.md`, `docs/agent-loop.md`).
- Runtime failure recovery distinguishes unknown check causes from affirmative code failures; native edit retries use typed current-source/range witnesses; bounded capability-aware advice preserves producer data; tool-loading receipts report effective state and search readiness breaks relevance ties only; failed mutation prerequisites withhold dependent unstarted actions while retaining diagnostics/disjoint work; and configured precommit syntax blocking covers native edits, whole-file writes and AST apply (including preview tokens). Recovery suggestions never execute or authorize actions. Unsupported grammars and explicit syntax overrides retain their meaning. Built-in full guidance drops the duplicate whole-file pre-read instruction now enforced by native witnesses; tool descriptions and edit preference remain. Live-model reliability and prompt-cost effects are unmeasured (`docs/agent-loop.md`, `docs/requirement-verification.md`, `docs/tools.md`).
- Hidden text-embedded tool calls now keep the live generation counter and silence timer advancing while their syntax streams. Raw tool markup stays hidden and is not treated as a validated or executable call; engine-reported counts still take precedence over estimates (`docs/terminal-interface.md`).
- Identical full-file writes now use the shared no-change refusal instead of committing and reporting another overwrite. Real replacements retain read-witness and transaction safeguards. Per-model Edit style shows its inherited grammar and explains that inheritance follows Dial's policy rather than adaptive benchmarking (`docs/tools.md`).
- Built-in full and compact guidance now distinguishes routine tool-log narration from brief user-facing findings, decisions, changed plans and blockers. This is prompt guidance, not a guarantee that a model emits prose or reasoning on every reply (`docs/terminal-interface.md`).
- MLX vision requests now translate the existing template thinking option and local thinking hints to the server's top-level `enable_thinking` field, using checkpoint vision metadata rather than model-name guesses. Explicit reasoning controls and server defaults remain authoritative; text MLX requests retain their existing wire format. This fixes request mapping, not a guarantee of reasoning or prose on every reply (`docs/llm-transport.md`).

- Standalone `todo` updates no longer receive advice to batch another call when the active request permits only one call, including the small local tier and reliability recovery. Todo remains optional, and tool eligibility and permissions are unchanged. This fixes contradictory guidance; live-model feature-use effects are unmeasured (`docs/agent-loop.md`).

- Runtime recovery is controller-enforced without growing corrective prompts: PTY starts include bounded initial screen/process observation; failed file-operation retries track their own source prerequisites; conservative native `already_applied` edits are observational and side-effect-free; requested-run evidence retains launch-time source identity and becomes stale after external changes; rejected completed candidates end honestly with their unmet prerequisite; and parent/worker non-progress shares a finite policy before final publication. Existing approvals, confinement, explicit readiness, configured escalation and cost limits remain. Process observations are not behavioral verification; live-model effects are unmeasured (`docs/shell.md`, `docs/background-services.md`, `docs/tools.md`, `docs/agent-loop.md`, `docs/requirement-verification.md`).

- Shell launch context now has a typed controller-owned representation shared by foreground, named-session and managed shell routes, including job observations. Parent and worker recovery distinguish terminal/input requirements from capability absence and provide eligible launch/control or deferred-tool guidance without replaying commands or overriding policy. Auto-background adoption no longer counts as observed running solely from its launch receipt. Generic terminal and stdin recovery benchmarks verify the original probe and input receipts; real-model outcome improvement requires measurement (`docs/shell.md`, `docs/agent-loop.md`).

- Subagents spend less time and context on coordination. Handoff briefs reference, instead of repeating, user messages equal to the current request or session goal; resumed workers no longer receive another copy of the saved codebase context; and the per-delivery "Coordination update" note is replaced by one line in the child system prompt. Delegation and test-runner guidance now starts test-runners after their inputs settle and collects with `wait` instead of readiness holds, steering and status polling, and workers carry out scoped changes without stopping to ask for confirmation (`docs/subagents.md`).

- Recovery now accounts for productive work once per assistant round rather than treating every successful bookkeeping call as progress. Isolated workers preflight whole patches and roll back guarded installations on failure; optional configured acceptance checks bind promotion to stable declared inputs, independently of worker completion claims. Native rendered-image receipts bind delivered pixels to exact targets, tool discovery exposes conservative prerequisite metadata, and operator-mapped deliverable contracts require current observations. Superseded per-call resets, partial-conflict promotion and tool-name visual credit are removed. These safeguards grant no permissions and do not establish semantic correctness or measured live-model improvement (`docs/agent-loop.md`, `docs/subagents.md`, `docs/requirement-verification.md`, `docs/tools.md`).

- Runtime task control preserves declared deliverables across notes-only updates and requires explicit human confirmation to remove, cancel or redefine them. Completion evidence and advisor review reconcile claims with current linked checks without certifying unverified references. Controller-observed capability gaps can offer eligible specialists; a tool-search miss returns a bounded catalog page directly, replacing the empty-search recovery prompt. Failed file-operation retries retain path-local prerequisites instead of being released by unrelated edits. Permission, unknown-effect and verification boundaries remain; live-model completion/recovery improvements are unmeasured (`docs/agent-loop.md`, `docs/tools.md`).

- Native shell results report launch context separately from exit status, including zero-exit application diagnostics. Structured data and a short factual note describe that launch's stdin, captured output and lack of an allocated terminal; permissions and process status are unchanged. Live-model outcome effects are unmeasured (`docs/shell.md`).

- Print runs (`dial -p`, `dial review`) defer `task`/`message`, `working_state`, `codebase_memory`, `todo` and `find` to the loadable list, and drop the delegation and project-map prompt sections that depend on them. `task.prefer_subagents=true` keeps `task` offered. In a Terminal-Bench pilot, print runs never called `find` (`grep`/`glob` cover it), and `todo` updates took 17 calls, often a whole request each. In the offline default-offer test, tool schemas shrink from 32,180 to 19,246 bytes. Outcome and latency effects are not measured (`docs/agent-loop.md`, `docs/rpc-and-acp.md`).

- Automatic tool routing no longer adds specialist schemas from incidental words in a request. Code spans and identifiers (`$request_method`, `server.py`) no longer count as intent. "Your task is to" no longer offers `task`. A web server to build no longer offers `web_search`, a custom error page plus "verify" no longer offers `browser`, and keeping a service running no longer offers `monitor`. On the Terminal-Bench nginx task, the routed offer shrinks from 48.0 KB to 24.9 KB of schemas, which every request of the run carries. This is an offline measurement; live token savings are not yet measured (`docs/agent-loop.md`).

- Print runs now say when a running `bg` job will be killed as the run exits. When the turn would end, or on the first `finish`, the model is told once which explicitly started session jobs are still running. It is pointed to `bg persist=true` for a service the task still needs, or told to stop the job, and told not to report it as still running. In a Terminal-Bench pilot, servers started with `bg` (kv-store-grpc, pypi-server) were gone by the time the verifier ran. Offline regression only; the outcome effect is pending the Harbor rerun (`docs/rpc-and-acp.md`).

- Local-host agent requests now inherit the serving API's generation defaults by omitting unset temperature and output controls. Explicit Dial overrides, including a temperature explicitly set to the former 0.2 default, remain authoritative. Automatic Ollama context no longer sends `num_ctx` or silently applies a memory estimate; reported loaded capacity is used for planning. Residency extensions, llama.cpp cache overrides and repetition penalties now default off, with existing opt-ins preserved. Request-body regressions are offline; live-host behavior and outcome effects are unmeasured (`docs/agent-loop.md`, `docs/provider-catalog.md`, `docs/llm-transport.md`).

- The status bar (`temp … · out …`) and the `/policy` local request settings now show the local server's own value for a generation setting Dial leaves to it, such as `temp 0.6 (provider) · out unlimited (provider)`. Values come from llama.cpp `/props` `default_generation_settings` and Ollama `/api/show` Modelfile `parameters` (temperature, top-p, top-k, min-p, output allowance), read once per session. Previously the output allowance showed Dial's internal planning budget although no limit was sent, and an omitted temperature showed `unset`. LM Studio reports no sampling defaults, so its omitted fields read `provider`. Requests are unchanged. Offline parser and rendering tests only; live servers unmeasured (`docs/llm-transport.md`).

- Non-hermetic parent requests now repeat bounded runtime-recorded latest-user context at the request tail for ordinary correction turns as well as live steering, without promoting drafts, peer or monitor text into user intent. Explicit-target discovery guidance explains scoped inspection of ignored directories rather than substituting similarly named fixtures. Search exclusions and permissions are unchanged. Focused offline regressions cover context delivery and scoped discovery; live-model outcome effects are unmeasured (`docs/agent-loop.md`, `docs/tools.md`).

- Native write preflight and controller edit/write binding preserve typed path-access refusals, including relative and symlink escapes and protected storage. Recovery respects the permission boundary instead of treating a refusal as an edit-contract failure; malformed paths remain ordinary errors. Sandbox enforcement is unchanged (`docs/tools.md`).

- Local recovery applies a reached escalation step at the next model-request boundary within the same turn, refreshing the prompt, offer and edit grammar together while preserving pins and budgets. Unrelated peer lifecycle chatter no longer regenerates automatic roster notes; direct messages remain delivered. Recipe checks and isolated candidate verification now share structured focused-check verdicts instead of trusting printed exit markers. The local benchmark suite adds an offline movement/harness-repair regression, including skipped-test rejection. Live-model outcome, cost and latency effects are unmeasured (`docs/agent-loop.md`, `docs/session-coordination.md`, `docs/requirement-verification.md`, `docs/benchmarks.md`).

- Goal continuations and critical carry now preserve runtime-recorded original user-message provenance without promoting harness prompts into user intent. Optional working-state deliverables connect outcomes and evidence references to completion blockers, while goal boundaries flag unchanged recorded progress without a new stop rule. The drafter remains opt-in; references and model statuses are not verification certificates. This correctness change has no measured live-model outcome, cost or latency improvement (`docs/agent-loop.md`, `docs/tools.md`).

- Approval and question prompts that arrive during full-screen menu navigation now temporarily
  yield the alternate screen and restore the same menu afterward, instead of painting inline
  output over the menu (`docs/terminal-interface.md`).

- Full and compact parent prompts now share testing-decision guidance: reuse sufficient existing coverage, add tests only for specific uncovered risks, and broaden focused runs only for affected interfaces, unresolved risk, explicit requests or required gates. Worker/test-runner guidance follows the same rule; check enforcement is unchanged. Test-authoring time and live-model effects are unmeasured; no outcome improvement is claimed (`docs/agent-loop.md`).

- Automatic source-tool routing distinguishes output target phrases from inputs to inspect (for example, analyzing text logs and writing `summary.csv`, including comma-separated instructions). Real source inputs and explicit tool requests remain eligible; commas inside filenames are preserved. This correctness fix reduces irrelevant schema offers; live-model token, latency and success effects are unmeasured (`docs/agent-loop.md`).

- Compaction now uses offline model-aware counting for explicitly recognized OpenAI IDs, full-coverage bounded fragments instead of single-message truncation, opportunistic live-prefix tool-disabled Handoff with portable fallback, and conditional cloud-only split-summary concurrency through existing provider slots. Recoverable pruning stays opt-in and batches actual net savings after artifact-pointer costs. Exact critical carry, final projected-request checks and non-destructive failures remain. Live-provider cost, latency, cache-hit and recall effects are unmeasured (`docs/agent-loop.md`).

- Snapcompact auto presets now follow the reading model across gateways: Claude uses 11px tracking, Gemini/GPT/Kimi use 22px leading, and GLM uses 8on16. Opus 4.7+/Fable/Mythos and Gemini receive OMP's larger frame sizes; size/host-edge and image-detail overrides are exposed in settings. Modern GPT OpenAI routes request original detail. Exact-source recovery, conservative billing and no-truncation fit checks remain; equal recall or token efficiency has not been benchmarked (`docs/agent-loop.md`).

- Long-history housekeeping now uses incremental worker-activity projections, shared token/wire accounting, dirty-node runtime flushing and candidate-scoped ambient source validation. Content hashes and scope coverage remain authoritative for remembered findings; no stat-only freshness cache is introduced. The offline runtime baseline adds deterministic long-history workloads without claiming model-quality gains or a measured end-to-end speedup (`docs/benchmarks.md`, `docs/subagents.md`, `ARCHITECTURE.md`).
- `tool_search` can enumerate eligible tools in bounded pages without activating them, giving models a general fallback when a capability search misses (`docs/agent-loop.md`).
- Plan mode can discover read-only computer actions without permitting desktop input (`docs/agent-loop.md`).
- Local-model recovery guidance warns against depending on an edit that failed, empty glob results explain literal whitespace, and explicit visual-inspection requests enter the existing visual-evidence tracker (`docs/tools.md`, `docs/agent-loop.md`).
- Local escalation retains an edit-failure streak across one intervening successful round, so a recovery read does not hide a repeated edit failure; two successful rounds clear the streak (`docs/agent-loop.md`).
- Recorded model speed is labelled end-to-end in model details and stats because request time includes prompt processing. Edit witness errors give a targeted-read recovery hint (`docs/terminal-interface.md`, `docs/tools.md`).
- Child worker model requests now announce when they wait for a provider concurrency slot, including local servers (`docs/subagents.md`).
- Research checkpoints now retain their concrete finding and next action as unverified follow-through guidance instead of inviting another broad survey. Internal checkpoint JSON is journaled separately from assistant answers and is not streamed or replayed as an answer. Advisory retrieval guidance favors scoped missing evidence; local request-setting labels distinguish Dial's intent from unconfirmed backend behavior (`docs/agent-loop.md`).
- Focused local-model guidance moves from a supported target to a concrete hypothesis, a discriminating check and reversible implementation, while preserving advisory answers and focused clarification when scope or correctness is ambiguous (`docs/agent-loop.md`).
- Foreground turns and child workers now share one reply triage (`agent/run/reply_triage.rs`) for output-limit recovery (reasoning gate, raised limit, continuation, stop) and unusable tool calls, and one request-tail assembly step (`run::guidance::assemble`). Workers therefore merge their plain harness notes into one message per request like the parent, classify form failures after inline-edit recovery (a recovered inline edit is no longer counted as a form failure), and send the parent's fuller correction for a dropped tool call with invalid JSON. No outcome change is claimed; the effect on model outcomes is unmeasured (`docs/agent-loop.md`).
- Training captures record the conversation in the message layout actually sent on the route, for example one merged system message on loopback Chat Completions endpoints, instead of the canonical multi-system layout (`docs/improvement.md`).
- Contributor rule: changes to model-facing wording or tuning thresholds state before/after benchmark evidence or that no outcome change is claimed (`CONTRIBUTING.md`, PR template).
- Local-model resilience: oMLX's "prefill memory guard rejected this prompt" error is now classified as context overflow, so the turn compacts and retries (up to the existing overflow limit) instead of ending as a fatal error (`docs/llm-transport.md`). Research checkpoints accept a valid checkpoint JSON that arrives beside requested tool calls (the calls are still discarded, never run) or inside one ```json fence; a tool-only reply gets a correction naming the tools and showing the expected shape. The three-attempt limit is unchanged; the effect on local-model outcomes is unmeasured (`docs/agent-loop.md`).
- Skill, memory, capability discovery and peer-list lookups now count toward the read-only research window alongside source reads; bookkeeping does not reset it. The window itself is unchanged: synthesis is requested after 12 observation-bearing rounds and the tool-free checkpoint is enforced after 24. Read-only work can still resume after evidence-linked synthesis, without edits or manufactured todos (`docs/agent-loop.md`).
- A `read` refused by workspace confinement can be retried unchanged after an approved directory grant makes its target accessible; unrelated failure barriers and access protections remain in force (`docs/agent-loop.md`).
- Completion guidance respects a user's requested final-response format when work is complete and no blocker, unverified check or safety-relevant gap needs disclosure, rather than always asking for a multi-sentence report (`docs/agent-loop.md`).
- Benchmark integrity: machine runs no longer inject suite grader commands into the isolated child configuration or context. Strict-contract variants now report unavailable unless an explicit, valid agent-visible verification contract is supplied, rather than passing on suite-derived obligations. Non-strict agents no longer receive suite-derived verification commands; independent grading is unchanged. The recorded `case_requirement_mapping` is now `none_grader_independent_v2` (`src/cmd/benchmark/mod.rs`, `docs/benchmarks.md`).
- Local-model research routing now offers the eligible web-search/fetch pair even in lean/minimal
  profiles, ahead of incidental MCP matches. Discovery results include explicit load status and
  next action; empty searches and tool names mistakenly invoked as shell programs get actionable
  guidance without changing tool permissions or releasing the unchanged-retry barrier
  (`docs/agent-loop.md`).
- Measured local interfaces bind fixture evidence to context-window and sampling settings and use
  one snapshot for each turn's prompt, contract and diagnostics. Opt-in measured native-tool failures
  seed the minimal offer at any window without overriding edit pins. Reliability reports separate
  configured policy, server facts, saved measurements and session selection (`docs/agent-loop.md`).
- Added five opt-in local task benchmarks for cited comparison, deferred discovery, empty-discovery
  recovery, read/edit/test repair and denial-stop. Independent outcome checks and integration-path
  traces stay separate; offline mutation tests reject weakened visible tests and swapped citations.
  Local-only and explicitly remote hosted-reference profiles are separate templates
  (`examples/benchmarks/local-tasks.toml`, `docs/benchmarks.md`).

- Local OpenAI-compatible chat-completions endpoints, including LM Studio, now use the same single-system-prefix message adaptation as Dial-launched local models. Later and transient instructions stay at their original conversation boundary as user messages, fixing strict-template errors such as `System message must be at the beginning` without rewriting saved history. Hosted routes and other API protocols are unchanged (`docs/llm-transport.md`).

- **Independent worker** now starts its worker immediately, even while the model is streaming a reply or tools are running, instead of at the next step boundary. The running reply and tools are untouched; a worker that cannot start still queues the message as a follow-up.
- The terminal tab no longer changes color (iTerm2 OSC 6 is gone). A colored circle now ends the title instead: orange while working, red when an approval or question waits or a turn failed, green when a turn finished, none when idle; it works in any terminal that shows titles and is omitted with ASCII glyphs or `ui.terminal_integration = false`. Keeping the circle at the end fixes it disappearing when iTerm2 truncates an unselected tab's title from the left. The title also names the session: `dial · <project> · <session title>` (its `/rename` name, else its first message), updated after each turn, rename, resume, new session and fork. Dock attention is unchanged.
- The launch screen keeps moving after the start-up sweep when `ui.motion` allows: the knob's needle breathes slowly across the next tick mark and a glint of light crosses the knob and wordmark every twelve seconds, at 8 frames a second until the first message (`welcome::Home::ambient`, `fx::AMBIENT`). The default activity spinner is now a two-dot comet circling a braille cell like the needle. Nothing moves with `ui.motion = false`, `NO_COLOR`, ASCII glyphs or without truecolor, and the ASCII spinner is unchanged.
- `/settings` now follows a fixed layout table (`LAYOUT` in `crates/dial-llm/src/config/schema.rs`): tabs run Appearance, Interaction, Model, Tools, Memory, Improvement, Voice, Experimental, and sections inside each tab lead with the most-used ones. A test requires every setting to sit in a listed section. The section list marks sections where you have overridden settings with a `● n` count. Configuration keys and saved files are unchanged (`docs/configuration.md#settings-layout`).
- `ui.notify_errors` now defaults to `true`, so a failed, blocked or incomplete turn notifies like a finished one when `ui.notify` is on. Set `notify_errors = false` to restore the old behavior; explicit values are preserved (`docs/configuration.md#default-baseline`).
- Added `web.timeout_secs` (default 30, range 1..600) for `web_fetch` and `web_search`, plus `/settings` rows for it, `web.search_provider` and `web.jina_reader` under Tools → Web.
- Turn efficiency reports now attribute request phases, prefix churn, decoded reply/argument bytes, recovery attempts and requests before the first observed workspace change. Inclusive backend timings measure verification input capture/reuse, source hashing and edit preflight; process-local storage counters expose sync, runtime-flush and control-publication costs without skipping acknowledgements or equal-state journal events. These measurements do not establish a live-model speedup (`docs/agent-loop.md`).
- Cross-agent benchmark reports now retain nested turn-efficiency diagnostics, executable identity, seeded balanced trial order and immediate-exit stderr pointers; Dial uses a fresh home per trial and concurrent runs are labelled unsuitable for latency comparison (`docs/benchmarks.md`).
- Added adversarial ledger reliability fixtures with independent acceptance commands and mutation tests that reject partial fixes even when visible tests pass (`examples/benchmarks/reliability.toml`).
- Added an opt-in local SWE-bench Verified Dial/omp adapter with frozen task/image identities, shared CLI execution and Docker test access, standard patch predictions and official grading. Missing grading evidence remains unavailable; installed omp configuration and ARM emulation are explicit comparison limitations (`docs/benchmarks.md`).
- Added a macOS loopback-only, serial three-way overhead comparison for frozen Dial baseline/candidate binaries and omp, with scripted tool outcomes, independent file checks, fresh homes, egress self-tests and retained request-byte accounting. Synthetic usage is explicitly not a token, cost or cache-hit measurement (`examples/benchmarks/offline_compare.py`, `docs/benchmarks.md`).

- The skill hint no longer asks for a standalone `skill` call: it says to load a matching skill in the same response as the first reads (`crates/dial-storage/src/storage/skills.rs`). `check` refuses a top-level `command` with the exact `repro` contract instead of an unexplained error. A `repro` that is not an object, such as a JSON-encoded string that does not parse, now gets a shape error that names the JSON problem, not a misleading `repro.name` error (`crates/dial-agent/src/agent/verify/check_tool.rs`).
- `experimental.repro_first` steers a debugging task toward a failing reproduction only when the request explicitly asks for verification (a word starting with "verif", such as "verify" or "verification"). A plain fix request ("Fix the bug in calc.add") no longer gets the reproduce-first note after its first source edit, so the model decides how much reproduction the fix needs. That note prompted extra repro registrations and requests on routine fixes (`crates/dial-agent/src/agent/verify/repro_first.rs`, `crates/dial-agent/src/learn/task_policy.rs`, `docs/agent-loop.md`).
- `make preflight` (`scripts/preflight.sh`) runs CI's checks on a clean snapshot of a commit: fmt, clippy for the host and Linux, cargo-deny, musl and Windows compile checks via `zig`, the slim build, every contract and Python suite, the workspace tests in the system temp directory, and the smoke and real-terminal suites. Passing trees are cached. `make hooks` installs a pre-push hook that runs it; `scripts/release.sh` runs its quick compile/lint gate (`docs/release-and-install.md`).
- The Rust test suite runs under `cargo-nextest` in CI and `make preflight`, so test binaries run in parallel instead of one after another. Three tests that spent minutes waiting now finish in under a second: a reconnect test that waited out the 300 s production retry backoff, and benchmark-contract tests that re-hashed the ~260 MB test binary for every contract (the executable digest is now computed once per process, and `sha2` is optimized in dev/CI builds).
- Opening the interactive shell and loading configuration are faster: settings and resource watchers are registered in one batch (on macOS each separate registration restarted the FSEvents stream, about a second per shell), and resolving a local MLX model's context window inspects only that model instead of checking every shard of every downloaded model (`crates/dial-app/src/app/resources.rs`, `crates/dial-llm/src/llm/catalog.rs`). The `dial-app` tests that build a shell drop from about 430 to 90 test-seconds. Loopback image-generation fixtures stop when the test finishes rather than idling for seconds, and two timing tests no longer fail under a parallel nextest run.
- The first model request no longer waits for configured MCP servers to finish connecting. Startup connects them in the background; servers still connecting join at the first request of a later turn, and `tool_search`, `mcp_servers` or loading an unregistered tool wait for them. Exiting before they connect cancels the connection instead of waiting out its startup budget. With three `npx` servers and one remote server configured, this blocked every `dial -p` run for about 1.5–2 s before its first request. At exit, every MCP server now shuts down at the same time. Before, each stdio process stop and remote session deletion waited for the previous one, which added about 0.2–0.4 s after the final result (`crates/dial-tools/src/tools/integrations/mcp/mod.rs`, `crates/dial-tools/src/tools/integrations/ext.rs`, `docs/extensibility.md`).
- Every request in an efficiency-enabled session is smaller. The compact policy is one locate → change → prove → test → report loop and keeps every safety contract. It leaves tool mechanics to the tool descriptions and to the harness refusals that already enforce them, and restores the test-design, project-map and LSP rules when those tools are offered. It is about 7 KB, down from about 14 KB for the full policy (a byte-budget test guards it). Built-in loadable tools are listed by name when `tool_search` can describe them. The system prompt names `side_request` and `task` only when they are offered, and no longer repeats the mechanics of `tool_search`, `working_state`, `artifact_read` and `task`. The `task`, `bg`, `grep`, `find`, `todo`, `bash`, `glob` and `read` schema descriptions are shorter. Parameter shapes and contract hashes are unchanged, so recorded sessions still resume (`crates/dial-agent/src/agent/prompts/system.rs`, `crates/dial-agent/src/agent/window/prompt.rs`, `crates/dial-agent/src/agent/base/lifecycle.rs`, `docs/agent-loop.md`).
- Model requests to the same remote host reuse a warm connection. Each request previously opened a new TCP and TLS connection, about 0.1 s per model round. A connection is shared only after its reply was read to the end, and each borrowing request gets its own cancellation and silence limits. Loopback/LAN hosts, proxied connections and agents with a caller-supplied resolver never share (`crates/dial-llm/src/llm/cancel/reuse.rs`, `crates/dial-llm/src/llm/stream.rs`, `docs/llm-transport.md`).
- The skill hint suggests a skill the request does not name only when at least three distinctive description words match, not two. Words of coding work in general ("verify", "behavior", "wrong", "test", "change" and similar) never count, since nearly every request contains them. A bug report like "Checkout applies the wrong discount … verify its public behavior" previously shared exactly three such words with the debugging skill, and every trial of that benchmark case loaded the skill before reading any code. A skill named in the request still matches by name (`crates/dial-storage/src/storage/skills.rs`).
- The `repro_first = "advise"` note, which follows a debugging fix already applied, asks for a focused check of the reported behavior, such as one asserting command. It no longer asks to show the bug reproduces, which after the fix only led to new regression tests or a `check` repro written after the fact. `gate` still asks for a reproduction before the edit (`crates/dial-agent/src/agent/verify/repro_first.rs`, `docs/agent-loop.md`).
- `check` refuses to register a `repro` that passes before any change in the turn: it does not reproduce the bug, so the final gate does not rerun it. The result asks the model to fix the command's exit logic (for example an inverted assertion) and register it again (`crates/dial-agent/src/agent/verify/check_tool.rs`).
- The batching rule now also covers independent edits and asks for no narration of routine tool calls (`crates/dial-agent/src/agent/prompts/system.rs`).
- The cross-agent benchmark runs Dial with its default configuration: an isolated `DIAL_HOME` under the run directory, sharing only subscription sign-in through `DIAL_SUBSCRIPTION_AUTH_HOME` (`examples/benchmarks/cross-agent.toml`, `docs/benchmarks.md`).
- A turn that edited Python files ends about 4 s sooner (measured with three edited files against a local scripted model: 5.2 s → about 0.7 s between the final answer and exit). The automatic AST check reuses results by snapshotting the installed interpreter's standard library (about 8,500 entries). That snapshot ran around fifteen times per turn end: before and after every check, and in every input revalidation. Each run took 0.6–0.75 s, because every entry was re-resolved through two `realpath` calls and an `lstat` per path component. Now the parents of a directory's members are checked once, the installed runtime is not re-resolved inside its own policy-free workspace, runtime files get one metadata observation, and one revalidation pass shares a single runtime observation. A check's own before and after captures stay independent. Path resolution also skips canonicalizing for sensitive-root matching when no sensitive root is granted (`crates/dial-agent/src/agent/verify/inputs.rs`, `crates/dial-agent/src/agent/verify/inputs/automatic.rs`, `crates/dial-agent/src/agent/verify/checks.rs`, `crates/dial-tools/src/tools/mod.rs`, `docs/agent-loop.md`).
- A resumed session (`dial -p -c`) again counts files read with a multi-file `read` (`paths`) in an earlier turn as read. Resume rebuilt read witnesses only from single-path reads, so the first edit of such a file in a follow-up turn was refused with "no read of this file is recorded", and the identical retry after a re-read was withheld by the retry barrier. In the corrected-requirement benchmark case this cost two failed edits and a whole-file rewrite per trial (`crates/dial-tools/src/tools/edit/hashline.rs`).
- Docker Model Runner tool requests no longer fail with `Failed to initialize samplers: failed to parse grammar` when a nested tool parameter has a large string or array bound. Only the outbound generation schema omits length bounds above 1024; Dial still validates returned arguments against the original tool contract (`docs/docker.md`).
- "page" alone no longer marks a request as visual work: it also means pagination ("keep the page size at 25"), so with an ordinary verb such as "change" it appended "Visual inspection was not completed" to answers on Python-only tasks. It now needs an appearance word such as "layout", "looks" or "spacing" (`crates/dial-agent/src/agent/verify/visual.rs`).
- The failed-operation retry barrier no longer withholds an `edit` or `write` after its target file has been read successfully. The read supplies the witnesses the failed call lacked, so the identical call may be sent once more; if it fails again the barrier re-arms. Before, the model had to change the arguments or rewrite the whole file after an edit refused for a missing read (`crates/dial-agent/src/agent/run/engine.rs`).
- Workflow and failure guidance distinguish confirmed unrelated pre-existing failures from regressions in untouched code; a changed caller can still cause a check to fail. The compact report step asks for a brief outcome, checks and gaps without requiring headings for small changes (`crates/dial-agent/src/agent/prompts/system.rs`, `crates/dial-agent/src/agent/prompts/recovery.rs`).
- `find` can include bounded, secret-screened source excerpts for top code hits using the ordinary editable snapshot reader. Complete returned rows carry edit witnesses, including after session resume if the file is unchanged; ranked snippets and related summaries do not. The workflow also describes issuing an already-known edit and focused verification in one ordered tool batch, without executing the check in parallel with the edit (`docs/tools.md`).
- Lexical `find` now rejects files that cannot match the query before building a syntax index, without caching query-dependent misses or relaxing fresh reads, hashes and scope checks. Related evidence stops parsing reference candidates after its quota is full while continuing to look for requested types and configuration references. A single `find.grep_keywords` string is normalized to one literal keyword before validation, avoiding a repair round for that observed pilot error; other tools and array parameters remain strict. Focused tests demonstrate reduced syntax work, not a measured SWE-bench latency improvement (`docs/tools.md`).
- Prompt-cache warming follows the active model route instead of replaying through the initial provider/configuration after a switch or fallback. Queued stop events take precedence over due paid replays, queue overflow stops warming safely, and an outstanding run guard cannot keep a dropped warmer alive. Local startup health and embedding warm-up requests now observe cancellation, including with an unlimited startup timeout (`docs/llm-transport.md`).
- WebSocket TCP sockets, including proxy tunnels, now use the same TCP keepalive settings as HTTP sockets. Expired Codex sockets are closed outside the shared session-cache lock so their network writes cannot block unrelated sessions from accessing that cache (`docs/llm-transport.md`).
- The `/peers` panel now refreshes itself about every two seconds while open, so sessions that start or end in the checkout show up without pressing `r`. The selected peer stays selected, the panel holds still while a message is being typed, and an unavailable registry leaves the last board in place (`docs/session-coordination.md`).

### Added

- `dial benchmark --ablate-assistance` gains `ablate-structured_tools` (bash-only tool roster) and `enable-shake_before_summary` (elide before summarizing) rows, and machine profiles accept `assistance.bash_only` and `efficiency.compaction_method_order`, so the harness-design findings in arXiv 2609.20804 can be measured per model and context window. No defaults change (`docs/benchmarks.md`).
- Foreground turns suspend inference at a tool-free yield or `finish completed` while owned background jobs, live workers or side questions are pending. Completion/messages, user input and stop boundaries resume the existing lifecycle without model polling; explicit services, approvals and verification requirements are unchanged. Worker and side-question waits reuse the platform wake channel (`docs/agent-loop.md#event-driven-continuation`).
- Peer messages are delivered when delivery is safe instead of at a turn boundary. A turn suspended on background jobs, live workers or side questions now resumes as soon as fresh peer mail arrives (the awaited work keeps running), and messages sent from the side pane during a turn go straight to the peer's inbox instead of waiting for the turn to end (`docs/session-coordination.md`).
- The parent now monitors the subagents it started. A running worker's status carries `running_secs`, `idle_secs` and `health` (`active`, `slow` or `stalled`), derived from a new `updated_ms` stamp that worker progress and tool starts write. A worker silent for `task.stall_secs` (default 300, the remote idle timeout; 0 disables) is flagged `stalled` once with a background notice, and a `task` `wait` stops early for it without cancelling anything; a worker still progressing after three times that run time is `slow` (`docs/subagents.md`).
- `task` actions that do not spawn (`status`, `wait`, `cancel`, `peek`, `close`) no longer build the child tool harness, and the worker evidence audit counts cited file lines by streaming instead of reading whole files.
- Tool failures derive typed effect/retry recovery state from controller-owned execution facts and render the retry requirement in model-visible diagnostics, preserving committed work and permission boundaries.
- `/image settings` for MLX-Serve models has a **Layer weights** row (`image.models.<key>.cond_weights`): per-tapped-text-encoder-layer conditioning weights, comma or space separated, with any count (3 for FLUX.2, 12 for Krea) sent as `cond_weights`; the server validates the count for the model.
- In the `/watch` side pane's **Peers** tab, `Enter` opens the chat history between this session and the selected peer: messages sent and received since the session started, with mail still queued for the agent marked unread. `m` replies from the chat (`docs/terminal-interface.md`).
- Opt-in **Allow layer weight edits** under Image generation settings (`image.allow_weight_edits`) and the approved `image_weights` tool: inspect checkpoint tensor names and scale exact floating-point tensors into a separate safetensors file, preserving source, metadata and unselected bytes. Source-hash validation, finite-value checks, ordinary path authorization and no-overwrite publication apply; no implicit quantized-bit edits or MLX-Serve reload (`docs/image-generation.md`).
- Stream rules can defer instead of interrupt: `interruptMode` (`always`, `prose-only`, `tool-only`, `never`) chooses which matches stop the inference; other matches keep the reply and remind the next request. Judged `question` rules ask the judge role about each completed reply (`portable_rules.judge`, default `auto` = only with an assigned judge role) and never interrupt. `portable_rules.repeat_after_turns` lets a fired rule fire again. Every firing shows a notice (`docs/stream-rules.md`).
- `dial rules list` shows loaded rules and refused rule files; `dial rules test` reports which stream rules a sample text, reasoning or tool call fires, without a model (`docs/stream-rules.md`).
- `examples/benchmarks/cross_agent.py` runs benchmark suites through several agent CLIs (Dial and oh-my-pi `omp` in `cross-agent.toml`) with the same fixtures, prompts, model, thinking level and independent checks. It reports pass rates, wall time, tokens and tool calls side by side, excludes Dial-only checks from every score, keeps each turn's raw output beside the report, and accepts per-agent environment overrides such as an isolated `DIAL_HOME` (`docs/benchmarks.md`).

### Removed

- Runtime plugins and Python extensions are gone: the `dial plugin` CLI, `/plugins`, the Plugins tab and plugin entries in `/extensions` marketplaces, `--ext-flag`, plugin tools, commands, panels, UI slots and observer events, the bundled `dial_extensions` SDK and its examples. Saved `[extensions]` settings are dropped on load. Installed plugin directories under `~/.dial/plugins` are no longer read and can be deleted. Lifecycle hooks, MCP servers, `[tools.*]` custom tools, skills and the Rust embedding SDK (`extensions::ExtensionHost`) are unchanged (`docs/extensibility.md`).

### Changed

- `/omfg` is renamed `/rule`. It still drafts a stream rule from a complaint and saves it after review (`docs/stream-rules.md`).
- With `verification.automatic_checks` off, the completion gate now runs the applicable focused checks itself when a final answer arrives with unverified edits, using the `check` tool's approval and freshness path. A pass accepts the answer without a reminder round; a failure keeps the single reminder and includes the observed output. Readonly approval, deny rules, deadlines and cost caps still prevent the run (`docs/agent-loop.md`).
- Efficiency-enabled sessions use shorter built-in engineering/workflow guidance and a bounded, searchable deferred MCP index instead of repeating the full MCP tool inventory on every request. Callable tools, approvals, visual verification and operator-owned prompts remain unchanged (`docs/agent-loop.md`).
- Print mode (`dial -p`) no longer offers the `ask` tool. It has no human input channel, so every question was refused anyway; the schema no longer rides on each request. Interactive, RPC and ACP sessions are unchanged.
- When `write` refuses to overwrite an existing file that was not read, a file of up to 200 lines and 16 KiB is now shown in full in the refusal and counts as read. The next `edit` against that tag can replace it without a separate `read` round. Nothing is written by the refused call, and larger files keep the plain refusal.
- MCP server instructions are included only when the request offers a tool from that server. Deferred or disallowed servers no longer add unrelated setup suggestions and instruction text to every prompt; tool discovery and loading restore the relevant instructions without changing permissions (`docs/tools.md`).
- The default tool offer no longer carries `lsp`, `git`, `mcp_servers`, `session_search`, `peers` or `side_request` schemas on every request. They stay listed as loadable tools, `tool_load` enables them for the rest of the session, and task routing offers `lsp`, `git`, `mcp_servers` and `session_search` directly when the request names their work. `working_state` and `codebase_memory` stay offered. Pinned `[harness] tools` lists, hermetic runs and worker selections are unchanged. The `bg` and `task` descriptions are shorter, and the duplicated MCP routing paragraph is gone from the system prompt. In the default-offer test fixture, the serialized schemas shrink from about 49 KB to 37 KB (`docs/agent-loop.md`).
- Print mode (`dial -p`) starts connecting configured MCP servers before the session and agent are built, the same background pass interactive startup already uses, so a slow server no longer delays the first model request. Tool availability is unchanged: the registry still waits for each server's result before offering its tools.
- The skill rule asks the model to load a matching skill in the same response as its first reads, not as a separate round.
- Releases are gated before tagging, not after: `scripts/release.sh` refuses unless `HEAD` matches `origin/main` with a successful CI run, and runs the new `make preflight` (CI's test-job gate locally, using the system temp directory and the real-terminal smoke scripts). CI also checks both musl release targets on every push, keeps its build cache after a failing run (macOS runs had been rebuilding everything from scratch), and saves caches only from `main`; the release no longer repeats CI's Windows contract job (`docs/release-and-install.md`).

### Fixed

- Request-tail task-context and advisory-note markers (`dial_task_context`, `dial_guidance`) are now stripped like other harness bookkeeping before Chat Completions requests and training capture. Previously they reached the provider and made harvested training samples fail validation as malformed (`skipped_invalid`). An offline integration regression covers the harvest path (`docs/improvement.md`).

- Bounded subprocesses that are cancelled, time out or exit now signal their process group a second time after the leader exits, while its PID is still reserved. On macOS a descendant forked during the first `killpg` could miss it and hold the output pipes open, so a cancelled command was intermittently reported with incomplete output after a 300 ms drain wait. The test-suite flake was reproduced outside Dial (about 7% of kills under load) and was not seen with the second signal.

- On Linux, sandboxed background jobs (`bg start` and `bash` with `pty: true`) are no longer killed as soon as the tool call that started them returns. bubblewrap's `--die-with-parent` uses a parent-death signal that fires when the forking *thread* exits, and tool calls run on short-lived threads, so a job's first `bg input` found it `killed by signal 9`. Children are now forked by one launcher thread that lives as long as Dial, so the signal still fires when Dial itself exits. The `terminal-launch-recovery` smoke case caught this on Linux CI; a Linux unit regression covers a child spawned from an exited thread.

- OpenAI-compatible zero-output usage reports no longer falsely mark local prompt processing as generation and arm the 30-second silence watchdog. Positive output counts still mark generation, and real stalls remain recoverable. A deterministic parser regression covers zero, positive and missing output counts; live-host reproduction is unmeasured (`docs/llm-transport.md`).

- Tool reliability and usage statistics (`docs/plans/tool-reliability-and-stats.md`):
  - Test and unloaded-config runs no longer write rows into the real `~/.dial/stats.sqlite3`. Stats are keyed by the project directory under `projects/` and skipped when no project is loaded. Worker sessions are recorded in a new `worker_sessions` table so reports can separate them, and the session index syncs when a session ends. Subscription-billed requests (ChatGPT, Claude subscription, subscription OAuth) are recorded as such, and `dial stats` and `/models` show `subscription` for them instead of `unpriced` or `-`. Older rows are unchanged (`docs/share-stats-telemetry.md`).
  - `check` runs commands in the foreground, so a check that outlasts `shell.auto_background_secs` reports pass or fail instead of an error. `codebase_memory` at its 128-topic cap names stale topics to forget, and workers are only told to `recall`.
  - `edit` hashline errors name the corrected operation for empty `PUT` bodies or sections, and an empty tag quotes the latest read. `message` to a finished worker returns a non-error "finished" receipt, and unknown recipients list the live roster. `grep` accepts `limit` as `max_results`. Peer scopes accept a leading `./` and a trailing `/`.
  - `lsp` finds `rust-analyzer` through `rustup which` (no auto-install) and resolves pyenv shims to the active version or skips them. Its not-found error lists the directories it searched. `dep_source` may read Cargo registry/git checkouts and the Go module cache under Strict sandboxing; denied paths still win (`docs/tools.md`).
  - `bg` port readiness for unsandboxed jobs probes loopback listeners the job owns without network permission. `web_fetch` refusals for localhost point to `browser` or `curl`, and reading a session transcript path points to `session_search`.
  - `bash` puts a short digest of the first error lines and `file:line` locations before the output of failed cargo, pytest, go test and package-install runs whose first error sits deep in the output. Silent exit 1 from grep/rg, `git grep`, `git diff --quiet`, diff, test and similar commands now states its conventional meaning. The error status is unchanged.
  - `browser` tab errors list the open tabs and say that tabs do not survive a browser restart.
  - A direct call to an eligible tool that was not offered loads it and is validated against its schema in the same round. A valid call runs through the normal approval and sandbox path; an invalid one is refused with the schema (`docs/tools.md`).
  - Not changed: the macOS `__eh_frame section too large` linker warning on debug builds. `-Wl,-no_compact_unwind` silences it but made a test program abort on panic, and debug-info settings do not shrink the 21.6 MB unwind table below the 16 MB limit, so no setting was adopted. Compaction was measured over 712 compactions and left unchanged.
  - Effects on live-model error rates have not been re-measured yet.
- Local tool-call recovery is broader. DeepSeek (`<｜tool▁call▁begin｜>` with a JSON fence) and Kimi K2 (`<|tool_call_begin|>functions.NAME:IDX<|tool_call_argument_begin|>`) calls are read from plain text, with their section markers removed, and are hidden from streamed output. A call that closes a reply's reasoning when there is no answer text is lifted on local endpoints only. Text-call detection (`looks_like_text_call`) now accepts any spacing, code fences and every dialect the parser knows, sharing the parser's opener list, so such replies reach the constrained retry or the nudge instead of ending the turn. Tool names with `default_api.`/`tools.` prefixes, a `:N` index, or a case/`-` difference from exactly one offered tool resolve to it. A llama.cpp server reporting a chat template without native tool support starts at the minimal tool set instead of waiting for failures (not saved to `local-profile.json`) (`crates/dial-llm/src/llm/text_tool_calls.rs`, `crates/dial-agent/src/agent/base/helpers.rs`, `crates/dial-agent/src/agent/run/tool_compat.rs`, `crates/dial-agent/src/agent/model/local_profile.rs`, `docs/tools.md`, `docs/agent-loop.md`, `docs/benchmarks.md`).
- vLLM and SGLang servers on loopback or private hosts are recognised from their `/v1/models` list (`owned_by`): their `max_model_len` is the planned context window, they receive the thinking switch and repetition/presence penalties, and they use the `json_schema` constrained retry. Pythonic tool calls (`[read(path="a")]`) are lifted on local endpoints when the whole reply is a list of calls to offered tools. Replies whose tool calls Dial had to read out of text are counted per model, and two of them in a session produce one notice naming the server's tool-parser option. Ollama and LM Studio models that report no native tool support start at the minimal tool set, like the llama.cpp case. All of this is from the servers' documented formats and mocked responses; it has not been run against live vLLM, SGLang, Ollama, LM Studio or llama.cpp servers (`crates/dial-llm/src/llm/catalog.rs`, `local_hints.rs`, `local_server.rs`, `text_tool_calls/pythonic.rs`, `crates/dial-agent/src/agent/model/local_profile.rs`, `docs/agent-loop.md`, `docs/tools.md`, `docs/benchmarks.md`).
- GLM-4.5 and later tool calls are now read from plain text when the local server does not parse them. `<tool_call>NAME<arg_key>K</arg_key><arg_value>V</arg_value></tool_call>` used to become a malformed call with an empty name, so the model was told its call was unreadable; it now becomes a structured call (values are raw text, objects and arrays parsed, as for Qwen3-Coder). A block cut off inside a pair stays malformed and is never executed (`crates/dial-llm/src/llm/text_tool_calls.rs`, `docs/tools.md`).
- The test suite no longer hangs. A test whose model request failed unexpectedly slept through up to ten 5-minute automatic retry waits (`agent::tests::local_prefill_silence_is_not_cancelled_by_the_generation_watchdog` stalled for over 30 minutes). Test builds now default to a 20 ms interval; tests of the interval set it explicitly (`crates/dial-llm/src/llm/provider.rs`). The failure behind it was real: with an unlimited request timeout, draining the end of a finished reply could wait forever, because the reply's terminal deadline was applied only to reads that ureq labels as response phases. The deadline now bounds every read, and a later grace window can only shorten it (`crates/dial-llm/src/llm/cancel.rs`). Two timing-sensitive tests no longer fail under a loaded parallel run: the stalled-WebSocket fixture waited 5 s for the client's close, and the RPC lifecycle tests allowed 5 s to start a `dial` process.
- A tool call that sends a list of strings as the list's JSON text without its brackets (Claude sent `find` `"grep_keywords": "calc\", \"add"` for `["calc", "add"]`) is decoded into the list instead of failing validation. This happened in 2 of 4 `find` errors in a cross-agent benchmark run. Only text that decodes into two or more strings is converted; any other string for a list stays a type error and is never split or guessed (`crates/dial-agent/src/agent/run/tool_compat.rs`).
- A provider that answers before it has read the whole request, then closes the connection (a rate limit, an oversized or rejected body), no longer surfaces as `io: Broken pipe (os error 32)` after about 0.1 s. Dial now stops sending, reads the early answer, and reports the real status and `retry-after` hint, so rate limits wait as long as the server asks instead of retrying blind. On macOS, the bytes the server sent before resetting the connection are read even though the reset socket refuses timeout options (EINVAL). A close with no answer remains a retryable connection error (`crates/dial-llm/src/llm/cancel.rs`, `crates/dial-llm/src/llm/cancel/tcp.rs`).
- A connection-level failure before any response (connection refused or reset, broken pipe, DNS or connect timeout) is retried once after 2 seconds on a fresh connection instead of after the ordinary 5-minute wait; later attempts keep the configured interval and server `retry-after` hints still win. The first retry notice no longer promises "growing waits" and names `/retry` (`docs/llm-transport.md`).
- Devin reuses its `GetUserJwt` session across requests until shortly before the user JWT expires, instead of authenticating before every turn; an authentication failure discards it. Cursor sends a request's appends and 5-second heartbeats over one reused connection instead of a new connection each (`docs/llm-transport.md`).
- Codex and GitLab Duo Agent WebSockets now honour `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` and `NO_PROXY` through an HTTP CONNECT tunnel, as HTTP requests already did; previously they always connected directly (`docs/llm-transport.md`).
- Prompt-cache warming replays make one attempt bounded to 120 seconds; a failing replay no longer runs the full provider retry schedule with no time limit in the background.
- Compaction preparation now reserves for regenerated evidence notes, exact session state and provider-token calibration before requesting a summary. Portable summaries choose complete retained groups by the available budget rather than a fixed window fraction, and reject an impossible newest group without spending a summary request. Final live-request validation and continuation policy remain authoritative (`docs/agent-loop.md`).
- Anthropic context sizing now also consults bundled model metadata when discovery has not supplied a capacity. Known models such as Sonnet 4.6 no longer fall back to a 32k window and trigger premature compaction. Explicit user caps and custom-endpoint limits are preserved (`docs/provider-catalog.md`).

- Switching providers with `--provider` no longer carries the previous route's URL, protocol, credential environment variable, headers or account pin into the new provider. Explicit `--endpoint` overrides and same-provider custom routes remain supported, including across settings reloads (`docs/configuration.md`).

- A round that ends with an accepted `finish` no longer sends an extra advisor live-review or automatic-escalation request, whose advice had no later request to reach. A completed outcome still gets the completion review (`docs/improvement.md`).

- Image generation no longer rejects valid provider results just because their format differs from the output extension. Dial converts PNG/JPEG/WebP locally when needed, including saving MLX-Serve results as JPEG/WebP; matching formats keep their original bytes. Approval, bounded decoding and no-overwrite guarantees remain unchanged (`docs/image-generation.md`).
- A live worker's `message` to `parent` is no longer held back by the parent's own `task` wait: the wait now stops early, keeping every worker running, and the message reaches the parent's next request. While idle with workers running, the terminal shows such messages within about a quarter second instead of up to two seconds. Messages to a parent still never start a turn on their own (`docs/subagents.md`).
- Hosted OpenAI and Anthropic requests are no longer cancelled with "Silent inference detected" while the model reasons silently before its first or next streamed event. For remote routes, the `[request_recovery] silence_secs` watchdog now waits at least as long as the transport's stall limit (`remote_model_idle_timeout_secs`, default 300 s), which already retries genuine stalls. It does not fire at all when that limit is `0`. Local routes keep the configured limit. Responses output-item lifecycle events now also count as watchdog activity (`docs/tools.md`).
- Hardening: the sandbox network proxy closes a tunnel 30 s after its upstream side ends instead of waiting indefinitely for an idle client, backs off on failed `accept` instead of spinning, caps Linux bridge connections like the proxy listeners, and treats 6to4 (`2002::/16`) addresses by their embedded IPv4 endpoint so private or loopback targets cannot be reached through them. WebSocket proxy tunnels split their connect budget across resolved addresses, so a blackholed first address no longer consumes it, and a refused tunnel reports the proxy's status line. Pythonic tool calls with a repeated dictionary key are refused like repeated keyword arguments. ACP client file requests refuse a non-UTF-8 path instead of panicking. A failed `--output-schema` correction turn in print mode still emits the final result event (`crates/dial-tools/src/tools/exec/sandbox/netproxy/`, `crates/dial-llm/src/llm/ws_proxy.rs`, `crates/dial-llm/src/llm/text_tool_calls/pythonic.rs`, `crates/dial-app/src/rpc/client_fs.rs`, `crates/dial-app/src/app/mod.rs`).

## [0.2.9] - 2026-10-29

### Added

- Opt-in idle session preparation: local one-token prefix warming, explicitly approved language-server source preparation and offline locked Cargo checks. All activity defaults off; no hosted fallback, model download or final-verification credit (`docs/session-preparation.md`).
- Explicit native paid OpenAI/Anthropic provider batches with private restart-safe submit/status/results/cancel records and review-only archive summary snapshots. No scheduler, subscription-token routing, automatic submission or application of generated text (`docs/provider-batches.md`).
- Explicit finite-lifetime restart-surviving Unix pipe jobs, owned by a private helper instead of the calling session. Ordinary background jobs remain session-owned; reconnect never replays commands, installs services or grants additional permissions (`docs/background-services.md`).
- Opt-in bounded parallel isolated candidate generation/evaluation through `solve.concurrency` (default 1), and byte-validated single-file native read/grep preparation under the existing disabled speculation flag (`docs/async-candidates-and-reads.md`).
- Opt-in finite exact batch approvals, conservative dependency scheduling for disjoint native operations, and controller-owned background focused checks with freshness barriers. No saved permissions or experimental settings are enabled automatically (`docs/async-execution.md`).
- First-class trusted Python extensions: bundled SDK for Python 3.11+ commands, hooks, panels, autocomplete, custom tools and protocol-2 actions; `dial plugin new`/`check` authoring commands; explicit install/update prepares a per-extension virtual environment and declared dependencies. Python must already be installed. Model-callable plugin tools retain approvals, versioned contracts and session ownership; state is explicit and Python plugins are not sandboxed (`docs/python-extensions.md`).
- Expanded Python extension API: controller-mediated tools, owned tasks, bounded active-model requests and selected conversation context; session workers with polled subscriptions and progress; revisioned declarative workspace views; typed user/workspace settings, credentials, shortcuts, public presets, scoped profiles and approval workflows. Added temporary `dial plugin dev` reload previews, compatibility/dependency-lock checks, diagnostics and three dependency-free example packages. Explicit trust and all existing approvals still apply (`docs/python-extensions.md`).
- Explicit cross-model session teaching: retain bounded successful request/target observations from local or hosted model A, switch to local model B and run `/improve session` (or name a target selector). Approved source examples keep their identities, use the existing curation and sealed evaluation gates, and never count as B's measured live performance. No automatic training or activation (`docs/improvement.md`).

- Experimental evidence foundation: immutable artifact-backed citations, parent attribution, optional original-source snapshots and freshness checks, explicit coverage and uncertain clock alignment. The `evidence` tool captures/reopens/compares/refines retained data; media reads and approved monitor matches emit citations. Session export/relocation and GC follow bounded manifest dependencies without granting access or verification credit (`docs/evidence.md`).
- Source investigation tools: document structure and content/layout revision observations, timestamped whisper.cpp file audio with retained clips, reproducible CSV/JSON/XLSX calculations and linked charts, attributed comparison findings and hypothesis/experiment records, and the `/evidence` terminal workbench. Bounded video frame investigations require `experimental.video_analysis = true` (off by default because image-token cost varies). All outputs retain existing evidence trust and authorization boundaries (`docs/evidence-investigation.md`).
- Extended source investigations: region-cited image/diagram annotations and coordinate OCR, explicit scanned-PDF pages, clock-aware event timelines, row-linked distribution/trend/correlation charts, expectation-versus-observation checks, persistent research notebooks, channel-preserving audio signal measurements, and human-approved live inspection budgets. Video remains opt-in; measurements, interpretations and verification remain distinct (`docs/evidence-investigation.md`).
- Agent guidance now asks models to inspect schema, units and coverage before conclusions and to make targeted rechecks for ambiguous audio, OCR and conflicting revisions; guidance is not proof of compliance or accuracy. Document inspection exposes workbook sheets and unevaluated formulas; citation-scoped `evidence_search` combines local lexical retrieval with optional configured-model reranking while preserving source locations and limitations (`docs/evidence-investigation.md`).
- Structured data now supports attributed dictionaries and identifier checks, explicit whole-source selection, nested JSON Pointer/JSONL rows, bounded relational joins, dimension-checked unit conversions, strict date/offset parsing and exact decimal-string sums. Added reproducible adversarial interpretation fixtures and offline grader tests; these are not a measured live-model accuracy claim.
- Experimental system investigations: bounded local runtime trace/CPU profile analysis, redacted HAR/API conversation observations with OpenAPI response-status comparisons, and read-only SQLite schema, query and query-plan inspection. Sources and results retain evidence identities; no live capture, remote database access, application execution or causality/verification credit (`docs/system-investigation.md`).
- Approved application monitoring: expiring one-shot watches on Dial-owned background output or existing workspace log files can wake an idle terminal agent to diagnose and repair within the current task and permissions. Every start/rearm requires explicit approval; no external process takeover or automatic restart (`docs/application-monitoring.md`).
- Foreground application monitoring on Unix: `dial run --name web -- pnpm start` preserves interactive PTY input, resize and Ctrl-C while capturing bounded private output for explicitly approved, future-only `monitor` watches in another session of the same project. Watches never own or stop the app; restart, capture loss and wrapper death disarm observation. Windows currently refuses the wrapper (`docs/application-monitoring.md`).
- Images picker discovery for direct OpenAI, xAI and MLX-Serve image models, plus native Gemini, explicit Vertex/Imagen, Black Forest Labs, ComfyUI workflows, fal queues and Replicate predictions. Per-model provider inputs and workflows are editable in image settings; generation stays approval-gated with bounded polling, no submission retries and credential-free output downloads (`docs/image-generation.md`).
- Automatic OpenAI image routing now defaults to `gpt-image-1.5` instead of `gpt-image-1`; explicitly selected models remain unchanged. Catalog discovery does not establish account access or a successful generation.
- Collaborative hashline edits coordinate writers before reading current text, preserving explicit anchor checks during races. Atomic batch conflicts return fresh local context across files and mappings for unchanged anchors; preserved read witnesses and range-selective change notices reduce unnecessary rereads without granting unseen-source access (`docs/tools.md`).
- Compact model-only file handles are issued automatically for native file/search result paths and expanded before normal policy checks; user tool views retain actual paths, and session resume preserves handle identities. Successful complete Cargo/Rust test output omits recognized progress while preserving diagnostics/counts and a full-evidence link. Exact redundant read-only results share cache-aware context filtering without deleting history (`docs/tools.md`).
- The interactive terminal checks for newer Dial releases in the background at most once a day, caches the answer and offers `/update` when an executable update plan is available. Strict/network-off sandboxes skip the check, and print/protocol modes show no notice. `dial update` and `/update` can install verified release binaries, run Homebrew/Cargo updates, or fast-forward and rebuild an available source checkout with `make update`; dirty tracked source files are refused and an already-current version skips reinstalling. Cargo uses the Git install route, not crates.io, and `/update VERSION` passes `--version`. Nix and unsigned builds whose original checkout is unavailable retain manual instructions rather than attempting an unverifiable binary update (`docs/release-and-install.md`).
- `/watch` now opens the live side pane automatically in tmux, Zellij, WezTerm and kitty (with socket remote control) as well as iTerm2. When no supported terminal is detected, or the split fails, it prints the exact `dial … watch SESSION_ID` command to run in another terminal. GNU screen is detected and refused with a reason (`docs/terminal-interface.md`).
- The `/watch` side pane is now a tabbed dashboard for session activity and ongoing work: **Feed** (edits, advisories, reviews, turn outcomes, screenshots), **Plan**, **Jobs** (`bg` processes and slash-command jobs such as `/update`, with output), **Workers**, **Threads** (`/btw`) and **Peers**. From the pane you can stop a job, cancel a worker or a streaming side answer (each confirmed with `y`) and message a peer; approvals stay in the main window. While it is attached, slash-command output and the pinned worker block move to the pane, and the conversation keeps one-line status. `--once` and redirected output still print the feed. The feed's turn entries now read `completed`/`failed` instead of internal names (`docs/terminal-interface.md`).
- New `art-to-ui` example skill (`dial skills import ./examples/skills/art-to-ui`): generate art with `/image` or `image_gen`, then build and screenshot-check the page around it (`docs/image-generation.md`).
- The `task` tool accepts an optional `isolation_backend` (`auto`, `clone`, `overlay`, `worktree`, `copy`, `projfs`) per task or batch item. It overrides `[isolation] backend` for that worker only and implies `isolation: "copy"`. A backend that can't run here refuses the whole call before any worker starts, never falling back. Sessions recorded before the property still resume (`docs/subagents.md`).
- New `cost` status-line segment, for example `$0.0412 turn · $1.20 session`. It updates live during a turn, settles to the exact per-turn ledger figure at the end, and stays hidden when the cost is unknown or zero (local and subscription models). It is on in the default, `nerd`, `ascii`, `compact` and `full` presets (`docs/terminal-interface.md`).
- Bare `/peers` now opens a peers panel. It shows this session and every live peer with phase, summary, claims (overlaps marked ⚠) and unread mail. `Enter`/`m` messages the selected peer, `r` refreshes and `c` clears your claims. `/peers list` prints the old text listing, and the other subcommands are unchanged (`docs/session-coordination.md`).
- `/undo N` undoes the last N turns and restores their file changes; bare `/undo` still undoes one. `/rollback 0` is now refused up front (`docs/tools.md`).
- The browser tool installs bun when it needs to and finds none (`DIAL_BUN`, then Dial's own install, then `PATH`), after approval, using the existing `bun` setup recipe. If you refuse, the error names `/setup bun yes` (`docs/execution-and-web.md`).
- The palette's "Manage ongoing work" goal now includes `/watch`.

### Fixed

- Speech input now defaults to press-to-talk with silence-ended recording and automatic normal-turn submission, rather than interpreting a tap as a short held recording and leaving every transcript unsent. Voice settings expose hold-to-talk, review-before-send and spoken replies; speech during active work queues after the turn, and starting capture stops reply playback (`docs/voice.md`).
- Microphone capture now stops before segment recognition begins, so slow transcription cannot delay release/deadline handling. Queued audio remains available within the configured recording cap, and cancellation still discards the transcript (`docs/voice.md`).
- Ordinary automatic provider retries now wait five minutes (bounded by `retry.max_delay_secs`) while preserving server minimum-delay hints, attempt limits and failover. During a turn, `/retry` skips a pending retry wait immediately, including a server-requested delay, without cancelling the turn or queueing input; its idle failed-turn retry behavior is unchanged (`docs/llm-transport.md`, `docs/composer.md`).
- Question dialogs now submit Other/custom input on Enter instead of returning to the choices; multi-question dialogs advance to the next question, while notes still save without submitting (`docs/terminal-interface.md`).
- Model settings now page by the visible list height, and section navigation reaches local setting suggestions before wrapping back to the first section (`docs/terminal-interface.md`).
- Removing a `/queue` picker row now takes a second `Delete`/`x` press on the same row, and the first press names the item; any other key cancels, and `Backspace` only edits the search instead of removing the selected message (`docs/terminal-interface.md`).
- In model downloads, `Backspace` with a result selected now edits the query and returns focus to it rather than doing nothing. Saving a provider API key without a base URL now explains why instead of silently ignoring Enter. Extensions panel errors from the controller show as a failed job, and a later neutral notice clears that failure.
- Reduced built-in lean prompt overhead to preserve response headroom on small context windows. Default manual compaction of an already-safe history within the configured keep-recent budget is now a no-op instead of attempting summaries that cannot reduce it (`docs/agent-loop.md`).
- Fixed dictation discarding queued microphone audio when recording stops, and distinguished missing/short recordings from undetected speech and empty transcription results. The disabled-input notice now names the actual Voice → Dictation setting (`docs/voice.md`).
- Updated CI and release workflows to SHA-pinned Node.js 24–based `actions/checkout` v5 and `actions/setup-python` v6, removing their Node.js 20 deprecation warnings.
- After a stream-journal append fails (for example because the disk is full), the next turn retries the writer instead of retaining the old error forever. Recovery preserves committed frames, discards only the failed append's partial row and refuses removed or replaced journals; it does not replay tools or recover unpublished output (`docs/terminal-interface.md`).
- Voice settings now ask before installing a missing local backend, prepare its runtime and required model, and select it only after successful setup. Cancellation or failure preserves the previous backend (`docs/voice.md`).
- Silent-inference recovery now defaults to 30 seconds instead of 120. Anthropic lifecycle and signed-thinking activity reaches the agent watchdog, so that hidden activity renews the silence timer instead of producing a false stall; keepalives and usage-only updates still cannot hide a stall. Explicit configured silence limits remain unchanged (`docs/tools.md`).

- Automatic language-server provisioning now uses normal npm global, uv tool, Go, rustup and Homebrew destinations rather than redirecting new servers into Dial storage. Approved first use detects existing package-manager bins outside `PATH`; custom and existing local installations remain unchanged (`docs/setup.md`).

- Ingestion paths now expand compact handles before format detection and share authorized bounded source capture, including added workspace roots. Rich document workers consume captured bytes and extraction caches detect same-size rewrites even with restored timestamps. Legacy HEIC/video/PDF-image reads require explicit converter authorization; PDF rendering applies pixel and output bounds during conversion (`docs/tools.md`).

- On macOS, Option+V starts/stops dictation even when the terminal sends `√` instead of an Alt key event. The fallback follows the active voice binding; bracketed paste remains literal (`docs/voice.md`).
- Every Option/Alt application shortcut now has a default that needs no Option key, since macOS terminals type symbols for Option+letter: `Ctrl+Q` queues a follow-up, `Ctrl+L` repaints, and `F2` model selector, `F3` roles, `F4` providers, `F5` show/hide tools, `F6` worker hub, `F7` latest worker, `F8` next favorite, `F9` dictation. Editor redo adds `Ctrl+6`. A key you bind to an editing or list action is no longer shadowed by an application default in the composer (`docs/terminal-interface.md`).
- Windows: `Ctrl+G` falls back to Windows PowerShell when PowerShell 7 (`pwsh`) is not installed. `Alt+V` attaches clipboard images, because Windows Terminal handles `Ctrl+V` itself and drops image-only clipboards. Dictation moves to `F9`. `Ctrl+Q` replaces `Alt+Enter`, which Windows Terminal uses for fullscreen. The HTTP header editor saves with `Ctrl+S` as well as `F10`, which GNOME Terminal takes for its menu (`docs/terminal-interface.md`).

- `/models` → **Images** now lists the direct Gemini API's image models (`gemini-3-pro-image`, `gemini-3.1-flash-image`, `gemini-3.1-flash-lite-image`, `nano-banana-pro-preview`, `gemini-nano-banana-2.1` and the `-preview` ids), not only `gemini-2.5-flash-image`; they generate through the native Gemini transport with your Gemini key. A saved model catalog from an earlier version picks them up when loaded, without a manual rescan (`docs/image-generation.md`).
- Command results on the launch screen are visible: the first command that prints something (for example `/add-dir`) restores the normal conversation instead of leaving its answer in scrollback behind the full-height home. Text that starts with `/` but is not a command, such as a pasted path `/Users/me/notes.md`, is now sent as a message instead of failing with "unknown command"; paths no longer open the command menu or get completed into a command on `Enter` (`docs/terminal-interface.md`).
- Test-writing guidance now favors compact, risk-driven suites: extend existing coverage, group equivalent cases, test distinct behavior and important boundaries, and isolate deterministic test state. Main/lean prompts, implementation/tester roles and the test-runner skill discourage redundant one-off cases, brittle prose/source assertions and test-count quotas without removing useful coverage (`docs/subagents.md`).
- Context filtering consolidates exact successful read-only duplicates under pressure without waiting for a large pruning batch. Short observations and repeated harness notices no longer expand into larger placeholders; full artifacts, failures, mutations and the newest result remain intact (`docs/agent-loop.md`).
- Images rendered by `visual`, `video`, `evidence` and derived-media `read` (SVG, HEIC, PDF pages) now reach vision models in normal parent and worker turns. One shared capture rule replaces the tool-name list that only covered `browser` and `computer`; the tool's description text is kept as the result, and a failed attachment no longer discards it (`docs/tools.md`).
- `monitor` watches on a busy `bg` job no longer disarm with a spurious "output overrun" after more than 32 KiB arrives between idle polls. Output is scanned forward from the cursor; only bytes the bounded ring really discarded count as loss (`docs/application-monitoring.md`).
- Under context pressure, old large results from the bounded read-only investigation tools (`data`, `database`, `document`, `network_inspect`, `runtime_inspect`, `timeline`, `evidence_search`, `audio`, `visual`, `video`) are replaced by artifact pointers like `read` and `grep` results. Tools that also record state keep their results verbatim (`docs/agent-loop.md`).
- The file-handle registry is journaled to the session only when it changes, instead of after every tool result.
- Tool routing no longer loads `investigate`, `research`, `monitor`, `timeline`, `reality`, `perception`, `video` or `workbench` schemas just because the request uses those everyday words; name them in backticks or with "tool". `monitor` routes for real runtime intent ("check the app logs for errors") and no longer for coding tasks that mention a server or watch mode.
- Complete retained background pipe output and completion notifications now reuse shell minimization on success or failure. Recognized Cargo progress and passing Rust test noise are removed without losing diagnostics, failures, totals or raw artifacts; partial/delta and terminal output stay unfiltered (`docs/agent-loop.md`).
- Automatic codebase and persistent-note context abstain on conversational follow-ups and incidental overlap, without narrowing explicit recall. Side-request polls return delivery references instead of duplicating canonical answer messages; reporting guidance discourages repeated completion reports (`docs/agent-loop.md`).
- Automatic context relevance is now enforced for parent/child codebase context even with efficiency disabled, and for semantic/Hindsight automatic recall using the current human subject rather than prior assistant or synthetic worker reports. Changing bundles no longer repeat unchanged revision-sensitive note/source records; explicit recall keeps its broader scope (`docs/memory-and-learning.md`).
- Model-facing coordination and output avoid redundant noise: delivered successful worker/pool reports keep recovery pointers instead of repeating narratives, automatic peer rosters are bounded and overlap-prioritized, homogeneous Cargo-check sequences share conservative filtering, and retained successful hashline edits omit equivalent diff duplication. Handoffs bound recent file inventories; historical read-only diagnostics condense only after an exact retry with explicit success/effects metadata. Raw evidence, failures, warnings, approvals and verification distinctions remain available (`docs/agent-loop.md`, `docs/subagents.md`, `docs/session-coordination.md`).
- Large Git status inventories return exact counts and bounded path samples with full artifact recovery; status accepts the existing path filter. Skills support authored compact entrypoints with bundled references and reject partial instruction bodies rather than silently truncating policy (`docs/agent-loop.md`).

- Automatic image generation no longer silently hides `image_gen` from Opus and other chat models when no image role or automatic service is available. Calls report missing provider setup instead; explicit image-route errors, automatic opt-out, credentials and approval requirements remain unchanged (`docs/image-generation.md`).
- `/models` → **Images**, the image role picker and `Ctrl+A` now offer ChatGPT plan models even though their catalog omits the hosted image tool, so a ChatGPT subscription can generate images while main uses another provider. Generation still uses the approval-gated Responses hosted tool on the official endpoint, account access is verified only by the request, and an explicit `disabled` image-generation declaration still refuses the model (`docs/image-generation.md`).
- `/image edit SRC PROMPT` now writes `<source stem>-<prompt slug>.<ext>` next to the source, for example `kite-add-a-sun.png`, instead of a generic name in the project root (`docs/tools.md`).
- Plain-language questions about local runtime captures, HAR files and SQLite snapshots now offer eligible system-investigation tools without requiring tool names or inspection jargon. Coding/conceptual questions, tool allowlists and schema budgets retain their existing boundaries (`docs/system-investigation.md`).

- `/models` capabilities now match what requests assume. Anthropic's `capabilities` object (image input, thinking) and Gemini's `thinking` flag are read. Ollama's `/api/show` list sets tools, vision and reasoning. Hosted rows with no live capability fields (OpenAI, Anthropic, Gemini) fall back to the same bundled record the runtime uses, instead of showing unknown. A chat model that declares no tool calls is flagged in the detail line, because Dial's tools cannot run on it (`docs/provider-catalog.md`).

## [0.2.8] - 2026-10-28

### Fixed

- Terminal lifecycle checks wait for pasted input to render before submitting and use per-scenario model-discovery gates, avoiding lost Enter presses and premature discovery timeouts on busy runners.

## [0.2.7] - 2026-10-28

### Fixed

- Local-server shutdown tests distinguish exited Linux zombies from running processes, avoiding false failures while still checking that the failed server and its helper stop before retry.

## [0.2.6] - 2026-10-28

### Fixed

- Terminal lifecycle smoke checks wait for Dial's side-answer cancellation acknowledgement before inspecting the saved exchange, avoiding a race between provider disconnection and persistence.

## [0.2.5] - 2026-10-28

### Fixed

- Ordinary and detached Unix subprocesses now share the process-group spawn lock, preventing concurrent core spawns from inheriting unrelated output pipe writers on macOS and delaying cancellation EOF.

## [0.2.4] - 2026-10-28

### Fixed

- Desktop-disabled builds no longer compile unused native-only helpers; shared pointer coordinate mapping preserves gesture options without dead-code warnings.
- Linux atomic no-replace moves invoke the kernel syscall directly, avoiding a missing `renameat2` libc wrapper when linking musl release binaries while preserving no-clobber behavior.
- Fresh peer messages can wake completed replies as well as incomplete turns, so idle conversations can receive follow-ups. Wake-ups remain bounded, task-scoped and disabled for blocked, failed, interrupted or human-input-waiting turns (`docs/session-coordination.md`).
- Chat messages show UTC timestamps in live and resumed terminal transcripts. Turn footers display measured token speed when available (`docs/terminal-interface.md`).
- Release artifact validation checks the ELF identification bytes correctly for Linux x86-64 and arm64, with regression coverage for incorrect formats and architectures.

## [0.2.3] - 2026-10-28

### Added

- Project peers can guide the next step of an existing task and wake an incomplete task in an open terminal. Wake-ups are deduplicated, limited to three per user turn with a 30-second cooldown, and disabled with `coordination.auto_wake = false`; completed, blocked, failed, cancelled and human-input-waiting tasks stay asleep (`docs/session-coordination.md`).
- Inference latency reports now separate response wait, loading, prompt processing, generation, provider backoff and cancellation; local scheduling benchmarks measure batching and background overlap without paid model calls (`docs/configuration.md`).
- `/image generate <prompt>` runs `image_gen` directly with the typed prompt, under the usual approval, writing a new image file named from the prompt in the workspace root (`docs/tools.md`).
- `/image settings` sets per-model image generation settings, much like `/model settings` does for chat models. Every image API takes a default size. MLX-Serve adds steps, seed and LoRAs, chosen from `~/.mlx-serve/models/loras` with scales; OpenAI adds quality, background and moderation. Settings are saved under `[image.models.<key>]` and apply to every `image_gen` call for that model. The panel shows only the rows the model's image API accepts (`docs/configuration.md`).
- `image_gen` can edit an existing workspace image through the optional `image` argument, and `/image edit <file> <prompt>` runs one edit directly. MLX-Serve receives the source on its generations route, with the new **Edit strength** setting from `/image settings`. OpenAI Images (gpt-image models and dall-e-2) gets a multipart `images/edits` upload, and the Responses image tool gets the source as an input image. xAI, inline chat-image output and dall-e-3 refuse an edit before any request is sent. The source file is only read, and the result is written to a new file (`docs/tools.md`).
- Two new model roles, both with curated MLX defaults that one command downloads and assigns (`docs/model-vision.md`):
  - **`grounding`** finds a described UI element in a screenshot. It powers the `computer` tool's `target.locate(description)`, which returns screenshot pixels and never clicks, and the browser's `action=locate`, which returns viewport CSS pixels for `click_at`. Set it up with `/setup grounding`: UI-Venus-1.5-2B by default, or UI-TARS-1.5-7B.
  - **`image_embedding`** embeds text and images in one vector space. It powers the new read-only `image_search` tool (modes `text`, `similar` and `score`) and adds a prompt-similarity line to `image_gen` results. Set it up with `/setup image-embeddings`, which installs Qwen3-VL-Embedding-2B.

  Both roles also run on any OpenAI-compatible endpoint. A finished setup reloads the session, so the role applies without a restart.
- Settings rows that name something Dial can list now offer it instead of a typing field: model rows list discovered and downloaded models, and offer missing suggested local models as a confirmed background download that saves the setting only on success; provider and profile rows list configured names; program and folder paths open a file browser; known-value lists open a checklist; tables and arrays open a per-entry editor. `Ctrl-E` still types any value (`docs/terminal-interface.md`).
- Sessions in one checkout share build and test results. A check command (`cargo test`, `npm run build`, …) that another session already ran on an identical tree (same HEAD and uncommitted content) returns that result once instead of compiling again. If the other session's run is still going, this session joins it and is notified when it finishes. Shared results are labelled and never count as this session's own verification (`docs/session-coordination.md`).

### Changed

- Provider retry backoff is distinguished from the initial response wait. Experimental streaming dispatch recognizes explicit Anthropic, Bedrock and Responses tool-call completion, so normal-mode last/single reads can overlap the rest of the response without guessing from partial JSON (`docs/agent-loop.md`).
- `diagram` fences render plain ASCII line art (`->`, `|`, `v`, `+--+`) with box-drawing lines, arrowheads and rounded corners on a filled panel while keeping the source in ASCII. The advisory rule reaches the lean local prompt too and suggests unboxed diagrams for branching, looping or layered structure; a linear flow stays one sentence. No model-independent token-savings percentage is established (`docs/terminal-interface.md`).

### Fixed

- Release checks now accept the intentionally read-only `code_history` and `dep_source` tools for read-only subagents; coordination code and tests match the pinned Rust formatter.
- Linux installer detection respects the active libc even when musl development loaders are also installed. Cross-platform release checks compile Unix-only fixtures only on supported hosts and keep platform-specific Clippy clean.
- Idle cache warming now tracks accepted text-only/final requests as well as tool-call requests, and honors effective five-minute versus one-hour retention and provider refusal fallback (`docs/llm-transport.md`).
- `task wait`, `side_request wait`, `bg wait` and a foreground `bash` command not yet moved to the background now stop as soon as the user queues a message, so the message is handled promptly instead of after a long wait. Nothing is cancelled: the awaited work keeps running (`docs/agent-loop.md`).
- A shell command that repeats only a `cd`, `export` or `NAME=value` prefix of an earlier command (`cd crates/x && git status` after `cd crates/x && ls`) is no longer reported as an unchanged rerun.
- `message` sent to a project peer id, or as a broadcast with no live workers, now says to use `peers` (action=send) for other Dial sessions.
- An `edit` refused because its anchor lines had not been read shows those lines and says to re-issue it; the identical re-issue is no longer withheld by the failed-operation retry barrier.
- A failed shell command is no longer withheld by the retry barrier once the checkout has changed since it failed (different HEAD or uncommitted content), so a build broken by another session's file can be rerun after that session fixes it.
- Clipped build output keeps compiler errors and their locations ahead of warning locations, so 25 `--> file:line` lines under warnings no longer use the diagnostic budget before the `error[...]` line.
- Slash-command Tab/Right completion no longer wraps extension suggestions or their arguments in shell quotes, which broke slash-command dispatch. Quotes and backslashes supplied by the suggestion are preserved (`docs/extensibility.md`).

- Fixed a stdio MCP race where a fast server reply could arrive before local write completion was recorded, causing successful startup or tool calls to tear down a healthy connection. Cancellation and deadline teardown of genuinely blocked writes remain bounded.
- LM Studio vision models (native type `vlm`) are now marked vision-capable in `/models` and accept image input; text-only `llm` models are marked as such. Models LM Studio reports as trained for tool use are marked tool-capable; the rest stay unknown, since LM Studio still prompts tools to them.
- OpenRouter's Auto Router (`openrouter/auto`, `openrouter/auto-beta`) is no longer treated as an image-generation model. Its catalog row lists the output modalities of its whole routing pool, but image requests to it fail with HTTP 404 ("No endpoints found that support the requested output modalities"); assigning it to the image role now reports that it declares no image-generation capability before any request is sent.

## [0.2.2] - 2026-10-27

### Added

- Streamed `/btw` conversations run independently of the main turn, retain private history and interrupted prefixes, and can become full sibling conversations without changing the main branch (`docs/terminal-interface.md`).
- Focused worker transcripts now keep independent editable drafts and support worker-local `/model` and `/thinking` changes at request boundaries (`docs/subagents.md`).
- Managed worker pools support bounded `create`, `push`, `wait`, `peek` and `close` workflows with compatible backend/conversation reuse (`docs/subagents.md`).
- `--restore` reattaches safely resumable interrupted workers after opening their parent conversation, without resubmitting its main prompt (`docs/subagents.md`).
- Extension commands can open bounded declarative panels and receive typed submission, cancellation or unsupported outcomes; command-only plugins need no event subscription (`docs/extensibility.md`).
- Optional grapheme-safe streaming reveal (`ui.smooth_reveal`) and observed input/refresh/paint stall reports in `/debug performance` (`docs/tui-rendering.md`).
- `diagram` fences (```` ```diagram Optional title ````) render as framed, titled panels with tinted connectors on the code background; the agent is prompted to prefer them over prose for structure and flow, and `codebase_memory` findings keep them (`docs/terminal-interface.md`).
- The transcript tells activity kinds apart. Shell, code, edit and web calls with output are framed cards with facts on the bottom edge. Consecutive reads and searches fold into one `Read (N)` / `Search (N)` tree. Sent `message`/`peers` notes render as quoted `✉ IRC` lines with receipt chips. The workers card totals done, running and failed workers (`docs/tui-rendering.md`).
- `image_gen` is offered automatically when an image endpoint has a key (`image.auto`, default on): an OpenAI or xAI session provider generates with its own image model, otherwise `[image]` applies. The generated file is attached to the result so vision models can check and revise it, and image-creation requests offer the tool without a discovery round (`docs/tools.md`).
- Hardened `image_gen` with redacted availability diagnostics, explicit image-credential precedence, destination preflight before generation, bounded image decoding and portable regression tests; approval and no-overwrite guarantees remain unchanged.
- Image generation prefers the selected model when exact-model metadata declares image output, including account-scoped ChatGPT Responses hosted image generation, compatible inline-image chat completions and explicit Images API profiles. Dedicated image overrides, approval, bounded decoding and non-overwriting publication are preserved (`docs/configuration.md`).
- An `image` model role follows main when unassigned; the model picker adds Images and image-model downloads, with native MLX-Serve discovery and generation for downloaded image models (`docs/model-downloads.md`, `docs/model-routing.md`).
- Image generation is shared across chat models: automatic routing reuses configured image profiles and keyed provider defaults, and missing ChatGPT catalog metadata no longer hides its approval-gated hosted-image attempt. Explicit image overrides, disabled capabilities, account restrictions and no billing retries remain enforced (`docs/configuration.md`).
- The main agent can ask asynchronous, tool-free `side_request` questions over immutable conversation snapshots, with bounded concurrency, cancellation, automatic result delivery and completion tracking (`docs/subagents.md`).
- Long bash timeouts no longer postpone the configured automatic-background threshold, allowing independent work to continue; `shell.auto_background_secs = 0` still disables automatic handoff (`docs/shell.md`).
- Agent shell commands use a stable per-session Cargo target directory under workspace scratch space to avoid cross-session build locks, preserving explicit target-directory overrides (`docs/shell.md`).

- Independent per-provider authentication selection in `/providers`: explicitly use API-key or sign-in/subscription credentials while keeping both saved. Explicit routes never fall back to the other billing method; API-key removal no longer signs out Claude. Existing configurations retain legacy automatic selection until a method is chosen (`docs/provider-catalog.md`).

- `read` handles large images (downscaled, sources up to 64 MiB), SVG rasterization (`:img`), HEIC (macOS), video metadata, preview grids and frames, PDF page renders, `?q=` vision questions, http(s) URLs through the web fetch gate, and 7z/xz/bz2/zst/ISO/CAB/RAR archives with decode and size guards (`docs/tools.md`).
- `eval` runs JavaScript or Python (`language`), accepts `title`, and moves long cells to `bg` jobs; named `bash` sessions can background with their session state kept. Recorded `kernel` calls resume as Python `eval` (`docs/execution-and-web.md`).
- `dial session pack`/`unpack` and `/pack` produce verified portable session archives with side history, worker transcripts and artifacts; a moved-away source forks a sibling. Exports and `/dump` include worker transcripts; `/share` falls back to a gist and trims to fit (`docs/terminal-interface.md`).
- `/seance` and the `seance` subagent consult a recorded session or worker transcript read-only (`docs/subagents.md`).
- `/context` shows the last request's estimated token breakdown by category; `/debug memory` shows backend, recall-cache and last-recall details; the stats dashboard serves live, per-model, per-tool and request-trace views (`docs/terminal-interface.md`, `docs/memory-and-learning.md`).
- Print mode accepts `--image` (including `--image -` for stdin); RPC adds model/thinking control, session stats, command listing, export, host-provided tools and URI schemes, and OAuth sign-in (`docs/rpc-and-acp.md`).
- Plugin protocol 2: approval-gated actions, status/header/footer/title slots, composer autocomplete, declarative entry renderers and typed observed events including images on input and MCP notifications (`docs/extensibility.md`).
- Context discovery reads `AGENTS.local.md`, `GEMINI.md`, OpenCode instructions, Copilot instruction files and directories, legacy `.windsurfrules`, linked-worktree primary checkouts and `context_files.extra`; instruction files may `@import` files within their allowed roots (`docs/configuration.md`).
- Clipboard image paste works on macOS without extra tools (including Finder-copied files), Windows/WSL, Wayland and X11, without blocking the UI (`docs/terminal-interface.md`).
- MCP servers that announce changed tools, prompts or resources are refreshed live between model requests; `/mcp prompt <server> <name> [key=value…]` renders a server prompt into the composer in the background (`docs/extensibility.md`).
- Browser: pointer/keyboard actions, element queries, richer `wait`, screenshot options, PDF, screencast recording, CPU profiles, metrics and Web Vitals, cookies, storage, emulation and devices, clipboard, saved state, request log, glob routes, HAR export, init scripts and a WebMCP bridge. Request logs and HAR redact sensitive query values (`docs/execution-and-web.md`).
- `lsp` `action="request"` sends a raw LSP method with capability checks, refusal of lifecycle/mutating methods and approval (`docs/tools.md`).
- `web_search` accepts `domains` and `exclude_domains`, adds TinyFish, DeepSeek, Kimi, Synthetic, Ollama and keyless `public` providers, and adds explicitly selected grounded providers (OpenAI, Anthropic, Gemini, OpenRouter, Codex) that answer with the sources their native search cited (`docs/tools.md`).
- New tools: `report_tool_issue` (local redacted tool-malfunction report draft), `glab_mr` (GitLab merge requests via `glab`) and `ida` (IDA Pro analysis via idalib when installed) (`docs/tools.md`).
- Prompt-cache warming (`[cache] warm`, default off), spend warnings (`budget.warn_turn_cost_usd`, `budget.warn_session_cost_usd`) and `retry.fallback_revert` (`docs/configuration.md`).
- Providers: GitLab Duo Agent Platform (`gitlab-duo-agent`), SambaNova, Inception (with per-model effort wire values), TypeSafe System One as a `judge` backend, and several stored OAuth accounts per provider with rotation on rate limits, managed in `/providers` or with `dial auth accounts <provider> [list|select|remove]` (`docs/provider-catalog.md`).
- Release archives for Linux arm64, musl (x86_64/arm64) and Windows arm64; the installer verifies signatures with OpenSSL 3, Python `cryptography` or `ssh-keygen` and refuses unverified installs unless explicitly overridden (`docs/release-and-install.md`).

### Changed

- Terminal reads and bounded output run independently of rendering; adaptive repaint pacing, budgeted scrollback retirement and released render caches keep backlogs from forcing full-history work (`docs/tui-rendering.md`).
- `/tree` previews native transcript/tool evidence and supports turn, parent and sibling navigation while preserving an existing draft; moving the conversation does not roll back files (`docs/terminal-interface.md`).
- Questions and approvals quarantine pre-dialog input; model questions can keep the main draft editable without treating its Enter key as an answer (`docs/terminal-interface.md`).
- Messages typed while a turn runs steer it by default without a model call; `[message_routing] route = "classify"` restores the routing judge (`docs/configuration.md`).
- Anthropic-compatible providers that refuse `cache_control` fields are retried once without the refused field and remembered per endpoint and model (`docs/llm-transport.md`).
- The `kernel` tool is merged into `eval`; saved `kernel` integration entries become `eval`, and sessions recorded with `kernel`, or with the earlier `eval`, `browser`, `lsp`, `find` and `web_search` schemas, still resume (`docs/execution-and-web.md`).
- `make lint` and CI deny every Clippy warning, not only the correctness and suspicious groups.

- Consolidated shared write, reporting and role-boundary guidance across full and lean prompts, preserving rendered policy text. Documented the existing capability-, configuration- and evidence-based adjustments; no model-family prompt forks, routing changes or live-model performance claims (`docs/agent-loop.md`).
- Prompt assembly now composes lean workflow steps explicitly and uses named presentation inputs. Separate behavior changes give request-level one-call contracts precedence over batching/todo pairing, align verification with selected delegation and check ownership, deduplicate custom-template notes individually at complete-line boundaries, and show the small-tier worked example only in local one-call mode with the capabilities it demonstrates (`docs/agent-loop.md`). Escalation policy is unchanged; model-quality and performance effects have not been evaluated.
- Two check-before-change aids are now on by default: `experimental.fault_witness` (after a behavioral focused check passes on a turn with edits, it is rerun once in an isolated copy with the turn's edits reverted, to show whether the check exercises the change) and `experimental.repro_first = "advise"` (on a debugging task, the first non-test source edit before any reproduction failed gets one advisory note). The fault witness costs one extra check run per evidence revision, often a cold build; the keyword task classifier can misfire on `repro_first`, which only advises. No live-model benchmark was run before this change. Opt out with `fault_witness = false` or `repro_first = "off"` under `[experimental]` (`docs/agent-loop.md`).
- Prompt data framing is stricter. A project instruction file containing `</context>` can no longer end its `<context>` wrapper early, and context paths and MCP server names are attribute-escaped the same way as `@file` blocks. MCP servers that send blank instructions no longer add an empty block. Single-line previews (`truncate_chars`) now fold `\r` and `\r\n` as well as `\n`.

### Fixed
- Replace the abandoned 7z decoder with maintained `sevenz-rust2`, removing the vulnerable dependency while retaining read-only archive admission and expansion limits.
- Release CI installs the signature-test dependency before running installer contracts. Named shell sessions retain published state after ordinary nonzero background exits, and histories recorded before the optional `bash.session` property remain resumable.
- Parallel tool results and timings now appear as each call finishes, remain correlated by call ID, and retain original-order provider conversation settlement (`docs/agent-loop.md`).
- Resuming a session open elsewhere creates an independent sibling instead of taking over its live writer; restored workers retain their assignment and current-instance controls (`docs/subagents.md`).
- Side history follows session move/archive/delete operations, with collision refusal and source-preserving failure cleanup (`docs/terminal-interface.md`).
- Automatic runtime-update reports stay in quiet chrome rather than the conversation; `/setup updates` retains access to their details (`docs/tui-rendering.md`).
- Parent and worker turns now refuse an unchanged failed tool call after its first error, rather than waiting for repeated failures. Normalized identities survive compaction, todo updates and continuation; corrected arguments, diagnostic calls and retries after a successful novel source mutation remain available. Background warning text in successful results does not trigger the guard (`docs/agent-loop.md`).
- Settings and prompt-history lock contention now returns a bounded, retryable error instead of blocking command dispatch indefinitely. Settings-driven agent rebuilds (including `/drafter`) keep the composer active during MCP reconnection; queued Enter/shortcut input no longer drops into a synchronous startup wait (`docs/configuration.md`).
- Avoid repeated exact ambient provenance/codebase blocks while retaining cacheable history; defer unrelated web schemas with local task-intent routing; and skip auto-thinking judgments until a request profile supports selectable effort, retaining one judgment across fallbacks and repair rounds. Automatic-compaction defaults and opt-in advisor/drafter behavior are unchanged. No release-wide live token-savings percentage is established (`docs/agent-loop.md`).
- Computer AX role queries no longer confuse tabs with tables or tab groups, and trees expose non-press action hints. Native menu selection accepts explicit foreground recovery while retaining exact-key-window safeguards; unavailable-window diagnostics and multi-window keyboard/AX recovery guidance now distinguish macOS limits from closed windows (`docs/tools.md`).
- Browser navigation accommodates large unbundled module graphs with a count- and byte-bounded burst request queue instead of dropping requests above 16 concurrent jobs, a total admission ceiling, longer deadline-bounded navigation settlement and retained blocked-request causes. Completed non-navigation actions keep their short settlement budget; the active-job limit and exact-endpoint network safeguards remain unchanged (`docs/execution-and-web.md`).
- `web_fetch` preserves the download-size cutoff warning even when the displayed text is also truncated.
- `ast_search` and `ast_edit` report errors for explicitly selected files they cannot scan, and distinguish unreadable files from content and size skips in tree coverage (`docs/tools.md`).
- MCP discovery now gives lean prompts bounded capability hints, uses dependency names from complete Cargo and Python project manifest reads, recognizes distinctive inspection capabilities without a server name, and allows one bounded follow-up after discovery. Eligibility, approval and schema-byte limits remain unchanged (`docs/agent-loop.md`).
- Lean and minimal local profiles now automatically offer relevant MCP capabilities within a smaller schema budget; the context-overflow fallback still drops automatic additions (`docs/agent-loop.md`).
- LSP refactoring snapshots no longer fail an entire request on unrelated unreadable or common dependency/build trees; unobserved targets remain refused. Ordered same-position language-server insertions are accepted. Benchmark comparisons now report an inconclusive result and exit 3 when the baseline has no checked successes (`docs/tools.md`, `docs/benchmarks.md`).
- Anthropic model discovery now follows inference's authentication selection only at the canonical endpoint (subscription-first in legacy automatic mode): custom endpoints retain their API keys, disabled providers stay disabled, and unreadable or unrefreshable selected subscription credentials report an authentication failure instead of silently probing with an API key. Backend construction also rejects unreadable selected Claude subscription state rather than switching to API-key billing. OpenAI API keys and ChatGPT subscriptions remain separate routes (`docs/provider-catalog.md`).
- With `--system-prompt-template`, the rendered template now leads the system messages and `APPEND_SYSTEM.md` text comes last; before, the template was sorted behind the project-context and append messages. Harness blocks the template already renders (project context files, the append text, operator guidance, admitted notes, memory, skills, soul) are no longer sent a second time; notes and memory it does not render follow as separate messages, as with built-in guidance, so a memory change no longer alters the template message. Delegated workers still inherit `AGENTS.md` when the template renders it inline.
- MCP server instructions that would overflow the 8,000-byte budget now skip only that server; later servers whose instructions still fit are kept instead of all being dropped.

## [0.2.1] - 2026-10-26

### Added

- Automatically prevent idle system sleep during active agent turns and delegated workers, releasing each request when its work ends. Uses native macOS/Windows power requests and a Linux logind idle inhibitor without changing saved settings or inhibiting screen locking; unavailable protection is reported rather than failing the turn (`docs/agent-loop.md`).
- Graphify is a curated MCP server in `/extensions` and `mcp_servers`: `graphify` runs the local `graphify-mcp` stdio server from the `graphifyy[mcp]` PyPI package through `uvx`, in the project root, so it serves that project's `graphify-out/graph.json` with no account or key (`docs/extensibility.md`). Curated servers can now run in the project root instead of receiving it as an argument, and PyPI installs that differ only in extras (`graphifyy[mcp]` and `graphifyy==<version>`) fold into one catalog row.
- A relative `[mcp.<name>] cwd` now resolves against the project root instead of Dial's process directory, so a user-level entry with `cwd = "."` runs in whichever project Dial opened, including under `-C`.
- `/graphify [tree|html|callflow]` opens the project's Graphify knowledge graph in the browser. It runs Graphify's own tree, interactive-graph or call-flow exporter on `graphify-out/graph.json` (or `$GRAPHIFY_OUT`) through `uvx` as a background job, and names the build command when no graph exists (`docs/tools.md`).

### Fixed

- Read-only subagents (`scout`, `planner`, `reviewer` and read-only custom agents) are told their limits before their first call instead of discovering them through a refused call: the system prompt marks them read-only and, when `bash` is offered, lists the inspection commands that run without approval and says to use `cwd` instead of `cd`. A refused call now repeats that rule (`docs/subagents.md`).
- The ambient codebase brief no longer pulls in test, benchmark and fixture sources, files that match a long task prompt by a single outline word, or files that match only by a near-universal path word (`crates`, the project name, `mod.rs`/`tests.rs` file names). Such sources need to be named or, for tests and benchmarks, asked for (`docs/tools.md`).
- Core-loop reliability: deadlines, cost caps, overflow and compaction failures, guidance-regeneration limits and uncertain-effect refusals now stop a turn with a report instead of failing it; one turn state spans repair and stop-hook rounds; one-shot notes survive discarded requests; a no-reply watchdog and a shared no-progress ledger bound recovery; the last step of a finite budget asks for a report without tools; interrupts keep finished parallel results; timed-out checks count as failed; unverified edits get one reminder then a disclosure when automatic checks are off; ready fallbacks take over after two attempts instead of every retry; a primary that cannot be rebuilt keeps the fallback (`docs/agent-loop.md`).
- Installing a remote MCP server that requires an OAuth sign-in (HTTP 401 with a `WWW-Authenticate: Bearer` challenge, for example Graphify) no longer fails with "health check failed, nothing saved" and a `/mcp login` hint that could not work without a config entry. `/extensions` (install, import and Smithery) and the agent's `mcp_servers install` now save the `[mcp.<name>]` entry and direct the user to `/mcp login <name>`; the agent tool reports `needs_sign_in`. Other failures, including a 401 without an OAuth challenge, a 403 or refused static credentials, still save nothing.
- Cancelling a request now interrupts a large request body on Windows too, without resending or skipping bytes. Request bodies are sent in bounded non-blocking chunks with a readiness wait between them instead of a socket send timeout, because Windows accepts a send of any size while its buffer has room and leaves the socket state indeterminate when a send timeout expires.
- Listing desktop displays on macOS no longer fails with "native desktop worker panicked" when AppKit reports no localized name for a screen (headless or virtual displays); the display keeps its model-number name.
- Closing Dial or reloading MCP servers no longer hangs on macOS when several stdio servers started together. Process-group spawns are serialized, because std on macOS sets close-on-exec on new pipes only after creating them, so a server started concurrently could inherit another server's stderr pipe and keep it from ever closing.
- Homebrew update checks match the installed keg through the same canonical paths as the ownership check, so a Cellar reached through a symlink is no longer reported as "active binary does not match an installed formula keg".
- Cross-platform CI: tool tests create workspaces under the canonical temp directory (macOS `/var` is a symlink), the budget-expiry worker test bounds the stop after the budget is spent rather than Python start-up, and the real-terminal smoke expects the current queued-message row, uses `/help text` now that bare `/help` opens the command palette, sends Esc as soon as the pending-switch note renders, and stamps its fixture home so the daily runtime update check (Homebrew queries) does not run beside the session under test.

## [0.2.0] - 2026-10-25

### Added

- Bounded retrieval work: parallel grep reserves read budgets before dispatch, reuses matched source snapshots, and rebuilds capped-page coverage without rerunning content search. Codebase context reuses its current bounded freshness snapshot instead of hashing selected sources twice; missing observations remain unchecked. Added resource-budget, paging, stale-witness and freshness regressions.
- Completed retrieval and contract follow-up: bounded pre-sort directory admission shared by legacy/index and paged walkers with distinct ignore policies; reproducible offline search and graph-ranking experiments; typed observational policy reasons across foreground, workers, TUI and RPC; schema-backed client contract examples and portable review/pack skills with roundtrip tests. Graph experiments did not justify changing lexical defaults.
- Retrieval timing reports distinguish first-use workspaces from warm runs. Genuine OS-cold-cache latency remains unmeasured pending an operator-controlled cold-cache environment.
- Always-on verification aids: completion evidence lists `caller_obligations` (unvisited files that mention a declaration whose header this turn changed; blocking only under `verification.strict_requirements`), empty `grep`/`glob` results name earlier overlapping empty searches from the same unchanged workspace, and worker handoffs carry an `evidence_audit` that downgrades a `met` acceptance item whose every cited reference is missing.
- Experimental settings (all off by default, Settings → Experimental): `fault_witness` and `edit_bisection_runs` rerun focused checks in isolated copies with turn changes reverted (evidence only); `repro_first` (`advise`/`gate`) asks for a failing reproduction before the first source edit of a debugging task; `outcome_gradient_rounds` notes when checked rounds stop reducing failures; `check_predictions` adds `check` `expect` and reports mismatches; `streaming_dispatch` starts parallel-safe reads while the reply streams. `dial benchmark` gains one-change rows for each.
- Turn-drafter review offers **Accept all draft messages** to accept the current, queued and future drafts without further draft-review prompts until Dial exits. Consent is session-only; tool approvals and other confirmations are unchanged.
- Owned MCP request dispatch and settlement: bounded admission, cancellation-aware transport/authentication, an initialized-delivery barrier and absolute request lifetimes. Integration inventory now separates connection, notification health and catalog freshness; stale tool definitions lose read-only exemptions and fail closed.
- Shared controller facts distinguish background observation from child success, and captured/retained/presented output completeness with byte accounting. Bounded artifact loss and model-facing clipping remain explicit rather than claiming complete output. Existing local SQLite statistics and optional OTLP record sanitized lifecycle/integrity facts with bounded telemetry ingress and loss counters.
- Interactive turn-drafter review: proposed prompts are visible before **Accept and send** or **Cancel**, including steering. `/drafter [on|off|status]` controls the project preference; off sends original messages directly. Drafter guidance now preserves action requests as executable instructions without turning questions into edit requests.
- Complete settings management coverage: 60 additional schema-backed routes expose missing scalar options and structured collections through validated TOML editors. Search descendant configuration paths, manage the selected global/project layer without copying inherited credentials, and explicitly reveal masked sensitive editors. Coverage regressions guard all configurable fields while excluding runtime-only state and retired compatibility input.
- Standardized procedural skills with versioned examples and an authoring/utility guide; corrected unsupported test-runner instructions. Skill bodies retain their full 64 KiB allowance with separately bounded metadata and UTF-8-safe truncation. Package updates validate persisted sources, preserve local repository identities across projects and repair missing or modified stored content.
- Later automatic compaction for planning windows larger than 65536 tokens: apply summaries at the reserve boundary (normally 85% of the window), instead of 80% of usable space (normally 68%). Background preparation still starts early; small-window defaults, explicit overrides and response-headroom checks are unchanged. With the default 262144-token history ceiling, large models now trigger near 223k instead of 178k.

- iTerm2-aware terminal signals and a live side pane: `/watch` opens a targeted iTerm2 split
  (or prints a portable `dial watch SESSION_ID` command), with edit previews, LSP advisories,
  verified screenshots, turn outcomes and review results. Attached viewers keep screenshots out
  of the main conversation; unavailable viewers retain inline previews. Feeds use private,
  session-scoped storage and sanitize terminal text.
- Selective delegation by default: coupled implementation, regression tests and focused diagnosis stay in the main loop, with workers available for independent parallel work, fresh-context review and context isolation. Settings → Tools → Subagents → Prefer subagents (`task.prefer_subagents = true`) restores specialist-first guidance and prefers asynchronous tester-owned verification. Permissions, worker limits and completion gates are unchanged.
- Same-checkout session coordination: `/peers` and the `peers` tool list live Dial sessions, exchange bounded messages and declare advisory relative-path claims without model calls. Private presence metadata is cleaned up using OS liveness; coordination is disabled in hermetic runs and can be disabled with `coordination.enabled = false`. Declared complete verification inputs are revalidated before evidence is reused after concurrent changes.
- Research-progress checkpoints in foreground and worker loops: varied successful lookups trigger synthesis after 24 observation rounds without a committed change or task resolution. Read-only research can continue after a cited finding; tools requested during synthesis never execute, and repeated invalid checkpoints end incomplete rather than loop indefinitely.
- Experimental turn optimizer (`experimental.turn_optimizer`): when enabled, a `turn_drafter` role appears in /roles and rewrites every user message and steering message into a structured prompt with optional Mermaid diagrams before the main model sees it. The main model receives only the draft; the typed text stays in the session for the transcript, resume and memory. A failed, truncated or empty draft fails the turn instead of sending the original.
- Bash output minimizer (`shell.minimize_output`, default on): output from single simple `cargo`, `git`, `npm`/`pnpm`/`yarn install`, `pytest` and `go test` commands drops progress and boilerplate lines, and ANSI/progress redraws are reduced for every command. Error, warning and failure lines are never dropped, the exit code is untouched, and the complete output stays in the command's artifact.
- Git guard (`shell.git_guard = "workers" | "always" | "off"`, default `workers`): `bash` and `bg start` refuse commands that discard uncommitted work (`git reset --hard`, `stash`, `checkout -- <path>`/`checkout .`, working-tree `restore`, `clean -f`, `switch --discard-changes`) in delegated workers that share the parent's checkout.
- Provider-bound secret masking (`[secrets]`, off by default): credential-shaped tokens, secret-named environment values and `secrets.values` are replaced with keyed `$$SECRET_…$$` placeholders in requests to remote providers and restored in replies and tool-call arguments before anything else sees them. Local models are not masked. Credential patterns now live in `dial_core::credentials`, shared with session redaction.
- Provider resilience: `retry.fallback_on` (`any`, `usage-limit`, `except-usage-limit`); `provider/*` wildcard and role keys in `retry.fallback_chains`; delegated workers use their role's fallback chain after their configured routes; a 400 rejecting the reasoning parameter is retried once without it and remembered per endpoint; `providers.<id>.api_key_envs` rotates rate-limited keys.
- Hooks: new `context`, `assistant_message`, `after_provider_response`, `session_before_branch` and `session_before_switch` events (aliases `before_branch`, `before_switch`, `message_end`, `provider_response`). A hook's `additional_context` is delivered once to the next request as a transient note. Request-tail harness notes are normalized, content-free notes are dropped and recent duplicates suppressed; safety and failure notes are never suppressed.
- Context promotion (`[compaction.promotion]`, off by default): before compacting, or after a context-overflow rejection, the session switches to a configured larger-window model when one qualifies. The fallback-chain selector resolver is shared.
- Summarized whole-file reads: a hashline `read` of an oversized source file elides long function bodies as `⋮ lines A-B elided` rows instead of truncating; elided lines are not edit witnesses. `grep` probes large file sets on up to 8 threads with byte-identical pages. `ast_edit` dry runs return an `apply_token` that applies exactly the previewed rewrite, refused if any file changed.
- Hashline stale-anchor recovery refuses a remap that would move an edit into a different enclosing construct or that better matches the authored line numbers.
- RPC/ACP `ready` frames announce `supported_protocol_versions`, `methods`, `capabilities` and `limits`; `docs/rpc-schema.json` is the request/event schema, checked against the code by tests. The `message` tool returns per-recipient receipts (`queued` or `refused` with a reason).
- Session journals carry `schema_version` (legacy journals load unchanged; newer versions are refused). `/label <text>` labels the latest entry, `/tree` navigates this session's branches, and the former fork browser is `/tree forks`.
- Per-file copy-on-write clones use `clonefile` (macOS) and `FICLONE` (Linux) with a byte-copy fallback; `platform::fs::copy_counts()` reports which ran.
- `dial edit-bench`: a seeded, model-free benchmark of the edit grammars (hashline, replace, apply_patch, patch, sloppy, ast_edit) over a committed corpus, reporting exact/refused/wrong outcomes and payload size; exits 1 on any wrong result.
- Contributor and release tooling: `AGENTS.md`, `CONTRIBUTING.md`, `SECURITY.md`, a PR template, `.config/nextest.toml` with `make test-nextest`, `scripts/release.sh`, SHA-pinned CI actions, a docs-only `paths-ignore` for CI, and a `[workspace.lints]` ratchet (`dbg_macro`, `todo`, `unimplemented`, `rc_mutex` denied) inherited by every crate.

### Fixed
- Automatic Python verification executes fresh checks when the installed runtime cannot provide a supported input snapshot; explicit input contracts and stale-snapshot checks remain enforced.
- Isolated scripted integration projects from enclosing repository instructions when `TMPDIR` is inside a checkout. Aligned compaction and unlimited-step fixtures with current defaults and synthesis contracts, and stripped `dial_execution` bookkeeping from canonical model/training messages without relaxing unknown-field validation.
- Core-loop correctness and responsiveness: subset verification retains unrelated failed-check obligations; semantic check classification uses complete retained bytes rather than clipped output. Rejected plain finals receive bounded corrective feedback. Worker coordination cancels obsolete inference during admission/streaming, automatic post-edit hints run outside edit settlement, and live-steering receipt waits service aborts and newer input without losing ordered journal entries.
- Interactive responsiveness and navigation: `/hotkeys` and `/help` open the same panels during turns; provider tests return to the live provider list; saved display preferences apply on closing a busy panel. Compute/model preparation preserves contextual Escape dismissal and supports provider, tool, reasoning, repaint and dictation shortcuts. Screen reset no longer waits for inference, malformed worker commands report usage immediately, and palette mouse actions reject borders and undrawn rows.
- Integration and execution audit: MCP listings hide endpoint credentials and command arguments; temporary checks retain the configured OAuth identity and can test disabled servers without enabling them. Extension-browser remove/toggle actions update the owning settings layer. `/processes` displays scrollable pipe output without consuming tool output, keeps non-PTY/exited views read-only, and `bg wait` surfaces capture failures.
- GGUF startup errors identify the selected server executable and explain how to recover from unsupported llama.cpp architectures. Copilot unsupported-model errors name the selected model and account-policy/plan recovery steps while preserving the original HTTP error and retry classification.
- Installed external runtimes/tools are checked for updates in the background daily, with `/setup updates` for an immediate check and reviewed `/setup update <id> yes` for explicit installation approval; checks never silently upgrade system packages, models or project dependencies.
- Silent-request retries retain request-specific model profiles, local hints and reasoning controls instead of reverting to defaults. Stalled Codex WebSocket requests select HTTP streaming on recovery, including when the agent watchdog fires before the transport timeout; user cancellation remains distinct and completed tools are not replayed.
- The decision role now offers its installed/assigned Laya checkpoint in the model picker (F2 retains custom checkpoint entry). Laya selections no longer enter the chat-weight downloader, which rejected the judge's `decision` service alias as a model path.
- Busy `/models` subcommands now share the idle model panels and defer saved-model adoption safely until turn completion. Live-command scheduling is centralized in `command_registry::during_turn` for dispatch, help and palette descriptions.
- Automatic busy-message routing uses the configured judge (including Laya), bounds classification to three seconds including retries, and falls back to steering on unavailable capacity or failed classification rather than indefinitely delaying the turn.
- `/models` now names downloaded GGUF models `gguf/…` instead of `mlx/…`, and the detail line says which runtime serves each local model ("GGUF, served by llama.cpp" or "MLX, served by Dial's MLX server"). When `llama-server` is missing, the GGUF entry's source says so; Dial still offers to install it before the first turn. The route id in configuration stays `mlx`, so existing selections and sessions are unaffected.
- Added agent-initiated `request_directory` Yes/No approvals, with a separate native-file-only sensitive storage grant. Normal directory access supports file tools and sandboxed commands; sensitive grants exclude credentials and are not restored on resume.
- Recover from dangling action transitions and present-tense promises in foreground and worker replies instead of accepting them as finished work; repeated non-progress still ends incomplete after the existing bounded retries.
- Machine ranking recalculates throughput from verified outcomes and wall time instead of trusting cached report rates; offline comparison and reliability analysis reject malformed transport-usage records with validation errors.
- Bound completed edit-preview recovery even with unlimited productive work; continuation budgets now charge consumed steps rather than refunded request attempts.
- Reset current-turn change and verification evidence before fallible admission, preventing rejected prompts from republishing the preceding turn's provenance.
- Worker workspace-lock waits observe cancellation; partial batch launch failures preserve started-worker results, and worker persistence errors are visible separately from candidate answers.
- Benchmark recommendations require outcome-verified successes, report comparisons enforce recorded fixture-source identity, and zero-token transport failures remain visible in offline reliability analysis.

### Changed
- Terminal UX pass. `dial --help` groups options into task sections (running a prompt, sessions, model, permissions, context, work modes, extensions, protocols), lists everyday commands first and ends with examples and exit codes; subcommand help shows its own options before the grouped global flags. A lone near-miss of a subcommand (`dial sesions`) now exits with a did-you-mean error instead of starting a model turn; `dial -- <word>` and `-p` still send the word. `dial sessions` on a terminal prints a table with a header, relative activity, fitted titles, `~/`-relative projects (`--all`) and the commands that act on an id; piped output is unchanged. `dial doctor` marks ✓ installed and · missing capabilities with the `dial doctor --fix <id>` command (the TUI list keeps `/setup <id>`), a preview ends with the exact `--yes` command, and an unknown id points back to the list. Unmatched or ambiguous `-r`/session ids name the matches and the next command. In the TUI, the steer/queue strip above the composer is indented with a stage glyph, bold label and the quoted message; pinned sessions lead the resume picker as soon as it opens; and work-plan rows show a step's reason or finding before its phase.

## [0.1.22] - 2026-10-23

### Fixed
- Updated Tokio from 1.53.2 to 1.53.3 and stabilized cancellation, artifact-relocation and background-heartbeat regression fixtures without changing their behavioral contracts.

## [0.1.21] - 2026-10-23

### Fixed
- Secret-named environment values containing spaces or newlines are redacted as complete literal values; whitespace-only values remain excluded. Updated ctutils from 0.4.3 to 0.4.4.

## [0.1.20] - 2026-10-23

### Fixed
- Delegation avoids duplicate automatic parent-goal text while preserving explicit assignments and constraints; resumed workers retain developer-role operator instructions. Built-in guidance emphasizes evidence reuse, compact handoffs and shared-check ownership without changing permissions.
- Reliability audit: reject checkpoint restores whose recorded path now resolves elsewhere; bound MCP progress retention; preserve cancellation in automatic-thinking judgments and foreground compute admission; deliver subprocess stdin EOF before waiting for exit on Windows.
- Model transports reject incomplete Devin streams, cap expanded Connect gzip responses, refresh endpoint credentials between retries, and keep Codex WebSocket streaming cancellation and idle limits active inside frame reads.
- Collaboration transport sequence numbers are separate from transcript entry indices (protocol 1003; older peers must upgrade). RPC/ACP reject unsupported agent launch modes rather than silently ignoring them, and process accepted input before EOF cleanup.
- Desktop requests retain their admission-time cancellation generation, and explicit control release waits for acknowledgement. Repeated nonproductive output truncation now stops in both foreground and delegated runs even with an unlimited step budget.
- LSP startup cancellation covers initialization and configuration writes; uncertain command settlement propagates as unknown effects. Owned server descendants are terminated even if the leader has already exited.
- Session relocation imports referenced project-local text artifacts and their metadata before retiring the source journal. Missing required artifacts abort the move without removing the source; root-qualified image references retain their existing store identity.
- CLI tool allowlists recognize configured operator tools and validate dynamic extension/MCP names against the registered session tools rather than rejecting them as unknown built-ins.
- Browser collaboration viewers reconnect after transient host drops while preserving authenticated termination and replay checks. Relay tokens and extension configuration are published through owner-private storage primitives before any secret bytes are written.
- Explicit desktop window IDs use targeted backend lookup instead of a capped enumeration; window snapshot diagnostics report listing limits. Idle Eval cancellation no longer claims that it discarded a namespace.
- Local installation verification errors roll back activated binaries. The Windows test contract selects update tests from the binary target and rejects filters that execute no tests. MCP client regressions follow the separate-test-file architecture rule.
- Computer function runs preserve async execution after final-expression rewriting, fixing `await is not defined` for `computer.run(fn, {args})`. Direct-call and function-run arguments preserve JSON object keys, including nested `__proto__` keys, without changing object prototypes.
- Desktop computer runs now serialize through native cleanup and await retirement before returning, preventing late cleanup from cancelling subsequent calls. Cancellation reports its recorded source and whether native mutation dispatch started. Direct calls render returned values without duplicating Eval output; worker scripts get a helpful `desktop`/`computer` context hint, and window-resolution errors include snapshot diagnostics.
- A connection lost while a gzip-compressed model response was being decoded (`gzip decompression failed: Peer disconnected`) is retried like any other dropped stream instead of failing the turn or the subagent. Corrupt compressed data still fails. Applies to chat streams, Ollama and Bedrock (`stream::disconnect_kind`).
- Subagents running in an isolated copy are no longer offered `codebase_memory`, `code_history` or the memory tools (`recall`, `retain`, `forget`, `memory_update`, `memory_history`, `memory_import`). They are bound to the parent project's folder, so every call from a copy failed. Tools declare this with `Tool::serves_root`.
- Interrupting a `bg wait` no longer ends the turn with "Stopped: `bg` ended with uncertain effects". A wait only watches a job that keeps running, so it reports no effects.
- A fallback-class provider failure (rate limit, quota, auth, transient) with no fallback model to switch to now ends the turn like every other provider failure: it is recorded and carries the same recovery guidance.
- MCP servers that send a request (for example `ping` or sampling) on an open tool-call stream get their answer immediately instead of the call stalling until the stream closes. A failure to store the full MCP result now keeps the result and notes the storage error.
- `glob` patterns with many `**` segments no longer take exponential time on paths that do not match.
- `notebook` edits count as workspace changes: they advance the turn's mutation tracking like `edit`, and are serialized with other document commits to the same file.
- Worktree creation that carries uncommitted changes keeps untracked symlinks as symlinks and writes its temporary patch to an exclusive, unpredictable file.
- Clipboard reads and writes in `computer` give up after 10 seconds instead of hanging on a stalled helper. When a helper is installed but fails (for example `xclip` without a display), that error is reported instead of a later "not installed" one.
- Cancelling or timing out a skill-package `git` command also stops its transport helpers (`git-remote-https`, `ssh`).
- Moving a stream journal never replaces an existing destination file. Moving a session syncs the new directory entry before the old session is deleted.
- Provider errors: per-minute token throttles without a status code back off instead of compacting history, Cloudflare origin errors (520–524) and 408/409/425 are retried as transient, and custom-header redaction keeps typed stream errors retryable.
- The stats dashboard answers only `localhost`/`127.0.0.1` Host headers on its port and stops writes to stalled clients after 5 seconds.
- Text clipping and wrapping never exceed very narrow cell widths. The subagent viewer replays long transcripts in bounded chunks instead of reading the whole file per poll.
- Failures to save input history, the investigation ledger or a tool-denial journal entry are reported instead of silently dropped.
- `dial update --version v` is rejected as an invalid tag instead of failing later with a 404.
- Notebook edits on Unix no longer overwrite a notebook that another writer replaced just before the commit; the original is restored if publishing fails.
- The built-in shell no longer leaves `.tmp/dial-shell-*.sh` scripts behind when a command fails to start (Unix).
- GitHub `pr_diff` caching is bounded to 64 entries. A diff served after a failed refresh is labelled with its age and the failure, a cancelled refresh reports the cancellation instead of an old diff, and argument combinations `pr_diff` rejects are no longer answered from the cache.
- jj file paths containing quotes, backslashes, control or non-ASCII characters are passed as valid jj string literals.
- MCP resource and prompt listings fetched while the server announced a change are no longer cached as current.
- Large `@file` autocomplete no longer piles up stale ranking threads while typing. A crashed git or learning status probe is reported instead of hidden.
- `dial sessions export|rename|share` resolve an id prefix without listing every session. `dial update` bounds the new binary's `--version` output and stops its process group after 20 seconds.
- Build info reports a dirty tree when untracked source files exist.
- A skill import started right after another one finished waits up to 2 seconds for the import lock instead of failing with "another skill import is running".
- A slow or stalled browser sign-in callback can no longer keep sign-in waiting past cancel or timeout (5 second total read deadline).
- A corrupt Anthropic credential file shows an error in `/usage` instead of reading as signed out.
- Devin and Cursor model streams that go silent after their first frame now fail as a retryable stall after the idle limit instead of waiting indefinitely, and a long, steadily streaming reply is no longer cut off by the first-event limit.

### Changed
- Isolated-worker startup uses metadata-only preflight before hashing the worker baseline. Worker text journals store bounded-tail deltas, collaboration/RPC history projections consume incremental messages, semantic recall streams exact top-k ranking with bounded memory, and scheduler watch sets share filesystem traversal.
- Bounded worker-tail journal deltas use schema version 2; current readers retain version-1 compatibility, while older binaries refuse the newer frames instead of silently reconstructing incorrect text. Materialized checkpoints remain version 1.
- A failed turn saves its cause and error class in the turn summary (`turn_summary.error`, `turn_summary.error_class`), so the failure can still be diagnosed from the session after the transcript is gone. The turn report names the class, for example `Outcome: failed (rate limited)`.
- `solve` checks without `timeout_secs` now stop after 300 seconds instead of running unbounded (`0` still means unlimited).
- Background `/tan` jobs are interrupted when their registry is dropped, and the dispatch notice no longer promises that the job reports back to the session.

## [0.1.19] - 2026-10-22

### Added
- End-of-turn **Turn report**: a turn that edited files, was interrupted or failed ends with outcome, changed files, recorded checks (and whether they are current), open tasks, what `/undo` can revert, and shell or extension calls that cannot be undone. Built from the saved `turn_summary` control state; plain answers show nothing.
- Live advisor: with `--advisor` or `/advisor on`, the coach reviews each tool-using round and returns `none|aside|concern|blocker`. An unresolved blocker is re-raised once at completion and noted in the final reply. `/advisor completion` keeps the completion-only behaviour.
- Web search providers: keyed Firecrawl, Parallel, Z.AI and xAI (used by `auto` only when a key is set) and keyless Startpage, Mojeek and Ecosia scrapers. Scrapers report bot walls and unrecognised layouts as errors, never as empty results. `auto` now falls back from DuckDuckGo to Startpage, Mojeek, then Ecosia. Sessions recorded with the earlier `provider` list still resume. Endpoints and selectors are tested against fixtures only, not live services.
- `shell.backend = "inprocess"` runs foreground `bash` commands with the built-in shell inside the Dial process, without spawning a shell host. It is refused, running nothing, unless `sandbox.mode = "off"`. Limits: a builtin-only loop that never yields cannot be interrupted (the thread is abandoned and reported), stopping external commands is by process group on Unix only, and background jobs and named sessions still use the child-process embedded shell.
- `/setup llama-cpp` installs llama.cpp's `llama-server` (Homebrew where available, otherwise a pinned, checksum-verified release archive) into Dial's own `tools/llama-cpp` folder, which is found before `PATH`. Picking a GGUF model with no `llama-server` offers this install instead of failing.
- Dependency-aware verification reuse: within a turn, a passing check can be reused (`ran: false`) instead of rerun when its inputs are provably unchanged and no shell or extension effect occurred since. It is automatic for the built-in isolated Python AST check on recognised Unix CPython installs. Configured commands opt in with `[verification.inputs."name"]` (`paths`, `environment`). Snapshots are bounded (4,096 entries, 64 MiB, 250 ms), and anything unsupported or over a limit runs the check. `check` with `names` and reproductions always run.

### Changed
- `/review` splits the diff into chunks of at most 20,000 bytes, runs them as one reviewer batch (concurrent for remote endpoints), merges and dedupes findings, and prints a P0–P3 report with a verdict (`do not ship`, `inconclusive`, `ship with fixes`, `ship`) followed by machine-readable JSON. The output format changed: it is no longer a bare JSON array.
- Skill guidance now requests `skill` only on substantial multi-step work, so small or single-file edits no longer get a skill nudge (`SKILL_RULE`, `skills::catalog_block`). This changes model guidance, not a guarantee of fewer calls or better task success.
- The model settings overlay labels its runtime field "Local runtime" (it covers llama.cpp as well as MLX), and the Intel Mac/Linux setup message says GGUF models run on llama.cpp without the Python MLX runtime.

## [0.1.18] - 2026-10-22

### Fixed
- Windows: the first session after picking a model no longer exits with `error claiming new session writer ... access is denied (os error 5)`. Session journals open with read access so they can be locked, and the writer lock now covers one sentinel byte past the data instead of the whole file, so live sessions stay readable to listing, previews and header rewrites (`platform::fs::try_lock_journal`).
- Responses `error`/`response.failed` events with an unrecognized code (`unclassified_failure`) are retried within the bounded transient budget instead of ending the turn. Unrecognized `response.incomplete` stays final.

### Added
- Benchmarks: `compaction` suite, `--share-auth` to reuse the subscription sign-in without copying tokens, and resumable progress for interrupted runs.
- Agent status counts and usage rows in the metrics display, with token usage captured from Responses streams.

### Changed
- Automatic project understanding is on by default (`codebase.ambient_context = false` opts out): each user message gets a bounded, task-relevant brief of fresh saved findings and source outlines, with source links, stale findings named but never summarized, and no extra model request. A persisted `ambient_context = false` stays off.
- `find` rows state their attribution (`via=lexical`, `via=exact-text`, `via=model-rerank`) and behavior mode appends a bounded related-evidence bundle (owner, type definitions, call-sites, config references; syntax vs lexical provenance, never edit witnesses). New optional `related` argument (`related=false` restores hit rows only).
- Busy-message controls now offer explicit Steer now, after-turn queueing and Independent worker routing with receipt/application feedback. Steering cancels stale inference without pretending already-running tool effects were rolled back.
- Checkpoint review groups files by task/worker attribution, previews bounded combined diffs, and permits file/group revert with overlap warnings and per-write content checks. Partial rollback preserves detected intervening edits and retains recovery checkpoints; group revert is not a filesystem transaction.
- `/setup presets` reviews Local only, Local with approved hosted assistance and Hosted settings bundles, with project/global scope, explicit existing model choices, helper-role routing and pre-save override validation. No automatic downloads or provider probes.
- Tool and provider failures include consolidated recovery guidance, distinguishing known effects, uncertain effects, retained output and the next safe action.
- CI cancels superseded runs on the same pull request; benchmark command checks allow 120s.

## [0.1.17] - 2026-10-21

### Fixed
- Release validation handles inherited session-lease timing in the steering promotion regression. The adjacent tagged source changes otherwise consist of version metadata; the features previously grouped here shipped in 0.1.14–0.1.16.

## [0.1.16] - 2026-10-21

### Fixed
- Updated TLS certificate verification and bundled roots to `rustls-webpki` 0.103.16 and `webpki-roots` 1.0.10. Native Windows keepalive tests use the getters available on that platform.

## [0.1.15] - 2026-10-21

### Added
- Task-driven tool routing selects bounded native and MCP specialists from the request and dependency manifests already read, without requiring server names. Successful MCP discovery exposes relevant same-server consumers. Small MCP servers are no longer loaded wholesale; automatic additions share a six-tool/24 KiB budget and explicit unloads stay suppressed for the turn. Tool search also indexes parameter help and enum choices.
- Verification causality: unchanged bounded nonbehavioral baseline failures remain disclosed without unrelated automatic repair; explicit runner metadata recognizes missing prerequisites conservatively. Rust compilation targets owning packages, including non-default workspace members, and broadens for shared manifest/lock changes.
- Revision-checked `working_state` patches and addressable unverified claims; compaction carries exact designated critical state outside summaries. Worker handoffs pack priority fields and expose separate acceptance/blocker envelopes without changing custom-schema JSON results.
- Opt-in strict declared-requirement settlement and selective bounded independent coach review, using existing approvals and evidence. Neither reviewer claims nor model-recorded state become verification proof.
- Browser runtime provisioning can be disabled with `browser.allow_install = false` for cache-only execution and isolated integration evaluations.
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
- A llama-server started without `--jinja` that rejects `tools` is now a fatal error naming the fix, instead of being retried as transient.
- `codebase_memory refresh` drops index entries for deleted files even when a scan stops at a limit.
- Dial-launched llama-server passes each option and its value as separate arguments; current llama.cpp rejects the `--host=127.0.0.1` form, so the launch failed.

## [0.1.14] - 2026-10-20

### Added
- Live local-model request settings in the status bar (`inference` in custom layouts), with deduplicated Model settings notices when controls change and full details in `/policy`. Server-controlled values are explicitly unknown rather than guessed.
- Busy messages default to agent-selected routing: changes of approach steer the current task, while clearly independent requests start isolated background workers. Explicit after-turn queueing remains available.
- Local lean assistance now defaults on for local routes regardless of context size; `"auto"` retains the small-context heuristic and `false` opts out. Hosted routes and hermetic runs are unchanged.
- Model settings expose chat-template arguments and MLX server draft-model/KV-cache controls as individual rows, plus Ollama keep-alive settings.

### Changed
- Active-turn settings commands save immediately and defer agent adoption until turn completion; background command reports can arrive while the turn runs.

### Fixed
- Changing models on the welcome screen updates its model label and the status bar together as soon as the new configuration is adopted, without resetting recent-session navigation.
- Local models that ship `do_sample: false` now request greedy decoding (temperature zero) instead of falling back to the harness temperature. Explicit sampling overrides retain precedence.
- Local thinking now defaults to `assistance.local.thinking = "model"`, preserving model/server settings instead of automatically switching thinking by request phase. Phase-based switching remains an explicit opt-in.
- First tool failures receive diagnose-first recovery guidance, including uncertain effects, without granting permission to replay them. Edit guidance reuses current returned evidence and asks only for missing source.
- Hashline edits preflight all unread operation ranges together before staging; only complete current rows shown in the refusal become read witnesses.

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
- The lean-tool fallback integration test derives its context limit from the actual full request instead of assuming a platform-specific prompt size, retaining coverage on Linux and macOS.

## [0.1.11] - 2026-10-19

### Added
- Memory-model selection and richer model browsing: a dedicated Memory group and download shelf assign small instruction models to `memory`; model lists add filtering/sorting and recorded speed, price and usage details.
- Goal-first action palette, explicit busy-message send intent and a read-only `/todo` work-plan inspector. Empty Enter with queued work defaults to keeping the current turn running rather than stopping it.
- Structured completion evidence and opt-in requirement contracts distinguish execution outcome from open obligations, current/stale checks and required rendered pixels. Operator-declared checks and visual evidence do not establish complete request correctness (`docs/requirement-verification.md`).

### Changed
- Automatic improvement guard suites require statistical non-regression evidence; infrastructure exclusions stay distinct from model failures, and unknown measurement costs cannot pass cost/latency admission. Live monitoring records matched-prompt signals instead of rolling back automatically. Default-sized generated guards may be insufficient to establish the default tolerance; these gates do not guarantee improved models (`docs/improvement.md`).
- Terminal panels gain type-to-filter choices, visible selected rows in short layouts, clearer empty/search states and resize-gated confirmation when approval details cannot fit.

### Fixed
- Release container builds receive the tagged source revision explicitly, so excluding Git metadata from the Docker context no longer causes release compilation to fail. Containers also embed the release's trusted update public key.
- The terminal lifecycle regression follows the interactive work-plan inspector, including paging through long blocked-step details and returning to the plan.
- Focused Rust verification derives owning packages and registered targets from manifests, selects target/module checks and discloses coverage gaps; zero-test runs remain unverified (`docs/rust-focused-tests.md`).

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
- Requested synchronization access when opening Windows files and directories for deletion, fixing native deletion failures.

## [0.1.8] - 2026-10-15

### Added
- `/model defaults` and the model-settings shortcut open shared local/cloud context defaults in global scope.
- Multi-turn benchmark journeys cover forced compaction and corrected requirements.

### Changed
- Kept `message` available whenever `task` is offered, avoiding schema changes as workers start and stop.
- Stateless read-only shell calls can join parallel observation groups; named sessions and background calls remain serial barriers.
- Enter dispatches the slash command exactly as typed; Tab/Right accepts completion suggestions. Queued-message editing preserves routing and order, and Enter on an empty draft can stop the current turn and send the oldest queued message.
- Approvals preserve full tool descriptions while transcript summaries remain bounded.

### Fixed
- A smaller provider-accepted summary budget applies to remaining compaction windows after an overflow, avoiding repeated oversized sibling requests.
- Repeated-failure stops include the last concrete tool error, and automatic focused checks disclose changed file types they did not cover.

## [0.1.7] - 2026-10-15

### Fixed
- Limited sandbox toolchain-cache setup to non-Windows platforms, fixing Windows compilation.

## [0.1.6] - 2026-10-15

### Fixed
- Updated the hashline integration regression to verify that an unseen-line refusal supplies read evidence for a direct retry.

## [0.1.5] - 2026-10-15

### Added
- The stats ledger records each request's transport and cache-invalidation fingerprint (`transport`, `invalidation`, `common_prompt_bytes`, `prompt_bytes` in `requests`), so a prompt-cache miss can be attributed to a prompt change or to the provider.
- **Teach this model** (`/settings` → Model → Learning, `improve.teach`): an opt-in switch that lets a local model (MLX, Ollama, LM Studio) learn from your coding sessions. Turns are recorded, and the latest version admitted by the evaluation gate serves the session, LoRA adapter included. Turns are approved without typing scores: committing a turn's exact changes, Alt+Up, or passing behavioral checks counts as good; Alt+Down or `/undo` as bad. Hosted models are never recorded.
- Checkpoints record and restore journaled file changes under `/add-dir` workspace roots as well as the primary project, refusing restores into detached or redirected roots.

### Changed
- Release binaries use thin LTO instead of fat LTO with one codegen unit. Release builds start when the tag is pushed and run alongside CI; publishing still waits for CI on the tagged commit. CI builds and tests use the `ci` profile without debug information.
- On models with very large context windows, the history used for compaction and pruning is now capped at 262,144 tokens by default (`efficiency.history_token_cap`; 0 restores the full window). On a 1M-token window, automatic compaction now starts at about 178k tokens instead of 680k. Windows of 262k tokens or less are unaffected.
- The bash interceptor (`[bash_interceptor]`) is now wired into the agent's `bash` tool and on by default in a new `hint` mode. A command that only duplicates a dedicated tool (`cat`, `sed -n 'A,Bp'`, `grep`/`rg`, `find -name`, `sed -i`, `echo >`, `nohup`, dev servers) still runs, and its result gains one line naming `read`, `grep`, `glob`, `edit`, `write` or `bg`. Batched compound commands are left alone, because one batched call saves model round trips. `gh run view|list` and `gh pr checks` results that still show an unfinished run get a hint to wait once (`github` op `run_watch`, or `gh run watch` with `bg` `wait`) instead of polling. `mode = "block"` keeps the old refusal behavior. Before this change the setting was accepted but never applied.
- Interceptor rules: `cat >file` and `cat <<EOF >file` count as writes, not reads; `tail -f` points to `bg`, not `read`; `find` with `-exec`, `-delete`, time, size or permission predicates is no longer flagged; `perl` counts as an in-place edit only when `-i` is one of its leading switches; and the service, background and watch rules name `bg`.
- Session pickers prioritize pinned sessions and recent activity, hide unpinned empty sessions by default, support word-order-independent search and show prompt/answer previews.
- Private Dial storage remains excluded from native tools and sandboxed commands even when it sits within a workspace such as the home directory; starting a workspace inside private storage is refused. Sandbox `off` remains an explicit isolation opt-out.

### Fixed
- GPT background compaction requests now send an empty tools array instead of null, avoiding HTTP 400 errors from endpoints that require an array.
- Resuming or reopening a session could fail with "already open for writing" right after its writer closed, because a tool process being started at the same moment briefly holds a copy of the session file's lock. Reopening now waits up to two seconds for such a lock before refusing. This also removes intermittent failures in the session and resume tests.
- Fewer avoidable tool failures and bookkeeping rounds. `codebase_memory remember` normalizes cited paths (`./`, `path:12-40`, absolute paths in the root) and re-indexes changed sources inline instead of refusing, and names the path when it still cannot. Sandboxed dev commands get writable uv/pip/npm/Go/XDG caches under dial's cache directory. Generic recovery advice is no longer attached to mechanical errors. A response that sends only `todo` updates gets a once-per-turn nudge to do the work in the same response.
- An edit that touched lines no read had shown failed with "read them first", which cost a read call plus a retry. The error now shows those lines and counts them as read, so the same edit can be re-issued directly. Nothing is written until then, and lines whose snapshot is stale still need a fresh read.
- ChatGPT/Codex sessions lost nearly all prompt caching after an idle gap (a background wait, a long tool, a pause before the next message). The reused websocket had been dropped by the server, its first write failed, and the session was moved to SSE for the rest of the process. SSE requests carried no session affinity headers, so they reached arbitrary servers. Idle sockets are now replaced before use, a dead reused socket is reconnected and the request replayed instead of switching to SSE, and SSE requests send `session_id`/`conversation_id`.
- `/outcome` and `/undo` feedback had nowhere to go in coding sessions, and `/improve` told you to use them anyway; both now record when Teach this model is on and say how to turn it on otherwise.
- `/add-dir` expands `~` and attached directories participate in native file access.

## [0.1.4] - 2026-10-15

### Added
- Native Cursor provider: `/providers` sign-in, model catalog, usage report and streaming chat with tool-call handoff over Cursor's Connect/protobuf agent protocol, with no Node or Bun runtime. Cursor's protocol is private, so this path is verified against local fake servers only.
- `semantic` memory backend: per-project and user-level SQLite banks with keyword (FTS5) recall out of the box, hybrid keyword + embedding recall when the `embedding` model role is assigned (Alt+M lists only embedding-capable models for it), automatic fact extraction after substantive turns with the `memory` role (`memory.semantic.auto_retain`), periodic duplicate consolidation (`consolidate_interval_minutes`), a token budget, and `/memory rebuild|history|update|forget`. Saved settings that selected the earlier sidecar memory backend migrate to `semantic`.
- Skill packages: `/skills install|update|remove|installed|search` and `dial skills …` install skills from Git URLs (`url@ref#path`) or user-configured registries (`skills.registries`), with a lock file recording the resolved commit and integrity, a Skills tab in `/extensions`, and precedence project > user > packages > other tools' skill folders. Install scripts never run.
- Browser relay: `dial browser-relay install|serve|status` and a bundled Dial Chrome extension let the `browser` tool drive your logged-in Chrome tabs over a token-authenticated loopback relay, with no remote-debugging flag (`browser.relay`, `browser.allow_attach`, `DIAL_BROWSER_RELAY`).
- Built-in shell: `shell.backend = auto | system | embedded | powershell`. `auto` uses a system bash (Git for Windows bash on Windows), otherwise a built-in bash-compatible shell with common coreutils. The built-in shell can be compiled out with the `embedded-shell` feature.
- Isolation backends for workers (`isolation.backend`): `auto` picks overlayfs (kernel, then fuse-overlayfs) on Linux, `clonefile` on macOS and ProjFS on Windows, then a git worktree, then a copy, and says in the status line which one ran and why. Overlay hides secret files with pre-created whiteouts; dependency directories stay visible and are never merged back. ProjFS is compile-checked only.
- `message` tool: the parent and live workers exchange short messages (`to`: a worker id or label, `parent`, or `all`), delivered at the recipient's next step with bounded mailboxes. The Workers panel shows recent traffic; replaces the `task` action `message`.
- Word completion in the composer: a dim ghost finishes the word you are typing from your past prompts and this project's identifiers and file names; Tab or Right accepts (`ui.word_completion`). Prompts that look like secrets are never learned.
- `judge` model role (falls back to `small`, then main) for cheap typed decisions, with a 30-day local answer cache. `thinking = "auto"` (`--thinking auto`, `/effort auto`, Settings) judges each user turn's reasoning effort once; judge calls appear in session usage. The judge can be the Laya decision engine (Alt+M → Roles → judge → “laya/decision checkpoint”). It uses the decision role's checkpoint through Laya's typed `choice` API.
- `read` opens `.epub` books in reading order, with `[chapter N: title]` markers and a `:chapter=N` selector.
- Benchmark reports record each failed case's redacted reason (`evidence.agent_error_detail`) and flag runs in which no case reached the model (`invalid_run`); such runs exit non-zero, cannot serve as baselines and are never recommended by machine ranking.
- Directory reads list directories first, then files, by name, and name ignored/generated directories without expanding them; complete `glob`/`grep` walks report a one-line coverage footer.
- `memory_update` and `ast_search` are loadable on demand instead of offered on every request.
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
- Automatic escalation to the `coach` role (formerly `stronger`) after repeated failed rounds, under the escalate tool's approval decision (`escalation.auto_after_failed_rounds`, `max_auto_per_turn`).
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
- The default tool offer is the core roster plus essential integrations when configured (`find`, `lsp`, `bg`, `git`, `web_fetch`, `web_search`, `ast_search`). Other catalog tools are listed with a one-line description and loaded with `tool_load` instead of sending every schema each request. `assistance.compact_tools=true` retains bounded discovery for small local models.
- Tool results are shown whole up to the per-model limit, now 48 KiB (was 24 KiB, with successful inspections cut to 8 KiB and `efficiency.tool_preview_bytes` 4096 on top). `read` and `grep` pages grew to 48 KiB. `efficiency.tool_preview_bytes` defaults to 0 (no extra cap).
- The system prompt uses short MUST/NEVER rules about engineering, tools, workflow, verification and delivery, removing the separate harness-mechanics paragraph.
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
- Role assignments distinguish chat helpers (`main`, `small`, `judge`, `coach`, `tester`, `planner`, `memory`) from the Laya `decision` checkpoint and the `embedding` model. `judge` falls back to `small`, then main; an unset `decision` uses an installed Laya checkpoint, if available, not a chat-model fallback. Embeddings have no chat fallback, and model-calling memory requires its own explicit assignment. Legacy role keys migrate on load; `stronger` becomes `coach`, `--smol` sets `small` and `--slow` sets `coach`.
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
- Providers and transports: unknown stream events/deltas no longer abort replies; in-stream rate-limit and overload errors retry; Anthropic `end_turn` maps to `stop` and tool errors are sent as errors; `max_output_tokens` truncation returns the partial answer; streamed bodies use the 64 MiB stream cap. Codex unsupported upgrades and interrupted WebSocket streams can select SSE recovery without executing partial calls. OAuth-only providers (Copilot, Cursor, GitLab Duo, Gemini CLI, Antigravity, xAI) are listed and selectable after sign-in; ChatGPT sign-in works off macOS and keeps rotated refresh tokens.
- Tools: legacy MCP `sse` transport works and server instructions reach the prompt; installed plugins actually run; `session_start`/`session_end` hooks no longer fire on settings rebuilds; LSP `start_on_write`, diagnostics for unopened files and format-on-write work; LSP file renames are checkpointed; mixed-line-ending files keep untouched lines' endings; web pages in legacy and UTF-16 charsets decode correctly; browser `aria`/`audit`/`trace_*` actions and attach-only mode work; `/bg` jobs keep output printed before adoption; `dial commit --dry-run` leaves the index unchanged and `[git] bypass_hooks` applies to `/git` and `dial commit`.
- Other: `[compaction] supersede_reads`/`drop_useless` take effect; `ui.history_limit` applies live; `dial config set` validates values; scheduled watches fire across `dial schedule run --once`; `dial -p` no longer spawns background workers or blocks on `ask`; the collab web viewer joins rooms; live voice RPC methods work and stopping speech is not an error.
- Strict tool schemas: optional arguments sent as `null` are treated as absent by every tool (finish, ask, task, todo, edit, kernel/eval, gh, artifact_read and others); `working_state` updates are validated.
- Turn lifecycle: Blocked and Incomplete turns list the files they changed; `session_end` reports the real session on quit and fires on session swaps; session-scoped "always allow" grants, trajectories and plan state no longer leak into the next session, and an approved plan survives a settings rebuild; Ctrl-C at an idle prompt no longer cancels the next prompt; Ctrl-Z suspends the TUI; collab guest aborts interrupt a running turn.
- Cancellation: Esc stops `wait` on subagents, `@diff` expansion, `/cleanse` checks, `/loop` conditions (which now run off the UI thread), MCP reconnects and gate retry backoffs.
- Processes and blocking: hung git/gh/sshfs/terminal helpers, plugins, MCP servers and readers that leave grandchildren holding pipes are bounded and reaped; `/collab start`, Smithery search/install/login and the live adapter monitor run as background jobs; killing background jobs no longer stalls the UI.
- Persistence: child transcripts are private (0600); rule, changelog, runtime, ratchet and image-blob writes are atomic; `dial commit --dry-run` restores the index atomically and reports a failed restore; session pins are locked across processes.
- Other: `[tools.<name>] parameters` reach the model and the program; `shell.bash_timeout_secs = 0` means no deadline; a failing focused check skipped by an interrupt stays a repair obligation; harness candidates on an adapter-bearing version pass integrity checks; `git.bypass_hooks` no longer disables signing for user-initiated commits; per-project settings may define hooks and extensions again (they live outside the repository).

## [0.1.3] - 2026-10-07

Historical source-version milestone: no matching local release tag is available. The date and changes below follow source history, not a verified published archive.

### Fixed
- Native calibration respects the opt-out; solve verification fails closed (`38171cb`).

## [0.1.2] - 2026-10-07

Historical source-version milestone: no matching local release tag is available. The date and changes below follow source history, not a verified published archive.

### Added
- Windows installer and release packaging with native runtime checks (`647f3dc`).
- Native operating-system boundaries ported for Windows (`55eceaf`).

### Changed
- Windows ACLs applied through the existing security handles (`b88d6e3`).

### Fixed
- Native Windows paths, file-lock contention, and test contracts (`48a60f4`, `6919f63`, `61ab408`).
- Native descendant PIDs and deterministic CI fixtures (`4471249`).

## [0.1.1] - 2026-10-07

Historical source-version milestone: no matching local release tag is available. The date and changes below follow source history, not a verified published archive.

### Fixed
- Intel Homebrew command lookup (`4afa9af`).
- Reset sockets recognized in the cancellation closure test (`d94df84`).
- Release installer filename corrected in the usage example (`f01f616`).

## [0.1.0] - 2026-10-07

Historical source-version milestone: no matching local release tag is available. This groups initial development through October 7, including October 5 commits; it is not evidence of a published archive.

### Added
- Steering, adaptive scheduling, git and syntax tools, and compute controls (`198734a`).
- LICENSE, CI, and ARCHITECTURE.md (`e2dd00f`).
- Mac and Windows-WSL installation and release builds (`5c0d9c4`).
- Authenticated installation from private releases (`a267515`).

### Changed
- Live steering prioritized and repeated agent work guarded (`ac4fb95`).
- Clippy autofixes applied (`21c514c`).

[Unreleased]: https://github.com/KiidxAtlas/dial/compare/v0.2.6...HEAD
[0.2.6]: https://github.com/KiidxAtlas/dial/tree/v0.2.6
[0.2.5]: https://github.com/KiidxAtlas/dial/tree/v0.2.5
[0.2.4]: https://github.com/KiidxAtlas/dial/tree/v0.2.4
[0.2.3]: https://github.com/KiidxAtlas/dial/tree/v0.2.3
[0.2.2]: https://github.com/KiidxAtlas/dial/tree/v0.2.2
[0.2.1]: https://github.com/KiidxAtlas/dial/tree/v0.2.1
[0.2.0]: https://github.com/KiidxAtlas/dial/tree/v0.2.0
[0.1.22]: https://github.com/KiidxAtlas/dial/tree/v0.1.22
[0.1.21]: https://github.com/KiidxAtlas/dial/tree/v0.1.21
[0.1.20]: https://github.com/KiidxAtlas/dial/tree/v0.1.20
[0.1.19]: https://github.com/KiidxAtlas/dial/tree/v0.1.19
[0.1.18]: https://github.com/KiidxAtlas/dial/tree/v0.1.18
[0.1.17]: https://github.com/KiidxAtlas/dial/tree/v0.1.17
[0.1.16]: https://github.com/KiidxAtlas/dial/tree/v0.1.16
[0.1.15]: https://github.com/KiidxAtlas/dial/tree/v0.1.15
[0.1.14]: https://github.com/KiidxAtlas/dial/tree/v0.1.14
[0.1.13]: https://github.com/KiidxAtlas/dial/tree/v0.1.13
[0.1.12]: https://github.com/KiidxAtlas/dial/tree/v0.1.12
[0.1.11]: https://github.com/KiidxAtlas/dial/tree/v0.1.11
[0.1.10]: https://github.com/KiidxAtlas/dial/tree/v0.1.10
[0.1.9]: https://github.com/KiidxAtlas/dial/tree/v0.1.9
[0.1.8]: https://github.com/KiidxAtlas/dial/tree/v0.1.8
[0.1.7]: https://github.com/KiidxAtlas/dial/tree/v0.1.7
[0.1.6]: https://github.com/KiidxAtlas/dial/tree/v0.1.6
[0.1.5]: https://github.com/KiidxAtlas/dial/tree/v0.1.5
[0.1.4]: https://github.com/KiidxAtlas/dial/tree/v0.1.4
[0.1.3]: https://github.com/KiidxAtlas/dial/commit/38171cb
[0.1.2]: https://github.com/KiidxAtlas/dial/commit/647f3dc
[0.1.1]: https://github.com/KiidxAtlas/dial/commit/4afa9af
[0.1.0]: https://github.com/KiidxAtlas/dial/commit/198734a
