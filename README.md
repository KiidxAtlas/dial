# dial

A local-first coding agent harness for macOS/Apple Silicon. Run it in a project directory like any agent CLI. Self-improvement is automatic: observations feed verified practice, harness proposals and local LoRA training; only an external evaluation gate can activate a candidate.

Standalone: no dependency on omp, Hermes, or any hosted service. Rust control plane and tools; Python only for MLX inference/training (`mlx-lm`).

```sh
cargo install --path .
cd ~/code/my-project
dial                                  # native terminal UI; first run opens model/provider setup
dial "fix the failing test in src/lib.rs"   # interactive, opening with a message
dial -p -y "summarize this repo"      # one-shot; prints the answer (use -y or --approval auto for tools that change things)
dial -c                               # continue the latest session   (-r <id-prefix> to pick one)
```

For local MLX you can also prepare the runtime and model explicitly:
`dial setup -m mlx-community/Qwen2.5-7B-Instruct-4bit` (requires `uv`).
Discovery itself does not download weights. Selecting an uncached local model and submitting a
prompt resolves its weights; a missing default MLX runtime gets an installation confirmation.

## Terminal interface

- Inline, scrollback-native transcript with a live composer; finished turns remain in normal terminal
  scrollback. Model, provider and settings panels use a full-screen overlay and restore the terminal
  when closed. Approval prompts and settings adapt to narrow terminal widths.
- A configurable status line shows model, role, path, Git state, context, token usage/rate, approval,
  sandbox and learning state. Settings let you choose presets or define left/right segments.
- Searchable model/role, provider and settings panels. Settings save globally or per project outside
  the editable workspace, validate before writing, and apply display changes immediately.
- Dark, light, monochrome, Gruvbox, Nord and Paper themes; custom JSON themes, symbol presets,
  rounded or line composer, styled headings/code/diffs, expandable tool results, and `NO_COLOR`.
- Unicode-aware multiline editing, bracketed paste, persistent per-project prompt history and history
  search. Secret provider inputs are masked and never enter prompt history.
- **Inline suggestions:** as you type, the most recent matching prompt from your history appears as dim
  ghost text after the cursor. `Tab` or `Right` accepts it; `Esc` hides it; `Enter` always sends only what
  you typed. Suggestions appear only at the end of an idle draft, never in secret prompts or mid-line.
- **Slash commands:** typing `/` opens a list of every command (built-in and your `.dial/commands/*.md`)
  that filters as you type. `Up`/`Down` move (wrapping, with a position counter for long lists), `Tab`/`Right`
  complete, `Esc` dismisses without losing the draft. `Enter` on a partial name completes it so you can add
  arguments; `Enter` on a complete name runs it. Built-ins win over a custom command with the same name.
- While a turn runs, `Enter` queues steering for its next model step; `Alt-Enter` queues a follow-up
  for the next turn. The queue is visible above the composer. `Up` brings back the last queued
  message for editing. Interrupting preserves queued messages and the draft.
- `/sessions` and `/resume` open searchable session pickers; `/tree` shows forks. `/rename <title>`
  names a session; `/export [path] [--html]` exports Markdown or self-contained HTML without
  overwriting existing files. `/copy` copies the last answer; `Ctrl-G` opens the draft in an external editor.
- `/plan [on|off]` enforces read-only planning. The model's `exit_plan` request asks for approval
  before enabling edits. `/permissions` lists scoped rules; for example,
  `/permissions allow --persist bash cargo test` persists a project rule outside the workspace.
  Deny rules take precedence, including in automatic approval mode.
- Idle terminal frames are not rewritten. Fenced code gets theme-aware lexical highlighting,
  mutation cards show bounded before/after diffs, and completed Markdown prefixes are cached while streaming.

| Action | Shortcut / command |
|---|---|
| Model selector / roles | `Alt-M` / `Alt-R`, `/model` |
| Provider setup | `Alt-P`, `/providers` |
| Settings | `Ctrl-,` (`Ctrl-4` fallback), `/settings` |
| Previous/next prompt | `Up` / `Down`; preserves the unsent draft |
| Search prompt history | `Ctrl-R` |
| Command list / completion | `/`, then `Up` / `Down`, `Tab` or `Right` to complete, `Esc` to dismiss |
| Accept inline suggestion | `Tab` or `Right` at the end of the draft |
| Multiline input | `Alt-Enter`; `Shift-Enter` when supported by the terminal |
| Expand/collapse tool results | `Ctrl-O` |
| Scroll transcript | `PageUp` / `PageDown` |
| Cancel a panel / stop at next callback | `Esc` / `Ctrl-C` |
| Quit from an empty composer | `Ctrl-D`, `/exit` |

All non-text shortcuts are remappable in `~/.dial/keybindings.toml`; `/hotkeys` shows the active
bindings. For example:

```toml
"app.model.select" = "Ctrl+M"
"app.history.search" = []
```

Settings are also editable in `/settings`. To create a theme, put a JSON file such as
`~/.dial/themes/custom.json` there; it may extend a built-in (`dark`, `light`, `mono`, `gruvbox`,
`nord`, or `paper`) and overrides reload live. `NO_COLOR` forces monochrome output.

`Tab` changes global/project storage scope in settings, roles and providers. The model picker
switches between the main model and helper-role assignments. Supported roles are **main**, **feedback**,
**subagent**, and **compaction**; each routes real requests. “Use the main model” restores role inheritance.

### Providers and discovery

The catalog finds cached Hugging Face MLX snapshots and running LM Studio, Ollama and llama.cpp
servers on their usual loopback ports. It also discovers OpenAI-compatible endpoints and Anthropic's
Messages API. OpenAI, Anthropic, OpenRouter, Groq, Mistral and DeepSeek have setup presets.

`/providers` can add a custom endpoint, set/remove a URL-scoped API key, and choose a model. OpenRouter
also offers **Sign in with OpenRouter** (PKCE), which creates a user-controlled API key without asking
you to paste one. Dial checks that key against OpenRouter's protected `/api/v1/auth/key` endpoint
before saving. The option appears only for OpenRouter's official endpoint, so a generated key is never
forwarded to a custom gateway.

When entering OpenAI or Anthropic API keys, Dial checks them against each provider's model-list
endpoint before saving; this is metadata, not an inference call. OpenAI uses bearer authentication;
Anthropic uses `x-api-key` and `anthropic-version`. Other custom endpoint keys are saved without an
online check. Environment credentials take precedence over saved keys. Saved keys are **owner-private
plaintext** in `credentials.json`, not an OS keychain; they are never written into TOML or rendered
back into the UI. Failed discovery keeps cached models marked unverified. Anthropic uses its Messages
API with native streaming text, thinking and tool arguments; other remote endpoints use OpenAI-
compatible chat completions. **ChatGPT subscription** uses its own OAuth registration and the
Responses API. Claude.ai subscription login is not implemented.

**Use your ChatGPT/Codex subscription.** Restart Dial, open `/providers`, select **ChatGPT
subscription**, and choose **Continue with ChatGPT (add account)**. In the browser, authorize Dial
to use your ChatGPT plan. Return to Dial, open `/models`, and assign an account model to the main
role. Eligible plan access and shared limits are controlled by your account; the model catalog is
not an entitlement check, and your first completed inference request verifies access. Manage Dial's
permission and limits in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

The provider menu also lets you choose a saved account, sign in again, or sign out. Dial keeps each
issued client registration and verified identity separate, retains its host ID across restarts,
and renews rotating tokens under a cross-process lock. Sign-in continues when you close the provider
panel; use **Cancel pending sign-in** in its menu to stop it, or exit Dial. OAuth credentials live in owner-private, atomic `chatgpt-accounts.json` storage under Dial's
user directory (plaintext, not an OS keychain), separately from API keys and editable project files.
Subscription requests always use OpenAI's official Responses endpoint; they never fall back to an
API key. A partial or failed response never releases its tool calls for execution. This integration
follows OpenAI's [Sign in with ChatGPT flow](https://developers.openai.com/siwc/token-sharing-open-source).

Dial combines the account model list with visible models in a recent local Codex model metadata
cache, when present, so newly released models can appear before the account list catches up.
Those additions are labeled **Codex catalog**; their first completed request verifies account access.
Dial reads model metadata only from Codex and uses its own OAuth credentials for every request.
For ChatGPT models, `/reasoning` shows the current and supported levels. Use `/reasoning high`,
`/reasoning max`, or another advertised level to change it; `/thinking high` is an alias. Choices
persist per model in this project's settings and are sent through the Responses reasoning field.

**LM Studio.** Dial reads LM Studio's native model list (`/api/v0/models`, loopback or LAN hosts only) to show
whether each model is `loaded` (with its real context length) or `loads on first use`, and greys out
embedding models, which cannot chat. LM Studio loads a model on demand with a context length of its own
choosing, often far below Dial's 32768 default (an 8192 load was seen in practice); Dial asks the server
what is actually loaded and plans compaction and the context meter against the smaller number. A model
LM Studio refuses to load (memory guardrail, unsupported architecture) is reported with the server's own
message; load it in LM Studio first, with a smaller context, or choose another model. Larger models need
LM Studio's own resource limits to allow them; Dial cannot override those.

Use any OpenAI-compatible server instead of the local MLX model (LM Studio, Ollama, llama.cpp, a hosted API): `dial --endpoint http://localhost:1234/v1 --api-key-env KEY -m <model-name>`. Weight updates need the local MLX model.


### Automation

`dial -p --output-format json "prompt"` emits one result object; `--output-format stream-json`
emits newline-delimited system, text, tool, notice and usage events followed by the result.
JSON modes reserve stdout for JSON. Results include session ID, answer, tool outcomes, usage and duration.
Use `--allow read,edit` for named tool approval instead of blanket `-y`; read-only mode and deny rules
still win. With both a prompt argument and piped stdin, stdin is appended as fenced context.
Exit status: `0` completed, `1` error, `2` interrupted/no answer, `3` required approval denied.

`dial sessions --all` lists sessions across projects. `dial sessions rename <id> <title>` and
`dial sessions export <id> [path] [--html]` expose session management without the TUI.
Ctrl-C interrupts silent HTTP response reads without replacing the configured inference deadline with
a short timeout. Connection establishment and partial writes still use their configured deadlines.

For repeatable repository checks, create a TOML suite and run `dial bench suite.toml --report result.json`.
Each case is copied to a fresh temporary workspace and must declare deterministic checks; for example:

```toml
version = 1
name = "parser regression"

[[cases]]
id = "parses-emoji-path"
prompt = "Fix the parser so a path containing emoji is preserved."
checks = [
  { kind = "file_contains", path = "src/parser.rs", needle = "emoji" },
  { kind = "command", command = "cargo test --offline --lib parser", expect_exit = 0 },
]
```

`dial bench suite.toml --baseline old.json --report current.json` reports pass-rate, token, latency and
tool-call regressions and exits nonzero on regression. Command checks run in the disposable copy with
network disabled; state, `.dial`, build caches and common credential files are excluded from the copy.
The report omits prompts and model output. The Rust `BenchmarkSuite` API accepts custom agent and
verifier callbacks for normalized reports from other harnesses.

After edits, dial derives focused Rust and Python syntax checks from checkpointed changed paths.
`[verification].commands` can add operator-owned sandboxed commands (name, command, optional file
extensions and timeout); a failure is sent to the agent for the configured bounded repair attempts,
then the edited tree and final diagnostics remain visible for `/undo` or manual repair. Approval-denied
writes are not treated as successful verification.

`[dap.<id>]` defines an operator-owned adapter executable. The `dap` integration exposes
`dap_<id>` actions and requires approval on every call; adapter startup is lazy, and launch program
and cwd paths must resolve inside the workspace. `[mcp.<id>].lifecycle_hooks` opts into
`turn_started`, `turn_finished`, `tool_started`, and `tool_finished` notifications through
`notifications/dial/event`; payloads contain identifiers/status only, never prompts, arguments,
outputs, or source content. Hook delivery is observational and cannot bypass tool approvals.

Transient provider errors use bounded backoff with server retry hints and visible notices; authentication
errors and failed response streams are not replayed. `@image.png` and image `read` calls attach supported
PNG/JPEG/GIF/WebP files up to 5 MiB. Sessions store confined paths and content hashes, not base64;
changed or missing attachments are skipped with a notice rather than silently replaced on replay.
Known non-vision models skip images; unknown capabilities are attempted and may be rejected by the provider.
`/cost` shows accumulated cost only when catalog pricing and token usage are available; no local-model
price is invented.

## Model controls, embedding SDK and persistent RPC

Profiles declare capabilities for an **exact** provider/model; no model-name heuristics select reasoning,
sampling, vision or context behavior. Select with `--profile name` or `/profile name`; `/reasoning <level>`
and `/thinking on|off` update the selected profile and reject unsupported combinations.

```toml
provider = "openai"
model = "operator-model"
model_profile = "main"
[endpoint]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
[model_profiles.main]
provider = "openai"
model = "operator-model"            # replace with the exact supported model identifier
max_tokens = 8192
edit_mode = "replace"
reasoning_effort = "low"
[model_profiles.main.capabilities]  # declare only capabilities documented for that exact model
tools = true
reasoning = "openai-effort"
reasoning_efforts = ["low"]
output_limit = "max-completion-tokens"
```

Native Anthropic profiles support declared budget/adaptive thinking and `output_config.effort`.
`prompt_cache=true` requires declared caching support and adds stable system/tool cache breakpoints.
OpenAI caching is automatic. Cached read/write usage is reported separately; unknown counts/prices
remain unknown rather than being fabricated. Output-limit recovery honors its increased request budget.

For browser setup, install outside the editable workspace:
```sh
npm install --prefix "$HOME/.dial/browser" playwright
"$HOME/.dial/browser/node_modules/.bin/playwright" install chromium
```
Set `[browser] playwright_module` to that absolute module path. `allowed_hosts=["127.0.0.1:3000"]`
permits only that local dev endpoint; redirects/subresources are checked again. Metadata/link-local
addresses remain forbidden. Networking is paused between approved browser operations. Python and
Poppler must also be installed when using kernel/PDF capabilities.

`dial rpc` is a persistent version-1 JSON-lines control endpoint. Requests have `id`, `method` and `params`;
responses correlate by `id`, while unsolicited events have `event`. Methods include `send`, `state`,
`messages`, `compact`, `seed_context`, `new_session`, `resume`, `rename`, `plan`, `steer`, `abort`,
`task_start`, `task_status`, `task_message`, `task_cancel`, `approve` and `shutdown`.
Approval responses must name the currently pending `approval_id`; future/stale/replayed decisions fail.
Abort and task messaging/cancellation remain usable while inference or an approval wait is active.
Consumers must handle out-of-order response IDs and asynchronous events.

The Rust SDK exposes `extensions::{Extension, ExtensionHost, Command}`, versioned `tools::ext::Tool`,
atomic tool registration and observational lifecycle events. Operator-trusted extensions can register
tools, prompt commands and render panels through `Ui`; executions still pass the agent's normal
validation/approval boundaries. It is a Rust embedding API, not a loader for arbitrary workspace plugins.

Benchmark suites now support seeded `context_messages`, sequential `turns` with `compact_before`,
categories and bounded metric assertions. `dial bench suite.toml --profiles profiles.toml` selects
configured model-profile names and validated ablations (`no_integrations`, `replace_edit`, `hashline_edit`).
The first supplied profile is the baseline; comparisons rank verified success before latency.
Run `python3 examples/benchmarks/smoke.py --dial ./target/debug/dial` for deterministic capability
regressions, or add `--profiles examples/benchmarks/profiles.toml`. Scripted-provider results are harness
regression evidence, not claims about real-model quality.

## What it does

**Tools** (file edits confined to the project; shell access follows the chosen sandbox)
- The lean default is `read`, `write`, `edit`, `glob`, `grep`, `bash`, `todo`, `tool_search`, `tool_load`, `artifact_read` and `working_state`. Specialists are discoverable by name/capability rather than sending every schema on every request. `tool_load` takes `names`; newly enabled schemas are available in the **next** model round, and enabled selections survive session resume. Explicit profile allowlists remain authoritative.
- **Subagents** (`task`): `scout`, `reviewer`, `planner` and `worker` retain independent contexts, role routing, approval boundaries and structured-output validation. `action="run"` is blocking; `start` returns live IDs, then `status`, `wait`, `message` and `cancel` operate on those jobs. Peers use `task_message` for coordination and artifacts for shared evidence. `/jobs`, `/message <id> <text>` and `/cancel <id>` expose live controls to the user. Inbox/history/concurrency are bounded; children cannot spawn children, and workspace mutations remain serial.
- `read` takes line selectors in either edit mode: `path:50-200`, `path:50+150`, `path:-60` (last 60 lines), `path:5-16,960-973`, or `offset`/`limit`; the footer states the range returned and the file's total line count, and in hashline mode only the lines shown can be edited. `grep` matches a Rust regex per line (`fixed_string` for literal text) with `ignore_case`, `glob`, `context` lines and `max_results`.
- **Edit modes.** The default `replace` mode edits by exact text (`old` → `new`). `--edit-mode hashline` (as in omp) has `read` return `[path#TAG]` and `N:text` rows and `edit` take `PUT`/`CUT`/`MV`/`REM` operations anchored to those line numbers, with registers for moving text. Stale tags are recovered only when the file's changes provably do not touch your anchors; unseen lines cannot be edited; multi-file patches are validated before any write; every result lists the existing lines a replacement removed, because `PUT N.=M` replaces N through M inclusive and an off-by-one silently deletes a neighbour. Hashline suits strong models; a local 35B-A3B model was observed repeatedly deleting adjacent lines with it, which is why `replace` is the default. Block anchors (`N*`) use a bracket/indent heuristic, not a parser.
- **Loop safeguards.** The system prompt is identical on every request of a session (until a tool is loaded or plan mode changes); recalled notes and verified repair patterns are decided once per user turn and stored with that message, so a provider or local server can reuse its prompt cache. A reply that stops at the output-token limit (a reasoning model can spend the whole budget thinking and answer nothing) is never taken as the answer: dial retries with a doubled budget that still fits the context window, then nudges the model, then stops with a clear message. `harness.max_tokens` defaults to 8192. Tool results over 24 KiB keep their beginning and end with an explicit omission marker; `read`, `grep` and `bash` cap themselves at 20 KiB and say how to fetch the rest, so paging footers are never lost. A replace-mode `edit` that misses explains why (line numbers copied from `read`, differing indentation with the file's real text shown, or a drifted block with the line where it starts), edits CRLF files with LF text, and lists the line numbers of ambiguous matches.
- **Model I/O robustness.** Streamed replies are bounded by silence, not by total time. Remote models use `remote_model_idle_timeout_secs` (180 seconds by default, capped by `worker_timeout_secs`); raise it in `/settings` for models that reason longer without stream events. Local servers retain `worker_timeout_secs` after their first event and can take as long as needed before it. Keep-alive comments and read boundaries never reset the event deadline. While waiting, the UI stays editable and shows elapsed time without output; Ctrl-C or Esc cancels the request. Tool-call arguments that are almost JSON are repaired rather than regenerated: raw newlines/tabs inside strings, invalid escapes such as `\d`, single quotes, unquoted keys, trailing commas and a double-encoded object are accepted as written; cut-off or trailing-garbage arguments are refused and never executed.
- **Whitespace-tolerant `edit`.** If `old` is not found byte-for-byte but matches exactly one place once leading/trailing whitespace is ignored, and every line differs by the same indentation prefix, the edit is applied with `new` shifted by that prefix and the result says what was matched. Two candidates, uneven indentation or any text difference is refused with the file's real text shown.
- **Compaction is checked, not assumed.** The trigger (80% of the window) uses the provider's own count for the last request plus a local estimate of everything added since, so a large tool result is counted before the request it would overflow, not after. After summarising, dial estimates the next prompt the same way (the fixed system prompt and tool schemas included). Above 70% of the window it compacts once more, harder; if it still would not fit it says so and names the fix (a larger context window or fewer `integrations`) instead of summarising again every step.
- **Stuck-turn detection.** Besides the identical-call guard (warns at the third identical failure, stops at the fourth), dial counts failures whatever the arguments: the same file tool (`read`, `edit`, `write`, `glob`, `grep`, `ast_edit`, `ast_search`, `artifact_read`) failing five calls in a row gets a warning and ten stops the turn, even with successful reads in between (the read-guess-read-guess loop); `bash` and `kernel` failures are task feedback and are never counted. Four rounds in which every call failed add a warning, eight stop the turn. A stopped turn is **reported**, not raised: the reason is recorded in the conversation, shown as a notice, the outcome is `incomplete`, and print mode exits 2 (with `--output-format json` the result has `outcome: "incomplete"` and no `error`). The step limit ends the same way.
- **Step budget.** `harness.max_steps` counts model calls that did work. A reply that was unusable (cut off by the output limit, an unreadable call, a tool call written as plain text) is retried without spending a step, up to eight times per turn, and compacting after the server rejected the prompt size is never charged. Sealed evaluation runs get no refunds: their budget is what they measure.
- **Completion and todos.** Open todos survive session resume. If the agent stops with pending or in-progress tasks, dial asks it to continue through tools, up to three reminders; another reminder requires a changed todo list or new successful tool evidence, so repeated reads and text-only promises cannot loop forever. The `finish` control tool reports `completed`, `blocked`, or `needs_input`. Completion refuses open todos and running live tasks; blockers and questions require a concrete `reason` and leave open work intact. Ordinary informational replies still work without todos. JSON print results and RPC `send` responses expose `outcome`; blocked, needs-input, incomplete, and interrupted print runs exit with code 2 instead of reporting success. These controls do not independently prove that a model's task-status claims are true; relevant checks still run after completed edits.
- **Specialist integrations** (available per `integrations` in the config, loaded on demand; never in sealed evaluation/self-play). MCP tools follow `[mcp.*]`; enabling a schema never bypasses approval:
  - `bg`: long-running processes (`start`/`list`/`output`/`wait`/`kill`) under the same sandbox as `bash`, in their own process group, at most 8 at once, 256 KiB of output kept each, all killed when dial exits.
  - `web_fetch` / `web_search`: HTTP(S) fetch with HTML→text conversion and an SSRF guard (loopback, private, link-local and metadata addresses are refused on every connection and redirect), and search through DuckDuckGo's HTML endpoint (a scraping path that can break) or your own `[web] search_url` template containing `{query}`. Both need approval and are disabled by `sandbox.mode = "strict"` or `allow_network = false`.
  - `lsp`: definition, references, hover, implementation, type_definition, document_symbols, workspace_symbols, diagnostics and `rename_preview` (never applies) through language servers found on `PATH` (rust-analyzer, typescript-language-server, pyright/pylsp, gopls, clangd) or `[lsp.<language>]`. Servers run unsandboxed, so the call that would start one asks for approval.
  - `ast_search` / `ast_edit`: structural search and rewrite with ast-grep patterns (`$VAR`, `$$$ARGS`) over 28 languages. `ast_edit` previews by default; applying is all-or-nothing, atomic, checkpointed (`/undo` works) and refuses results that no longer parse.
  - MCP: every tool of each `[mcp.<name>]` stdio server appears as `mcp__<server>__<tool>` and requires approval. A server's `readOnlyHint` is not an authorization boundary. Servers are trusted operator programs (unsandboxed); a server that fails to start only produces a notice.
  - `find`: repository retrieval by behavior or exact text. Default behavior mode collects bounded, source-grounded lexical and syntax-declaration candidates (including declarations with no query-word overlap), then asks the configured model to rank candidate indices; paths, spans and snippets come only from captured workspace evidence. `ranking="lexical"` is deterministic and local; `mode="exact"` never calls a model. The semantic request is bounded and non-exhaustive, uses no embeddings or probability scores, and errors honestly if the model or response is unusable. Optional `lsp_path`/`lsp_query` adds live LSP metadata.
  - `browser`: real Chromium tabs, navigation, DOM actions/evaluation, screenshots and console/network evidence. Requires approved interactions and externally installed Playwright/Chromium; exact operator-owned local host/port exceptions enable dev-server verification.
  - `kernel`: persistent Python state, expression results, stdout/stderr and tracebacks. Approval required; reset/deadline/cancel discards the namespace. Required OS fork containment prevents subprocess creation: use `bash` for child processes.
  - Rich `read`: PDF pages (`file.pdf:page=2-4`), notebook cells (`file.ipynb:cell=2`), ZIP/TAR/gzip members (`file.zip:docs/readme.txt:2-20`), and read-only SQLite (`data.sqlite:table?offset=0&limit=20` or `:table:key=2`). Archives never extract to disk; notebook cells never execute. PDF requires Poppler `pdftotext`.
  - Shell/background output is streamed to the UI and fully spooled before clipping. `artifact://<sha256>` references survive compaction; `artifact_read` retrieves line ranges or exact byte pages. Abandoned temporary spools are removed.
- Native tool calling over streaming; Hermes-format `<tool_call>` text is understood when the server does not parse it.

**Safety**
- Approvals: `ask` (default) presents Allow once / Always allow / Deny before mutations, shell execution and operator tools; `auto` / `-y`; `readonly`. A small set of inspection shell commands is exempt, but builds, tests, package scripts, Git and ripgrep still require approval. Always-allow for bash records a command prefix, not blanket shell access. Unsupported compound shell syntax cannot inherit a prefix grant. Without a terminal, `ask` denies unless a named tool or permission rule authorizes it.
- Shell sandbox (macOS `sandbox-exec`, fails closed): `dev` (default) can read most of the machine but never `~/.ssh`, `~/.aws`, cloud/keychain dirs or dial's own state, writes only the project (plus configured `extra_write`), secret-looking environment variables are dropped; `strict` is workspace-only with no network; `off` is an explicit opt-out. `sandbox-exec` is deprecated and this is not a security boundary against a determined local attacker.
- Checkpoints: every file change is journaled per turn. `/undo` drops the last turn and restores its files; `/rollback n` restores files only.
- Configuration, state, memory, sessions and evaluation data live under `~/.dial` (outside the project), so the agent cannot edit its own approvals, sandbox or gate.

**Context (omp / Hermes style)**
- Project files: `AGENTS.md`, `CLAUDE.md`, `.cursorrules`, `.dial/AGENTS.md` up to the git root; `~/.dial/SOUL.md` for identity; `.dial/SYSTEM.md` / `APPEND_SYSTEM.md` overrides. Treated as data, not commands.
- `@file`, `@dir/`, `@diff`, `@staged` references in messages.
- Skills (`.dial/skills/<name>/SKILL.md`, `~/.dial/skills`, agentskills.io format) loaded on demand; the agent can create and refine them (`skill_manage`).
- Provenance memory lives in private `$DIAL_HOME/memory.sqlite3`. `recall`, `retain`, `memory_update`, `forget`, `memory_history` and approved `memory_import` support project/global scope, source/session provenance, revision-checked corrections and expiration. Relevant bounded entries are retrieved on demand with lexical BM25 ranking. Legacy `MEMORY.md`/`USER.md` import is explicit (`confirm=true`) and preserves originals; old configuration tool selectors are migrated, not kept as executable aliases. Forget is logical redaction, not forensic disk erasure.
- Sessions: JSONL per project, resume/fork, rewind, automatic summarization when the context fills (`/compact`), full-text `session_search`.
- Historical evidence stays append-only. The request context is a projection of it, built to keep the conversation prefix byte-identical from step to step so a provider or local server can reuse its prompt cache: recalled notes are stored with the user turn they were recalled for, a superseded read becomes a fixed placeholder once (and only while little text follows it, or when the context is filling up), and under context pressure old, large `read`/`grep`/`glob`/`find`/`bash`/`lsp`/web results outside the newest fifth of the window are replaced once by a pointer to their full text (`artifact_read` retrieves it). Mutation results, todo and memory updates, and small results are never replaced. `working_state` preserves explicit goals, constraints, decisions and unfinished work; compaction also carries the original goal and evidence references. Agent notes are not verification proof.
- Custom slash commands: `.dial/commands/*.md` (`$ARGUMENTS`, `$1`…).
- Session commands include `/help /new /sessions /resume /fork /tree /rename /export /copy /plan /permissions /cost /compact /undo /rollback /todo /skills /memory /commands /search /approval /outcome /context /improve /exit`. Independent supported read tools run concurrently with mutation barriers and ordered results. Interrupted tool-call groups are closed with explicit results for safe resume; automatic compaction is announced. Process startup and non-cancellable external tools may still delay interruption.

## Automatic self-improvement

No `improve init`, hand-written evaluation suite, or manual training command is required. Normal sessions record trajectories and tool failures. After **5 new live turns**, a detached worker is queued; cycles run at most once every **6 hours**. On local MLX, the worker waits for the foreground session to release the shared compute lock. A new session may wait if training/evaluation already owns compute. This is automatic idle-time learning, not uninterrupted concurrent training and serving.

### What a cycle does

1. **Observe and monitor.** Keep tool events, repaired calls and completed conversations. The event-feature logistic probe ranks risky steps when enough externally scored trajectories exist and its held-out AUC is informative; predicted risk remains separate from ground-truth scores. `/outcome` supplies a human verdict and `/undo` records rejection. Shell exit codes alone are telemetry, not task-success labels.
2. **Practice with external checks.** Run 24 generated file-editing, scripting, search and structured-data tasks in disposable, strictly sandboxed workspaces. Files, JSON values, answers and command results are checked deterministically. Later cycles prioritize train families with low verified pass rates, high risk scores or recent tool-error categories; gate/anchor families never enter self-play.
3. **Build training data.** Harvest successful verifier-scored trajectories plus schema-invalid tool attempts that were later corrected and executed without error. A bounded preference pair keeps the malformed assistant response as rejected and the verified correction as chosen; other raw failures are not preference labels. Conversations remain in one train/validation split, and held-out prompts are excluded. `learn_from_sessions = false` excludes future harvesting of live conversations and repairs.
4. **Train locally.** An isolated feedback-model invocation proposes harness changes. Bounded mutations explore prompts, notes, tool selection, temperature, step budgets and edit mode. For supported local MLX models, LoRA resumes from the active adapter on up to 32 new verified records plus 16 accepted replay records; validation is capped at 32. A small reference-free preference pass adds chosen SFT loss plus a 0.1-weighted pairwise logistic ranking loss on repair pairs. The base model stays frozen; rejected candidates do not advance the training watermark.
5. **Validate before activation.** Evaluate against fresh primary and general tasks, plus a frozen anchor suite. Admission requires primary improvement, enough paired wins, an exact one-sided sign test (adjusted for the cycle's candidate count), and no suite dropping over one percentage point. The anchor also limits cumulative drift from the original version. Private case contents and failed evaluator details never go to the feedback model.
6. **Serve or reject.** At most one candidate is admitted per cycle, with an atomic SQLite baseline check. Rejected candidates never become active. New turns pick up admitted versions; adapter changes restart the local server. A live monitor rolls back when enough external verdicts show a configured drop from the predecessor.

LoRA is enabled by default but requires a supported checkpoint, **24 examples** in the bounded delta/replay batch, a nonempty validation split and a **6-hour training interval**. Training defaults to **32 iterations**. A candidate must still pass the full sealed gate before activation. Data-poor cycles can still evaluate harness changes; no gain is guaranteed.

This is not online learning on every prompt: normal requests do not change model weights. The cycle
may train a separate LoRA adapter for a supported local MLX model from accumulated verified examples.
The adapter only becomes active after the external gate admits it; rejected candidates do not alter
the serving model. Hosted endpoints can improve the harness, not their remote weights.

During live work, Dial may add up to three matching repair hints from the same project to the system context. It shows only tool names, argument field names and failure categories—not old argument values—and never bypasses current validation or approval.

The MLX server adapter preserves malformed model tool calls as explicitly invalid, non-executable calls rather than allowing the pinned upstream formatter to crash on missing arguments. These errors remain visible to the agent and the learning pipeline.

```sh
dial improve status                 # scheduler state, last cycle, scores, probe and layer sensitivity
dial improve versions
dial improve disable                # pause pending/future automatic cycles; a running cycle finishes
dial improve enable
dial improve cycle                  # optional: run a complete cycle now
dial improve train                  # optional: request a gated LoRA candidate now
dial improve diagnose
dial improve harvest
dial improve propose proposal.json
dial improve rollback
dial improve outcome 42 --score 100 --source human-review
```

`improvement.log`, datasets, adapter artifacts and the SQLite audit history live under the project's private state directory, outside the workspace. For custom use cases, an optional `[gate]` can point to your own independent JSONL suites; cases accept `expected`/`matcher` or a declarative `check`, and optional seeded `files`. Automatic generated tasks measure a bounded coding/tool-use curriculum, not arbitrary real-world correctness. Custom gates use their explicit point thresholds rather than the automatic curriculum's significance rule.

On hosted endpoints, the loop can improve the harness but cannot train weights; automatic evaluation and feedback use that provider and may incur API charges. Set `[improve] enabled = false` to disable recording and scheduling entirely. Set `[training] enabled = false` to keep the harness loop but prohibit weight training.

Still capability-unavailable: expert growth, routing recalibration, attention retrofit and a hidden-state neural probe. The implemented event-feature risk probe and gradient-norm layer scoring are not implementations or benchmark reproductions of the named research papers.

## Configuration

Layered TOML, user then project-specific: `~/.dial/config.toml`, `~/.dial/projects/<name>-<hash>/config.toml`, then flags. Never read from the project directory.

Learning stores and adapters are additionally isolated under
`~/.dial/projects/<name>-<hash>/models/<identity>/`, so switching providers/models does not reuse an
incompatible adapter. Sessions, skills, memory and prompt history remain project-scoped.

```toml
remote_model_idle_timeout_secs = 180

[ui]
theme = "dark"                    # dark | light | mono | gruvbox | nord | paper | auto
status = "default"                 # default | minimal | compact | full | nerd | ascii | custom
status_left = ["spinner", "model", "path", "git"]
status_right = ["context"]
status_separator = "powerline-thin"
composer_shape = "box"             # box | line
symbols = "auto"                   # auto | unicode | nerd | ascii
show_tools = true
history_limit = 500

[model_roles.compaction]
provider = "mlx"
model = "mlx-community/Qwen2.5-0.5B-Instruct-4bit"

[model_roles.subagent]
provider = "ollama"
model = "your-model-id"
base_url = "http://127.0.0.1:11434/v1"
```

The model picker persists `provider = "mlx"` when selecting local inference, explicitly clearing an
inherited remote endpoint. Helper roles use the same rule. A project role with `provider = "inherit"`
follows the main model even if a global role assignment exists. An admitted harness may pin an edit mode;
the settings panel labels that mode as pinned instead of claiming an ineffective override.

```toml
model = "mlx-community/Qwen2.5-7B-Instruct-4bit"
approval = "ask"          # ask | auto | readonly
integrations = ["bg", "web_fetch", "web_search", "lsp", "ast_search", "ast_edit", "dap"]
context_window = 32768
[sandbox]
mode = "dev"              # dev | strict | off
allow_network = true
extra_write = ["~/.cargo", "~/.npm"]
[endpoint]                # optional: OpenAI-compatible server instead of local MLX
base_url = "http://127.0.0.1:1234/v1"
api_key_env = "LMSTUDIO_KEY"
[harness]                 # the part the improvement loop is allowed to change
system_prompt = "Work in small verified steps."
tools = ["read","edit","write","glob","grep","bash","todo","tool_search","tool_load","artifact_read","working_state"]
[tools.lint]              # operator tools: trusted programs, not sandboxed
program = "/usr/local/bin/mylint"
timeout_secs = 60
[subagents]               # custom scouts: extra instructions, selected with task agent = "security"
security = "Focus on injection, authentication and secret-handling bugs."
[web]
search_url = "https://search.example/api?q={query}"   # optional; default is DuckDuckGo HTML
search_api_key_env = "SEARCH_KEY"                    # optional bearer token variable
[lsp.rust]                                           # override or add a language server
program = "/opt/homebrew/bin/rust-analyzer"
extensions = ["rs"]
[mcp.docs]                                           # stdio MCP server (trusted program)
program = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
timeout_secs = 60

[dap.python]                 # approved local DAP adapter process
program = "/workspace/bin/python-debug-adapter"
args = []
timeout_secs = 60

[verification]
enabled = true
repair_attempts = 1

[[verification.commands]]
name = "focused-tests"
command = "cargo test --offline --lib"
file_extensions = ["rs"]
timeout_secs = 120


[improve]
enabled = true
background = true
interval_secs = 21600
min_new_turns = 5
selfplay_tasks = 24
gate_primary_cases = 30
gate_guard_cases = 20
max_candidates = 3
alpha = 0.1
min_net_wins = 2
lora_min_examples = 24
learn_from_sessions = true
monitor_min_turns = 8
monitor_drop = 15.0
drift_tolerance = 3.0

[training]
enabled = true
interval_secs = 21600
iters = 32
batch_size = 1
rank = 8
num_layers = 4
max_seq_length = 8192
```

## Known limits

- Tool-calling quality depends on the model: 7B-class instruct models handle the tool set; sub-1B models often emit unreadable calls (which become training signal rather than being hidden).
- Local compute is serialized across projects. Background cycles wait for a resident foreground session to exit; new sessions wait if a cycle already holds compute. There is no claim of zero-downtime training.
- The TUI is keyboard-driven; not every terminal supports modified Enter keys. There is no cron or voice integration. Browser and kernel require available OS containment; they fail closed rather than falling back to unrestricted execution.
- Live memory/skill edits are the user's working state and are not gated; only the admitted harness is.
- Operator tools are trusted programs and are not sandboxed during live work; they are excluded from automatic evaluation. Non-macOS: `bash` is unavailable unless `sandbox.mode = "off"` for live use, while generated evaluations still require the strict sandbox.

## Source layout

```
src/agent/    agent loop, system prompt, project context, todo, typed subagents, approvals
src/tools/    Workspace + built-ins (files, search, shell, hashline, sandbox) and pluggable tools
              (ext.rs registry: bg, web, lsp, ast, mcp)
src/llm/      backends, providers, catalog, auth, local MLX server, tool-call parsing
src/learn/    self-improvement: curriculum, harvest, mutate, probe, gate, worker, verify
src/storage/  sessions, memory, skills, checkpoints, SQLite store, paths, settings
src/config/   configuration types and validation
src/ui/       terminal renderer, composer, themes, overlays
```

Adding a tool: implement `tools::ext::Tool` in its own module, return it from a `tools()` constructor, and list it in `Registry::standard`.
