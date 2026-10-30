# Changelog

Notable changes to Dial, grouped by version, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Release attribution uses local release-tag trees from `v0.1.4` through `v0.2.6`; dates use the tag's calendar date. Tags establish source contents, not successful publication or live-provider compatibility. Older untagged notes are explicitly marked below. Unreleased describes the current checkout, not an installed release. Later entries supersede earlier defaults and behavior; optional integrations still require their documented prerequisites and approvals.

**Capability guide — current checkout, not first-release attribution.** The entries below track changes rather than repeat every unchanged feature. These guides cover Dial's main capability areas, including features inherited by the earliest available release tag:

- **Models and inference:** hosted and local providers, API-key/subscription authentication, model discovery/downloads, role routing, thinking, fallback and cache controls ([providers](docs/provider-catalog.md), [downloads](docs/model-downloads.md), [routing](docs/model-routing.md), [transport](docs/llm-transport.md)).
- **Coding and verification:** bounded file/search/AST tools, editing grammars and notebooks, Git/jj/GitHub/GitLab workflows, LSP, debugger adapters, checkpoints/undo, focused checks and requirement evidence ([tools](docs/tools.md), [verification](docs/requirement-verification.md)).
- **Execution and access:** foreground/background shell jobs, persistent JavaScript/Python evaluation, web search/fetch, Chromium and native desktop control, approved SSH and added-directory access; platform and sandbox limits apply ([execution/web](docs/execution-and-web.md), [shell](docs/shell.md), [SSH](docs/ssh.md), [directory access](docs/directory-access.md)).
- **Sessions and terminal UI:** streaming conversations, command palette, settings/keymaps/themes, branches/resume/export/portable archives, side conversations, live work dashboard and voice ([terminal](docs/terminal-interface.md), [composer](docs/composer.md), [voice](docs/voice.md)).
- **Delegation and collaboration:** role-scoped workers, isolated copies/pools, peer coordination and encrypted host/guest collaboration ([subagents](docs/subagents.md), [peers](docs/session-coordination.md), [collaboration](docs/collab.md)).
- **Extensibility and clients:** MCP discovery/OAuth/tools/prompts/resources, lifecycle hooks, skill packages, trusted plugins/Python extensions, print mode, RPC and ACP ([extensions](docs/extensibility.md), [skills](docs/skills.md), [Python](docs/python-extensions.md), [protocols](docs/rpc-and-acp.md)).
- **Memory and improvement:** persistent project/user memory, saved source understanding and change history, explicit local learning, evaluation gates, benchmarks and autoresearch experiments ([memory](docs/memory-and-learning.md), [improvement](docs/improvement.md), [benchmarks](docs/benchmarks.md), [autoresearch/archive](docs/autoresearch-and-archive.md)).
- **Media and investigations:** image generation/editing/search/grounding, documents/audio/data/video, retained citations, research/timelines, offline runtime/HAR/SQLite investigations and approved monitoring ([images](docs/image-generation.md), [vision](docs/model-vision.md), [evidence](docs/evidence-investigation.md), [system investigation](docs/system-investigation.md), [monitoring](docs/application-monitoring.md)).
- **Operations and distribution:** setup/doctor, runtime maintenance, schedules/background services, redacted encrypted sharing, local statistics/optional telemetry, signed release installers and updates ([setup](docs/setup.md), [services](docs/background-services.md), [sharing/statistics](docs/share-stats-telemetry.md), [releases](docs/release-and-install.md)).

## [Unreleased]

### Changed

- Releases are gated before tagging, not after: `scripts/release.sh` refuses unless `HEAD` matches `origin/main` with a successful CI run, and runs the new `make preflight` (CI's test-job gate locally, using the system temp directory and the real-terminal smoke scripts). CI also checks both musl release targets on every push, keeps its build cache after a failing run (macOS runs had been rebuilding everything from scratch), and saves caches only from `main`; the release no longer repeats CI's Windows contract job (`docs/release-and-install.md`).

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
