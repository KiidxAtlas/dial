# Provider and model catalog

Dial bundles the provider registry and model pricing/capability cards in `crates/dial-llm/src/llm/catalog/` (`providers.json` and `models.json`). This is Dial's own data, maintained in place: edit the JSON files directly and keep them valid. Each file's `source` header identifies the bundle. Third-party attribution for the catalog data is in `THIRD_PARTY_NOTICES` at the repository root.

`catalog::provider_endpoint` resolves known provider IDs to base URL, credential environment variable, and wire protocol. Selecting a bundled provider (for example `together` or `fireworks`) therefore needs only its own API key in the environment, without a local `[providers.*]` endpoint stanza. Credentials remain bound to the provider's configured origin; Dial does not forward a provider key to a different host.

The bundled model catalog exposes `known_context_window` and `model_capabilities` for inference budgeting and thinking configuration. Runtime discovery/provider metadata takes precedence over the bundled card. List prices are estimates, not invoices; missing usage or unpriced cards remain unknown. A provider-reported cost takes precedence over calculated list pricing where available, and subscriptions are not reported as API invoices.

## Model purpose and selection

`/models` separates **chat**, **embedding**, **image**, and **other** models using structured provider
metadata and exact catalog records, not words in a model's name or status. Unknown types remain
visible and chat-selectable for compatibility with custom endpoints; their details say **kind unknown**.
The CLI shows a **KIND** column; `dial models --kind embedding` filters by an exact declared type,
and `dial models --json` includes `metadata.kind`.

Enter on a chat model selects the main model. Enter on an embedding model assigns **embedding**,
not main; `/roles` and Ctrl+A expose compatible assignments. Model cycling and scoped chat-model
lists exclude known non-chat models. Enter on an image-generation model assigns the **image** role,
which follows the main model when unassigned. The **Images** group lists image generators; **Get models**
has an image-model shelf. Reranking and audio-only models remain under **other** and do not have a
general-purpose Dial role. Vision-capable chat models remain chat models. The request layer rejects
known non-chat types before making a chat request, including selections loaded from configuration.

LM Studio embeddings can be assigned to the embedding role without being marked unavailable merely
because they cannot chat. Downloaded embedding checkpoints are shown separately; Dial's supervised
MLX chat server does not serve them. Run an embedding-capable server and configure its endpoint.
See [embedding configuration](model-embeddings.md) and [local downloads](model-downloads.md).

Use `/providers` for endpoint, protocol and credential configuration, `/model settings` for
per-model controls, and [model routing](model-routing.md) for role assignments. Capability metadata
can be incomplete: an unknown capability is not proof of support. Operator profiles override reported
vision/tool support, context/output limits and supported request options; they do not install a new
runtime or turn an embedding checkpoint into a chat model.

## Independent authentication methods

In `/providers`, select a provider and choose **Use API key** or **Use sign-in or subscription**
where both routes are supported. The choice is saved as `providers.<id>.auth` and applies to that
provider's main and role backends. Switching methods keeps both credentials; signing in, signing
out, replacing a key, and removing a key do not change an explicitly selected method.
Changes apply between turns through the existing settings reload, without restarting Dial.

```toml
[providers.anthropic]
base_url = "https://api.anthropic.com"
auth = "api-key" # or "oauth" for Claude subscription sign-in
```

`api-key` uses only the API-key resolver (`--api-key`, environment, then saved URL-scoped key),
without reading or refreshing sign-in credentials. `oauth` uses only the provider's sign-in flow;
missing, unreadable, expired or rejected sign-in credentials never fall back to API-key billing.
Quota exhaustion does not automatically switch methods. API-key selection does not make an
OAuth-only provider endpoint accept API keys; the endpoint must support the chosen route.
OpenAI API-key usage remains `openai`, and ChatGPT subscription usage remains `chatgpt`.
OpenRouter browser sign-in creates an API key rather than a second subscription credential.

For compatibility, omitted `auth` (or `auth = "auto"`) preserves legacy credential selection:
canonical Anthropic prefers a stored subscription, and generic OAuth providers prefer a stored
grant. Choose a method explicitly to stop sign-in state from affecting routing. Saving a key
alone does not change this legacy choice. **Remove API key** removes only the saved key;
subscription/OAuth sign-out removes only that sign-in. Environment keys remain independently set.

### Anthropic subscription sign-in

Open `/providers`, select **Anthropic**, choose **Sign in with Claude subscription**, and complete
authorization in your browser. Select **Use sign-in or subscription** to pin that route, or
**Use API key** to keep the subscription signed in while using API billing.
Dial stores access and refresh tokens in its private user directory, refreshes access before
requests, fingerprints harness requests with Claude Code's billing-header and body attestation,
and sends subscription requests only to Anthropic's official API endpoint and protocol.
Custom Anthropic endpoints use their own API keys; subscription tokens are not sent to them.
Discovery follows the selected authentication method. Unreadable subscription storage can be
repaired by signing in again, or bypassed by explicitly selecting API key. Sign-out itself requires
readable state; it does not discard corrupt files. Providers with zero discovered models remain
in `/providers` so they can be configured.

## Providers and discovery

The catalog finds cached Hugging Face snapshots and models downloaded by LM Studio (its
`downloadsFolder`, default `~/.lmstudio/models`), listed under **Local models**: MLX checkpoints
are served by dial's own MLX server (so LoRA training applies), and running LM Studio, Ollama and llama.cpp
servers on their usual loopback ports. It also discovers OpenAI-compatible endpoints and Anthropic's
Messages API. OpenAI, Anthropic, Google Gemini, OpenRouter, Groq, Mistral and DeepSeek have setup presets.

SambaNova (`SAMBANOVA_API_KEY`, `https://api.sambanova.ai/v1`) and Inception (`INCEPTION_API_KEY`, `https://api.inceptionlabs.ai/v1`) are OpenAI-compatible chat-completions providers with bundled model cards (SambaNova `gpt-oss-120b`; Inception `mercury-2`, `mercury-2.5`, `mercury-3`). Inception's `minimal` reasoning rung is sent on the wire as `instant` (declared per model by the catalog's `m` effort-wire map); `low`, `medium` and `high` send their own names.

TypeSafe System One is a judgment-only backend, not a chat provider. Assign it to the `judge` role (`provider = "typesafe"`, `model = "jev-latest"` by default) and set `TYPESAFE_API_KEY`; `TYPESAFE_BASE_URL` overrides `https://api.typesafe.ai`. Thinking judgments then POST `{base}/v1/systemone` with a typed `choice` question. A missing key is reported when a judgment runs; it is never silently replaced by another model.

**GitLab Duo Agent** (`gitlab-duo-agent`, protocol `gitlab-duo-agent`, `https://gitlab.com`) runs GitLab's Duo Agent Platform workflows over a WebSocket. Authenticate with `GITLAB_TOKEN` (personal access token with `api` scope) or the `gitlab-duo-agent` OAuth sign-in. Set `GITLAB_DUO_PROJECT` to a full project path (`group/subgroup/project`) to choose the project; when unset, Dial uses the first project where the user has Developer or higher access. Nested projects resolve their root group from the namespace's full path. Dial creates a workflow for the conversation and streams the agent's checkpoint text as it arrives. Server-side `runMCPTool` actions become ordinary Dial tool calls, so approvals, sandbox confinement and tool output apply unchanged; results return to the same workflow. Workflow approval statuses that carry a tool action surface as that tool call through the normal approval gate, and a denial is sent back as the tool result. An approval status with no tool action still fails the turn. Workflows are reused across turns of one session. Resetting, cancelling or forgetting a session sends a best-effort REST stop (5-second cap) before closing the socket. Step-limit and transient "Duo Agent Platform" errors restart the workflow with the transcript (at most four step-limit restarts and one transient retry); text already streamed in the turn is not repeated after a restart.

**Local model formats.** Dial detects the format from the files in the model directory. Both formats share the local route
(`provider = "mlx"` in configuration), but `/models` names GGUF entries `gguf/…` and its detail line names the serving
runtime; a GGUF entry's source notes when `llama-server` is not installed yet.
- **MLX** (`config.json` + safetensors): served by `mlx_lm`/`mlx_vlm`; supports LoRA, steering and runtime tuning.
- **GGUF** (a `.gguf` file, or a directory holding one; sharded files load from the first shard): served by llama.cpp's `llama-server` (found via `DIAL_LLAMA_SERVER`, `PATH`, `/opt/homebrew/bin` or `/usr/local/bin`) with `--jinja` and one slot. Tool and reasoning support come from the chat template in the GGUF header; a sibling `mmproj*.gguf` is passed as `--mmproj` and marks the model as vision-capable. `--ctx-size` is the configured context window, or else the largest window the memory estimate below allows (never the full native window by default). Constrained repair uses llama.cpp's JSON-schema grammar, and sampling names are translated (`repeat_penalty`, `repeat_last_n`). `[local_runtime]` maps `draft_model` (a local GGUF) and `num_draft_tokens` to `--model-draft`/`--spec-draft-n-max`, and `kv_bits` 8 or 4 to a `q8_0`/`q4_0` KV cache; the MLX-only `kv_group_size` and `quantized_kv_start` are rejected. An adapter must be a GGUF LoRA (`--lora`); steering and LoRA training need MLX weights.
- **Splash** (`manifest.json` with `target/` and `tokenizer/`): only LM Studio can run these. Load the model in LM Studio and select it under the LM Studio provider; Dial's own launcher reports this instead of failing obscurely.

GGUF context fitting distinguishes unavailable memory information from an exhausted budget. With
known physical memory, a model that cannot fit the minimum 256-token context within the estimated
serving budget is refused before launch with a smaller-model/quantization hint; it never falls back
to allocating the full native window in that case. Unknown memory retains the native/requested fallback.

The improvement pipeline probes capabilities per format: MLX runs the full gradient and adapter reload probe, GGUF is reported as inference-only (harness improvement and evaluation proceed, weight training is skipped), and Splash is reported unsupported with the LM Studio hint.

**Local chat templates.** Dial combines leading system/developer instructions into one system
message for its supervised local servers. Later instructions and request-local guidance stay at
their original conversation boundary as user messages; tool calls, tool results and images keep
their order. This wire-only projection leaves saved history and remote-provider requests unchanged.
It supports templates such as Ornith's that reject system messages outside the first position.

**Train once, serve anywhere.** `dial improve fuse [VERSION] --out <dir>` merges a LoRA version (default: the active one) into its MLX base and writes a standalone MLX checkpoint at the base quantization. Dial, LM Studio and `mlx_lm` then serve the improved model without loading an adapter. `--dequantize` writes full-precision weights instead, the input llama.cpp's `convert_hf_to_gguf.py` expects when you want a GGUF build. Dial does not write GGUF itself: `mlx_lm`'s exporter produces files current llama.cpp cannot load for BPE tokenizers (Llama 3, Qwen, SmolLM and similar), and llama.cpp's converter needs PyTorch, which Dial's runtime does not install.

**Choosing a local model.** `dial models recommend` estimates which downloaded MLX checkpoints and GGUF files fit this machine (Splash bundles are not sized). It reads physical memory (`sysctl hw.memsize` on macOS, `MemTotal` on Linux; `--memory-gb` overrides), each model's weights (safetensors, or every GGUF shard) and its layout (`config.json`, or the GGUF header's block, head and key/value widths), and any `dial reliability check` reports recorded for it. It loads no model and makes no network calls, so it works before any runtime is installed. Flags: `--context <tokens>` (default: the configured context, else 32768; capped by the model's native maximum), `--kv-bits <2|3|4|6|8>` (estimate a quantized KV cache; default fp16; llama.cpp serves only 4 and 8), `--memory-gb <GiB>`, and `--json`. Each row shows weights, KV cache at the requested context, resident total, the largest context that stays within the comfortable budget, and recorded reliability. Estimated resident size is weights + KV cache + 1.5 GiB runtime allowance + 5% of weights. A model is `fits` at or below 60% of RAM, `tight` up to 75%, and `wont_fit` above that; `unknown` when memory cannot be read. The same estimate sizes `llama-server --ctx-size` when Dial launches a GGUF model without a configured context window, so llama.cpp does not preallocate a native window the machine cannot hold. Suggested downloads are listed by RAM tier and are not measured on this machine. Recorded reliability is shown only as `passed/total` with its age; reports older than seven days are marked stale, and the column never certifies coding accuracy. The JSON output (`models`, `suggestions`, `memory_bytes`, `memory_source`, `context_tokens`, `kv_bits`) is the same data as the table.

Use `/model defaults` (or press `d` in `/model settings`) for direct access to shared context
defaults. This opens in global scope; switch scope for a project-specific default.
`/settings` → Model → Context offers **Current model context**, **Cloud context default** and
**Local context default**. Each accepts `auto` (the default) or a token cap from 2,048 to 2,097,152.
Auto follows the selected model's catalog/downloaded metadata; unknown capacities fall back to 32,768.
ChatGPT discovery uses `max_context_window` when valid and larger than the standard `context_window`
default, so the picker and Auto budget expose the account's extended capacity instead of stopping at 272k.
Subscription capacities come from the account catalog and may differ from API model-card capacities.
A numeric current-model override takes precedence over the cloud/local default, and known model or
loaded-server capacities cap the result downward. Local defaults also cover loopback/LAN endpoints.
Picking a different model resets its current override to Auto while preserving both family defaults.
The window controls retained history and compaction; it does not send that many tokens on every
request or reserve that much model memory. Actual usage appears before the slash in the context meter.
Configure the same values in TOML, for example `context_window = "auto"`,
`cloud_context_window = 64000`, and `local_context_window = 16384`.

Anthropic discovery reads `max_input_tokens`; older catalogs with missing windows use
documented limits for known models, including 1M tokens for Claude Opus 5.5. The status meter and
compaction share this resolved window. Custom endpoints use their own reported limits.

**LoRA indicators.** `/improve models` is the dedicated learning model picker. It has a LoRA column: `unverified` for a local MLX model whose architecture
the installed mlx-lm implements (a static check of `config.json`, not proof of trainable adapter targets), `unsupported` when Dial’s adapter probe rejects it, and `-` for models another server runs. The main model distinguishes awaiting training,
training, paused for coding, evaluating, accepted vN, loading, loaded, used on last response, and request failed.

Local training yields to a new coding request at a safe step boundary. Dial saves adapter weights,
optimizer state, RNG state and the batch cursor, exits the training process to free GPU memory,
and hands compute to the coding turn. Idle chats unload their local models when the paused trainer
requests compute back; training resumes the same candidate and remaining iterations automatically.
The currently admitted adapter stays in use until the candidate passes evaluation. Dial allows
two seconds for cooperative handoff before terminating the training subprocess. `/improvement`
and `dial improve jobs` show the current phase and iteration progress. Operator stop also cancels
the idle wait. Resume occurs within the running improvement cycle; restarting a stopped or crashed
cycle does not automatically recover its unfinished candidate. Older already-running workers can
be stopped from the compute menu and restarted with the updated binary.

Open `/compute`, or **Settings → Setup → Compute & background work**, for a live menu of compute
ownership and processes across all projects. **All**, **Models**, **Background**, **Services** and
**Tools** tabs separate model servers, capability checks, improvement jobs, Laya and tool subprocesses.
Live processes and the compute owner appear green, including models still loading. Process rows show
PID, CPU use and memory in decimal GB. On macOS this uses physical footprint, including compressed
and GPU-backed allocations, with RSS as a fallback. The header totals Dial processes and sessions,
shared services, and their combined memory; shared services are counted once. Process details show
RSS separately. These observational totals are not a measurement of the whole machine's memory use.
Use Left/Right or Tab to switch sections, Enter for details, and **x** to stop/unload a process or
remove and disable a background job. Saved model weights, adapters and session history are preserved;
models reload on their next use. Orphaned model servers from older Dial versions are included too.
Pause/Resume background work and Start/Stop controls remain available. While waiting for compute,
Ctrl+, opens these controls directly. Status refresh and job controls run off the terminal thread.

**Compute locks** in **Settings → Model → Local worker** defaults to on. Foreground sessions share
the lock, so local models and Laya run side by side across Dial instances and never wait on each
other; only background training, verification and maintenance take it exclusively, and they yield
to every foreground request. Turn it off (`compute_locks = false` in TOML) to run background work
concurrently with sessions too, without acquiring or waiting for the lock. This is a per-configuration
policy: other sessions and already-running workers retain their own settings; restart background
workers to apply a change. Concurrent work can exhaust RAM/GPU memory. Explicit background pause,
stop and cancellation controls remain active; data-integrity locks are not disabled.
Laya decision and judge roles never make a session acquire the shared compute lock.

Idle chats keep resident main and role models loaded between turns, so the next turn starts
without reloading the weights. Once the turn and its child agents finish, they unload them only
when training, verification or other queued work requests compute; the next turn then reloads the
model. Browsing Models, Model Roles or Compute does not unload resident models. `/model restart`
and explicit compute actions can release a model; verification and evaluation also yield for coding requests.
If training does not acknowledge a pause within two seconds, Dial terminates its subprocess and
resumes from its last committed step later. A checkpoint is committed at every step, so only an
unfinished step may need repeating. **Stop job now** targets a verified detached background worker
and its child processes, preserves the admitted adapter, and abandons that unfinished cycle; it
never terminates a foreground chat. This also provides recovery for older workers without preemption.

**Changing model settings mid-session.** Saving settings rebuilds the agent between turns, never during a request. The running local server is handed to the rebuilt agent and reused while the model, adapter, steering, `[local_runtime]`, tool grammar and (for GGUF) the context window that sized `--ctx-size` all still match; any difference relaunches it on the next request. Sampling and prompt settings therefore apply on the next turn without reloading weights.

A failed capability probe overrides the static architecture check; only a successful probe can show
awaiting training. Architecture support alone never claims LoRA compatibility.
Ordinary startup and model browsing do not consult learning records or show LoRA controls. Use `/improve models` for explicit capability verification, and `/improve settings` for learning preferences. Unverified models stay unverified until an explicit F6 check or improvement
work needs the capability probe. Results and reasons are saved in each model's private project store
and reused across restarts. F6 runs a check with diagnostics after runtime/model changes under the shared
compute lock. Verification loads the model/tokenizer, selects architecture-compatible linear
projections, performs a rank-one gradient
update on a short prompt, and checks that a saved/reloaded adapter preserves output. The temporary
adapter is discarded. Ordinary attention keeps query-only targets; hybrid, recurrent, and fused blocks
use compatible output or other linear projections. Training and adapter validation share this selection.
Old probe results are rechecked on the next explicit verification or improvement run.
The managed Python worker updates with the binary; explicit custom worker paths are preserved.
Verification does not establish that a full training run will fit memory or improve quality.
Accepted means the gate admitted the version; loaded requires the local server's startup generation
check; used requires a successful, uninterrupted model response with that adapter. Runtime evidence
is session-local and clears when the server is released. Training/evaluation indicators require a live
worker lock, so a crashed worker cannot leave a permanent busy label. Disabling future training does
not hide an already accepted adapter. `/improve` shows the runtime adapter path, latest LoRA gate
decision and baseline/candidate scores, plus the last training skip reason. Scores measure the gate's
tasks; successful adapter use alone does not prove improvement. Once the learning loop has probed
the model, its recorded capability replaces the static architecture check.

`/providers` can add a custom endpoint, set/remove a URL-scoped API key, and choose a model. OpenRouter
also offers **Sign in with OpenRouter** (PKCE), which creates a user-controlled API key without asking
you to paste one. Dial checks that key against OpenRouter's protected `/api/v1/auth/key` endpoint
before saving. The option appears only for OpenRouter's official endpoint, so a generated key is never
forwarded to a custom gateway.

When entering OpenAI or Anthropic API keys, Dial checks them against each provider's model-list
endpoint before saving; this is metadata, not an inference call. OpenAI uses bearer authentication;
Anthropic API keys use `x-api-key` and `anthropic-version`. Other custom endpoint keys are saved
without an online check. Environment credentials take precedence over saved API keys. Saved keys are
**owner-private plaintext** in `credentials.json`, not an OS keychain; they are never written into
TOML or rendered back into the UI. Failed discovery keeps cached models marked unverified.
Remote routes use the declared protocol: OpenAI-compatible chat completions, Responses, Anthropic
Messages, Gemini or Ollama native streaming. Both **ChatGPT subscription** and **Claude subscription**
have dedicated OAuth sign-in flows and use their respective official inference APIs.

**Use your ChatGPT/Codex subscription.** Restart Dial, open `/providers`, select **ChatGPT
subscription**, and choose **Continue with ChatGPT (add account)**. In the browser, authorize Dial
to use your ChatGPT plan. Return to Dial, open `/models`, and assign an account model to the main
role. Eligible plan access and shared limits are controlled by your account; the model catalog is
not an entitlement check, and your first completed inference request verifies access. Manage Dial's
permission and limits in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

The provider menu lets you choose a saved workspace, sign in again, or sign out. Dial refreshes
rotating tokens under a cross-process lock. Sign-in continues if you close the provider panel; use
**Cancel pending sign-in** in its menu to stop it. OAuth credentials are stored in owner-private,
atomic `chatgpt-accounts.json` storage under Dial's user directory (plaintext, not an OS keychain),
separately from API keys and editable project files. Requests use the ChatGPT Codex Responses
endpoint at `chatgpt.com/backend-api/codex/responses`, include the account ID, and never fall back
to an API key. A partial or failed response never releases its tool calls. Dial follows OpenAI's
[Sign in with ChatGPT flow](https://developers.openai.com/siwc/token-sharing-open-source). Existing
accounts created by Dial's older login flow must sign in again. Legacy
`https://api.openai.com/v1` ChatGPT endpoint settings are migrated to the Codex backend on load.

Selecting the separate `openai` provider uses API-key authentication, even while a ChatGPT account is
signed in. Signing into ChatGPT does not change the credentials used by `openai`.

`retry.rotate_accounts` permits automatic ChatGPT failover only for unbound requests with
portable history. A successful account selection stays pinned on that backend for subsequent
requests. Session/cache identities and native Responses history are account-scoped: Dial refuses
automatic cross-account replay rather than sending incompatible history or silently discarding it.
For those conversations, explicitly select another allowed account and start a clean chat (or
explicitly rebind portable history). Rotation also requires compatible cached model capabilities
unless an explicit configured model profile supplies the request contract. Account allowlists and
native replay validation still apply.

Other OAuth providers can store several grants. Each grant is one account, keyed by its account ID,
else its email, else `account-N`. The first account added is active. Selection is per session and
sticky: a session keeps its account until a failure rotates it. On a rate limit, quota exhaustion,
or a rejected credential, the failed account cools down for its retry-after time (or the default
cooldown) and the session moves to the next usable account in key order, when
`retry.rotate_accounts` is enabled. When every account cools down, the request fails with the
provider's error. Refreshing a grant holds a per-account file lock, so concurrent processes refresh
one at a time and reuse the result. Existing single-grant `oauth-<provider>.json` files load as a
one-account store and are rewritten in the multi-account shape on the next save. The ChatGPT
`chatgpt-accounts.json` file already uses that shape and is unchanged.

Manage those accounts from `/providers`. For an OAuth provider with a stored grant, choose **Manage
<provider> accounts**: each account has **Use** (make it the account new sessions start on) and
**Remove** rows, and **Add another <provider> account** starts a sign-in that adds one more grant.
Removing the last account deletes the provider's grant file, as **Sign out of <provider>** does. The
same operations are available from the shell: `dial auth accounts <provider>` (or
`dial auth accounts <provider> list`, with `--json` for scripts) lists accounts and marks the active
one and any cooling down, `dial auth accounts <provider> select <key>` makes an account active, and
`dial auth accounts <provider> remove <key>` removes one. Use the keys that `list` prints. ChatGPT
accounts are managed in its own rows above and are not part of these commands.

Dial combines the account model list with visible models in a recent local Codex model metadata
cache, when present, so newly released models can appear before the account list catches up.
Those additions are labeled **Codex catalog**; their first completed request verifies account access.
Dial reads model metadata only from Codex and uses its own OAuth credentials for every request.
For ChatGPT models, `/reasoning` shows the current and supported levels. Use `/reasoning high`,
`/reasoning max`, or another advertised level to change it; `/thinking high` is an alias. Choices
persist per model in this project's settings and are sent through the Responses reasoning field.

**LM Studio.** Dial reads LM Studio's native model list (`/api/v0/models`, loopback or LAN hosts only) to show
whether each model is `loaded` (with its real context length) or `loads on first use`, and identifies
embedding models for the embedding role rather than chat. LM Studio loads a model on demand with a
context length of its own choosing, often far below Dial's 32768 default (an 8192 load was seen in practice); Dial asks the server
what is actually loaded and plans compaction and the context meter against the smaller number. A model
LM Studio refuses to load (memory guardrail, unsupported architecture) is reported with the server's own
message; load it in LM Studio first, with a smaller context, or choose another model. Larger models need
LM Studio's own resource limits to allow them; Dial cannot override those.

**llama.cpp and Ollama.** The same runtime check covers them: llama.cpp's `/props` reports the per-slot
context it serves (`-c` divided by `--parallel`; its `/v1/models` lists only the training context), and
Ollama's `/api/ps` reports the context a running model was loaded with. For a model Ollama has not loaded
yet, `/api/show` supplies its trained context as the bound. Ollama's own default context is small and it
truncates oversized prompts silently, so Dial always sends the window it plans against as `options.num_ctx`
(a model profile's `context_window` takes precedence); `OLLAMA_CONTEXT_LENGTH` is no longer needed.

**Ollama model profiles.** A `provider = "ollama"` profile accepts `local_sampling` keys `top_k`, `min_p`,
`seed`, `repetition_penalty` (sent as `repeat_penalty`), `repetition_context_size` (`repeat_last_n`),
`presence_penalty` and `frequency_penalty`, all sent in `options`; other local sampling keys are rejected.
`keep_alive` (`"30m"`, `"2h"`, `300`, `-1` to keep the model loaded) controls how long Ollama keeps the model
resident between requests, avoiding a reload after Ollama's default few idle minutes.

**One request at a time.** A model server on this machine or the LAN (Dial's own runtime, or an endpoint on a
loopback, private-range or `.local` host) receives one model request at a time across the main agent and
workers, so parallel requests neither queue inside a single-slot server nor evict each other's prompt cache.
For a server started with several slots (for example `llama-server --parallel 4`), raise it with
`task.provider_concurrency.<provider> = 4`.

Use any OpenAI-compatible server instead of the local MLX model (LM Studio, Ollama, llama.cpp, a hosted API): `dial --endpoint http://localhost:1234/v1 --api-key-env KEY -m <model-name>`. Weight updates need the local MLX model.

## Reasoning effort wire mapping

A model's `m` (`reasoning_effort_wire`) map sends a declared effort under a provider-specific name, for example Inception's `minimal` → `instant`. The mapping applies to every request that carries an effort value: chat-completions `reasoning_effort`, Responses `reasoning.effort`, Gemini `thinkingLevel`, Bedrock `output_config.effort` and Ollama `think`. Efforts without an entry send their Dial name unchanged.

## Local MLX runtime tuning and cost accounting

**Local MLX runtime tuning.** `[local_runtime]` sets optional MLX-LM server flags for Dial-owned text models: `draft_model` (speculative decoding with a draft model; empty disables), `num_draft_tokens`, `kv_bits` (2, 3, 4, 6 or 8; KV-cache quantization), `kv_group_size` (32, 64 or 128) and `quantized_kv_start`. Each `"auto"` value leaves the mlx-lm default. A model profile may set its own `local_runtime` table, which replaces the global one for that model. While a draft model is set, tool grammar, thinking control and activation steering are disabled; steering is bypassed with a notice rather than failing. Quantized KV caches disable request batching, and quality loss below 8 bits can be severe: with mlx-lm 0.32 and Qwen2.5-7B-Instruct-4bit, `kv_bits = 4` produced corrupt output once a prompt passed `quantized_kv_start` (upstream server, independent of Dial), while `kv_bits = 8` answered correctly. Prefer 8 and measure anything lower. Vision models reject these settings because MLX-VLM does not accept them. `dial benchmark` accepts the same values as per-configuration overrides (`draft_model`, `num_draft_tokens`, `kv_bits`, `kv_group_size`, `quantized_kv_start`), applied only to MLX providers and recorded in its settings output. Keep a draft model only when the benchmark shows a gain.
`/cost` separates main and worker usage, logical requests and actual transport/retry attempts, with
per-model and usage-report coverage. Observed priced spend includes accepted maintenance/helper replies,
including discarded results; unknown prices or missing usage stay explicit rather than becoming free.
Subscription counters are observed activity, not an invoice or quota forecast. Reasoning may overlap
output and must not be added twice. The live ledger is not a reconstructed cross-process billing total.
