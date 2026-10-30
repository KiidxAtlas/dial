# Configuration

Configuration layering, unlimited execution defaults and an annotated example configuration. Feature-specific settings are documented alongside each feature; see the [README index](../README.md#documentation).

## Configuration layers

Layered TOML, user then project-specific: `~/.dial/config.toml`, `~/.dial/projects/<name>-<hash>/config.toml`, then flags. Never read from the project directory.

Loading uses the same bounded private-file reader as settings updates. Missing layers are allowed;
unreadable, invalid UTF-8, oversized, linked or non-regular layers produce a path-specific error rather
than silently falling back to different settings. Files must be owned by the current user without
hard links; symlinked files and user-owned symlink ancestors are refused. Valid files are owner-private.

`dial config list` and `dial config get` use the runtime resolver, including defaults, migrations and
command-line overrides. Hooks append and deduplicate across layers rather than replacing earlier hooks.
Both text and JSON redact credential values recursively (also when requesting an entire table or array);
ordinary token budgets such as `harness.max_tokens` remain visible. Environment-variable names are not
credential values. The settings panel reports read failures instead of displaying empty fallback layers.

A running session reloads settings that change on disk, except its model choices. The main model
(`model`, `provider`, `endpoint`, `model_profile`, `context_window`) and `[model_roles]` stay as
they were when the session started, so picking a model in one terminal never switches another
terminal's session. A model picked in a session applies there immediately and is saved as the
default for sessions started later.

Learning stores and adapters are additionally isolated under
`~/.dial/projects/<name>-<hash>/models/<identity>/`, so switching providers/models does not reuse an
incompatible adapter. Sessions, skills, memory and prompt history remain project-scoped.

Service options are settings too. `fast_mode`, `slow_mode` and `extended_context` are the flags behind `/fast`, `/slow` and `/extended-context`; `/settings` → Model → Requests shows and toggles the same three rows, and `service_tier` sets an explicit tier that takes precedence over them. The session-only modes (`/vibe`, `/skillful`, `/advisor`, `/external-thinking`) are not stored in config and stay command-only.

## Interactive management and coverage

`/settings` exposes coding/interactive configuration; `/improve settings` exposes learning and
evaluation configuration. The shared registry in `crates/dial-llm/src/config/schema.rs` is the
definitive inventory for editable controls. Coverage tests compare it to serialized defaults and
Serde's field inventory, including optional fields and empty collections. Runtime-only fields and
the retired `ui.hyperlinks` compatibility input are excluded.

Simple settings use switches, choices and typed fields. Complex maps, nested options and arrays of
tables use a validated TOML editor, discoverable by parent or descendant path. Structured editors
manage only the selected global/project layer, so saving does not copy inherited values or credentials.
Sensitive editors are masked until explicitly revealed. See [Settings in the terminal interface](terminal-interface.md#settings)
for editing controls and surrounding setup navigation.
During a turn, shell display changes apply immediately on panel close; agent-bound settings are saved
and adopted at the next safe turn boundary.

Settings saves and prompt-history access wait at most 250 ms for each occupied storage lock.
Brief concurrent saves still serialize; longer contention reports a retryable error rather than
freezing command dispatch. A failed settings save leaves the effective setting unchanged; a failed
history save is reported without preventing the command from running.

Agent-bound settings such as `/drafter` can rebuild the agent and reconnect configured MCP servers.
The composer stays active during that connection pass, both at startup and after settings changes.
Once Enter or another action is queued, subsequent input is held in order until tools are ready;
it does not bypass initialization. Slow or unavailable servers still have their configured connection
deadline and report connection failures normally.

## Unlimited execution defaults

Most configurable execution limits and result/context size caps default to `0`, meaning **unlimited**:
worker idle timeout, `harness.max_steps`, `harness.max_tokens`, reasoning guard limits,
solve/escalation budgets, background daily compute, recalled context, history retention, and benchmark
case deadlines. A positive value restores a cap. Other deadlines and previews have finite defaults:
`shell.bash_timeout_secs` 300, hook `timeout_secs` 30, MCP `timeout_secs` 120 and `connect_timeout_secs`
30, extension `timeout_secs` 30, `request_recovery.silence_secs` 30, `efficiency.ambient_context_bytes`
8192, `efficiency.lightweight_requests` 4, `efficiency.cache_pressure_percent` 75 and
`efficiency.history_token_cap` 262144 (see below). The exception is
`verification.repair_attempts`: `0` selects the default of 3 automatic repair rounds (when
`verification.automatic_checks` is on), because a check that can never pass would otherwise re-prompt
the model forever.
Existing explicit settings are preserved; changing defaults does not rewrite saved overrides.

`harness.max_tokens = 0` means no operator output cap, not a literal zero-token API request.
Dial uses provider-advertised output capacity when available, otherwise the selected context capacity;
model profiles and the provider's actual output/context limits still apply. Context-window settings
remain `auto`, rather than pretending a model has infinite context. Sampling/training parameters,
approvals, sandboxing, host restrictions, protocol frame protections and loop-safety rules are unchanged.
Unlimited calls can wait indefinitely or consume substantial memory and money; Esc/Ctrl-C still cancels
interactive work. Configure positive limits where that is not acceptable.

`efficiency.history_token_cap` is the window that compaction and history pruning plan against when
the model's window is larger (0 = the model's full window). Every step resends the history, so on a
1M-token model the `auto` compaction trigger would otherwise fire near 850k tokens; at the default
it fires near 223k, while models with windows of 262144 tokens or less are unaffected by the cap.
Change it under `/settings` → **Model** → **Roles & Sampling** → **History token ceiling** (under *Show advanced*) or in
`[efficiency]`; validation accepts 0 or 8192..2000000.

Automatic compaction now applies at the planning window minus its response reserve (normally 85%
of the window), rather than 80% of that usable space. For example, a 200k window triggers near 170k
instead of 136k. Windows up to 65536 tokens retain the conservative 80%-of-usable policy. Background
summaries prepare before the trigger but do not replace history early. Explicit
`compaction.threshold_percent`, token thresholds and model/agent overrides remain in effect; the
independent response-headroom check is unchanged.

Set `efficiency.enabled=false` to disable these shared economy policies. Each completed/failed task
stores an `efficiency` completion snapshot in session control state. Structured print output and RPC
emit an `efficiency` event and include it in their final result/state. The report separates main and
worker usage by protocol/model, records request/retry and usage-coverage counts, and includes main-loop
schema/system/history/request-tail and tool-result byte measurements. These bytes are projections,
not billed tokens; missing provider counts stay distinguishable through coverage fields. Cache-token
semantics differ by provider, and late workers can update the live usage ledger after the completion
snapshot. Compare like-for-like tasks and correctness first; agent completion alone is not independent
proof of correctness or subscription quota savings.

The `inference_latency` object separates observed response wait, model loading, prompt processing,
generation, provider retry backoff, and cancellation in milliseconds, with request/failure/retry
counts. Each inference runtime result also retains its own latency breakdown and nullable
`first_generation_ms`. These are harness-observed wall times, not provider compute or exact token
timestamps: a provider that sends no progress events leaves network, queueing and prefill combined
under response wait. Generation includes reasoning and tool arguments. Concurrent worker timings
must not be added to estimate turn duration; compare the report's `duration_ms` separately.
The status distinguishes **waiting for first response** from **provider retry backoff**.

For reproducible local scheduling measurements (no paid model calls), run:

```sh
cargo test --locked --test integration scheduling_measurements -- --nocapture --test-threads=1
cargo test --locked --test integration scheduling_measurements -- --ignored --nocapture --test-threads=1
```

The first command verifies actual read results, reduced provider round trips from batching, and
managed background work overlapping independent foreground work. The second prints five alternating
serial/batched and foreground/background samples. The read fixture uses a scripted local provider;
the shell fixture simulates latency with two 200 ms waits. Neither measures a real model speedup,
and no timing threshold is used as a correctness assertion.

## Cost budgets and fallback revert

`[budget]` has two hard caps and two warnings, all in USD of observed cost (zero or unset disables
each). `max_turn_cost_usd` and `max_session_cost_usd` stop work at a step boundary. `warn_turn_cost_usd`
and `warn_session_cost_usd` only notify: each warns once when spend first reaches its threshold, and it
warns again only after spend has dropped below the threshold and risen back. Work continues after a
warning. The session total includes subagent spend because workers charge the same ledger. A session
resumed already past a warning threshold is not warned again in that run. Thresholds are read live,
so `/settings` changes apply to the next step. Validation requires finite nonnegative values.

```toml
[budget]
max_session_cost_usd = 5.0     # hard stop for the session, subagents included
warn_session_cost_usd = 2.0    # notice once when the session reaches $2
warn_turn_cost_usd = 0.25      # notice once per turn that reaches $0.25
```

`[retry] fallback_revert` controls when a session returns to its primary model after it failed over to
a fallback (see [model routing](model-routing.md)):

- `cooldown-expiry` (default): the session returns at the first request after the fallback's cooldown
  (the server's retry-after, else 5 minutes). This is the existing behavior, so upgrading changes
  nothing unless you set it.
- `never`: the session stays on the fallback until you switch models with `/model`, for operators who
  prefer a stable route over returning to a primary that may still be rate-limited.

A request in flight is never switched to another model; the return happens only at the next request
boundary. The fallback's first successful reply and each return to the primary are announced.

## Image generation (`[image]`)

`image_gen` is a shared tool: any tool-calling main model can use an available image route, even when that chat model cannot output images. An assigned **image role** wins; otherwise Dial first tries main, then configured image-capable model profiles (stable profile-name order), before the dedicated provider defaults. Image input (`vision`) is independent and never implies image generation. MLX-Serve's native `image` capability enables its PNG Images API. Catalog `experimental_supported_tools` / `supported_tools` containing `image_generation` enables the Responses hosted tool; `output_modalities` containing `image` enables compatible inline-image chat completions.

An operator can declare the transport for an exact model using its existing model profile:

```toml
[model_profiles.my-image-model]
provider = "chatgpt"
model = "exact-model-id"
[model_profiles.my-image-model.capabilities]
image_generation = "responses"
```

Supported declarations are `responses` (the selected model calls the hosted `image_generation` tool), `chat-completions` (inline base64 image data URLs), `images` (the selected model itself serves `images/generations`), and `disabled` (do not use this model for generation). Declaring support does not grant provider access. ChatGPT's official Codex Responses endpoint can offer the approval-gated hosted image tool even when catalog capability metadata is missing; this is **unverified model/account support**, not a claim that the chat model outputs images. An explicit `disabled` declaration prevents that attempt. ChatGPT uses the selected account's OAuth credentials, with no separate API key and no automatic fallback to API billing after a failure. Account access remains subject to private account restrictions and provider plan/model permissions.

`mlx-serve` declares the native MLX-Serve PNG-only Images API. Local endpoints can operate without an API key; remote endpoints still require credentials. MLX-Serve discovery defaults to `http://127.0.0.1:11234/v1`; use `MLX_SERVE_BASE_URL` or a configured provider for a different address. Servers on the LM Studio default port 1234 are also identified by their structured MLX-Serve metadata. After a successful rescan, cached rows remain visible as unverified while the server is stopped; start it again before generation.

Explicit `[image]` overrides take precedence over all automatic routing. `base_url` (default `https://api.openai.com/v1`), `model` (default `gpt-image-1`) and `timeout_secs` (default `180`, `0` disables) configure the dedicated Images API. `api_key_env` names its credential variable; when unset, Dial uses the provider's standard variable or a saved `/providers` key. Changing `base_url` or `model`, or setting `api_key_env`, always uses these settings as written. An assigned image role never falls back to another model on a configuration error.

Without overrides or an assigned role, automatic routing uses main's supported route (including ChatGPT's unverified hosted-tool attempt), then the active OpenAI/xAI provider's image model, then configured image-capable profiles in name order. Capability declarations can come from each profile or its cached catalog; ChatGPT profiles with unknown hosted-tool support are considered after the other profiles. Unavailable automatic candidates and disabled providers are skipped. The default OpenAI Images API is next, then xAI's image default if keyed. This lets local and other chat models use separately configured image services without changing main. Assign the image role to pin a model instead of relying on automatic selection.

With `auto = true` (default), the tool is exposed when a route resolves with the required credentials (including ChatGPT sign-in), or a local no-key endpoint. Discovery reads local state only: no generation, network probing or OAuth refresh occurs. `auto = false` restores the explicit `integrations = ["image_gen"]` opt-in. Calls always require approval. Customized routes and assigned-role errors are reported rather than silently hidden. A provider rejection never triggers another generation route. Output files are validated, never overwritten, and only published after successful completion; previews, incomplete responses and remote image URLs are not accepted as a completed image.

### Per-model image settings (`[image.models.<key>]`)

`/image settings` opens a panel for the image model the next `image_gen` call would use, through the same routing as above. It works like `/model settings`: `↑/↓` move, `←/→` cycle a choice or step a number, `Enter` types an exact value, `r` resets a row, `s` switches between project and global storage, and every change validates and saves immediately. Each entry matches one exact model identifier. The key is any bare TOML name; the panel derives `image_<16 hex>` from the model. Unset values leave the endpoint's own default. Saving never changes the chat model or its profile.

```toml
[image.models.flux]
model = "mlx-community/flux2-klein-9b-4bit"
size = "1024x1536"   # default when a call omits size; an explicit call size wins
steps = 8            # MLX-Serve, 1–200
seed = 42            # MLX-Serve, 0–4294967295; unset picks a random seed per image
strength = 0.6       # MLX-Serve edits only, 0–1: how far an edit may move from its source
loras = [{ path = "style.safetensors", scale = 1.25 }]   # MLX-Serve, at most 8

[image.models.gpt]
model = "gpt-image-1"
quality = "high"          # auto, low, medium, high (DALL·E 3: standard, hd)
background = "transparent"
moderation = "low"        # auto, low
```

Each image API receives only the settings it accepts, and the panel shows only those rows:

- MLX-Serve: `size`, `steps`, `seed`, `strength` (edits only) and `loras`.
- OpenAI Images and the Responses hosted image tool: `size`, `quality`, `background` and `moderation`. DALL·E takes only `size` and `quality`.
- xAI and inline chat-image output: only `size`.

Values set for another API are kept but not sent. A LoRA `path` is absolute, or a `.safetensors` file name inside `~/.mlx-serve/models/loras`; the panel offers the files installed there, and **All LoRAs** edits the full list for stacking. LoRA scales range from −8 to 8, and 1.0 (the default) is full strength. The provider still checks values against the specific model, and rejects any it does not support.

## Context files (`[context_files]`)

`context_files.extra` lists extra plain file names (for example `TEAM.md`) read beside each directory of the project hierarchy, after the built-in files. Names must be bare file names; path separators, `.`/`..`, empty entries, duplicates and the reserved built-ins (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `.cursorrules`, `GEMINI.md`) are rejected at validation. `foreign_sources` (for example `"windsurf"`) opts in to foreign user-level memory; `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` adds Copilot instruction directories.

## Example configuration

```toml
remote_model_idle_timeout_secs = 300         # 0 disables; local model servers are never limited

[reasoning_guard]          # operator-owned; not mutated by harness improvement
enabled = true
max_pre_action_secs = 0
max_reasoning_bytes = 0
require_plan = true         # after a gate hit with no open todo, only `todo` is offered until a plan exists

[codebase]
enabled = true
context_max_bytes = 0
history = true               # record per-turn file diffs for code_history and /code-history

[ui]
theme = "dark"                    # dark | light | mono | gruvbox | nord | paper | auto
status = "default"                 # default | minimal | compact | full | nerd | ascii | custom
status_left = ["spinner", "model", "path", "git"]
status_right = ["context"]
status_separator = "powerline-thin"
composer_shape = "band"            # band | box | line
symbols = "auto"                   # auto | unicode | nerd | ascii
show_tools = true
history_limit = 0

[message_routing]         # routing of a message typed during a turn when no explicit option is chosen
route = "steer"           # steer (deterministic, default) | classify (routing judge decides; failure steers)

[secrets]                  # mask secrets in provider-bound messages; see llm-transport.md
enabled = false
env = true                  # also mask values of KEY/TOKEN/SECRET/PASSWORD-named environment variables
values = []                 # extra literal secrets (at least 8 bytes each)

[model_roles.small]
provider = "mlx"
model = "mlx-community/Qwen2.5-0.5B-Instruct-4bit"

[model_roles.tester]
provider = "ollama"
model = "your-model-id"
base_url = "http://127.0.0.1:11434/v1"
```

The model picker persists `provider = "mlx"` when selecting local inference, explicitly clearing an
inherited remote endpoint. Helper roles use the same rule. A project role with `provider = "inherit"`
follows the main model even if a global role assignment exists. An admitted harness may pin an edit mode;
the settings panel labels that mode as pinned instead of claiming an ineffective override.

### Live local-model settings

Before each main local-model request, Dial shows its requested temperature, thinking switch and
output-token limit in the status bar. These are the **last request's overrides**, not measured engine
defaults. Unspecified values say `server default` or `model default`; no settings are shown before
the first request, and switching models clears the previous model's values. Narrow terminals may
drop the segment; custom status layouts can include `inference` in `ui.status_left` or `ui.status_right`.

A **Model settings** transcript notice appears for the first request and whenever those controls
change (including phase thinking and loop-recovery penalties), not for every token or result update.
The full notice and `/policy` include top-p, top-k, min-p, sampling source, explicit reasoning
effort/budget and reasoning-guard limits. No sampler or thinking policy is changed by this display.

```toml
model = "mlx-community/Qwen2.5-7B-Instruct-4bit"
approval = "write"        # ask | write | auto | readonly
integrations = ["bg", "web_fetch", "web_search", "lsp", "ast_search", "ast_edit", "dap"]
context_window = "auto" # defaults to 32768 tokens; serving endpoints may cap to loaded context
[sandbox]
mode = "dev"              # dev | strict | off
allow_network = true
extra_write = ["~/.cargo", "~/.npm"]
[shell]
backend = "auto"         # auto | system | embedded | inprocess (sandbox.mode = "off" only) | powershell
[endpoint]                # optional: OpenAI-compatible server instead of local MLX
base_url = "http://127.0.0.1:1234/v1"
api_key_env = "LMSTUDIO_KEY"
[harness]                 # the part the improvement loop is allowed to change
system_prompt = "Work in small verified steps."
tools = ["read","edit","write","glob","grep","bash","todo","tool_search","tool_load","artifact_read","working_state"]
[bash_interceptor]        # bash commands that duplicate a dedicated tool
mode = "hint"             # hint (default): run it and add a one-line pointer | block: refuse it
# enabled = false         # turn it off; patterns = [...] replaces the built-in rules
[tools.lint]              # operator tools: trusted programs, not sandboxed
program = "/usr/local/bin/mylint"
timeout_secs = 0
[task]
prefer_subagents = false  # default: coupled work and focused testing inline; true prefers specialists
[subagents]               # custom scouts: extra instructions, selected with task agent = "security"
security = "Focus on injection, authentication and secret-handling bugs."
[web]
search_url = "https://search.example/api?q={query}"   # optional; default is DuckDuckGo HTML
search_api_key_env = "SEARCH_KEY"                    # optional bearer token variable
search_provider = "auto"                             # auto | brave | tavily | exa | serper | kagi | jina | perplexity
                                                     # | google | bing | searxng | firecrawl | parallel | zai | xai
                                                     # | duckduckgo | startpage | mojeek | ecosia (keys come from *_API_KEY env vars)
[lsp.rust]                                           # override or add a language server
program = "/opt/homebrew/bin/rust-analyzer"
extensions = ["rs"]
[mcp.docs]                                           # stdio MCP server (trusted program)
program = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
timeout_secs = 0
cwd = "."                                            # optional; relative paths resolve against the project root

[dap.python]                 # approved local DAP adapter process
program = "/workspace/bin/python-debug-adapter"
args = []
timeout_secs = 0

[verification]
enabled = true
repair_attempts = 0

[[verification.commands]]
name = "focused-tests"
command = "cargo test --offline --lib"
file_extensions = ["rs"]
timeout_secs = 0

[verification.inputs."focused-tests"]   # optional: opt this check into dependency-aware reuse
paths = ["src", "tests", "Cargo.toml", "Cargo.lock"]
environment = ["FEATURE_FLAG"]

[improve]
enabled = true
background = true
interval_secs = 21600
min_new_turns = 5
selfplay_tasks = 24
gate_primary_cases = 30
gate_guard_cases = 20
max_candidates = 0
alpha = 0.1
min_net_wins = 2
lora_min_examples = 24
teach = false              # "Teach this model": record local-model coding sessions and serve admitted versions
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

Fresh LoRA training accepts ranks 1–128. Warm-start training can resume a validated composed
adapter with rank up to 1024; its saved layout supplies the effective rank and target keys.
Model identity, tensor shapes and finite weights are still validated before training. This allows
admitted adapter mixtures to continue learning without relaxing the fresh-adapter limit.

Verification reuse is enabled automatically for the exact built-in isolated Python AST check,
without an input declaration, when Dial recognizes a conventional Unix CPython installation.
It hashes the source and inherited environment and snapshots runtime directory membership and
Unix file identity (inode, size, permissions, modification and change timestamps). The runtime
scan is bounded to 16,384 entries and one second; workspace content snapshots retain the limits
below. Site packages are excluded because this check uses `-I -S`. Unknown layouts, wrappers,
virtual environments, loader/shell injection variables, symlinks or exceeded limits fall back to
execution. OS libraries are assumed stable within the turn. Other built-in builds/tests and custom
commands do not have inferred dependency contracts; custom checks still need declarations below.

`[verification.inputs."check-name"]` (keyed by a `[[verification.commands]]` name) is an explicit
operator assertion that the check is deterministic and side-effect free and that `paths` lists all
of its workspace inputs; `environment` names environment variables that also affect it. Paths are
workspace-relative with no globs; a directory is hashed recursively (contents and membership,
including ignored files); symlinks and special files are unsupported. For configured commands, an
absent declaration means the inputs are unknown and the check is never reused; empty `paths` is invalid.
When the declaration holds, a passing
controller-run result may be reused within the same turn by the default `check` and the completion
gate (never by `check` with `names`, which always runs, and never for reproductions) if fresh bounded
snapshots show the inputs unchanged and no shell or extension effect occurred since; the result is
labeled reused (`ran: false`). Missing, unreadable or oversized inputs disable reuse and the check
runs normally. Keep toolchains and external resources stable, and do not declare checks that depend
on network, time, randomness or other inputs you cannot list; Dial cannot detect an incomplete list.
Snapshots are limited to 4,096 entries, 64 MiB total file content and 250 ms per capture; hitting
any limit falls back to execution. Checks without a supported input snapshot also invalidate reusable evidence.
The example is illustrative, not a dependency list for every Cargo project: include build scripts,
workspace crates, configuration and fixtures that your command consumes. File timestamps and other
unlisted filesystem metadata are not tracked; do not opt in checks whose results depend on them.
