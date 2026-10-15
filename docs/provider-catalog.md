# Provider and model catalog

Dial bundles the provider registry and model pricing/capability cards in `src/llm/catalog/` (`providers.json` and `models.json`). This is Dial's own data, maintained in place: edit the JSON files directly and keep them valid. Each file's `source` header identifies the bundle. Third-party attribution for the catalog data is in `THIRD_PARTY_NOTICES` at the repository root.

`catalog::provider_endpoint` resolves known provider IDs to base URL, credential environment variable, and wire protocol. Selecting a bundled provider (for example `together` or `fireworks`) therefore needs only its own API key in the environment, without a local `[providers.*]` endpoint stanza. Credentials remain bound to the provider's configured origin; Dial does not forward a provider key to a different host.

The bundled model catalog exposes `known_context_window` and `model_capabilities` for inference budgeting and thinking configuration. Runtime discovery/provider metadata takes precedence over the bundled card. List prices are estimates, not invoices; missing usage or unpriced cards remain unknown. A provider-reported cost takes precedence over calculated list pricing where available, and subscriptions are not reported as API invoices.

## Anthropic subscription sign-in

Use Dial's browser OAuth flow for a Claude subscription: open `/providers`, select **Anthropic**,
then choose **Sign in with Claude subscription** and complete authorization in your browser.
Dial stores access and refresh tokens in its private user directory, refreshes access before
requests, fingerprints harness requests with Claude Code's billing-header and body attestation,
and sends subscription requests only to Anthropic's official API endpoint. An Anthropic API key
remains supported; sign out of the subscription in `/providers` to use the API-key route.
Providers with zero discovered models are hidden in `/models`, but remain in `/providers`
so they can be configured.

## Providers and discovery

The catalog finds cached Hugging Face MLX snapshots, MLX models downloaded by LM Studio (its
`downloadsFolder`, default `~/.lmstudio/models`; listed under Local MLX and served by dial's own MLX
server, so LoRA training applies), and running LM Studio, Ollama and llama.cpp
servers on their usual loopback ports. It also discovers OpenAI-compatible endpoints and Anthropic's
Messages API. OpenAI, Anthropic, Google Gemini, OpenRouter, Groq, Mistral and DeepSeek have setup presets.

**Choosing a local MLX model.** `dial models recommend` estimates which downloaded MLX checkpoints fit this machine. It reads physical memory (`sysctl hw.memsize` on macOS, `MemTotal` on Linux; `--memory-gb` overrides), each checkpoint's safetensors size and `config.json`, and any `dial reliability check` reports recorded for it. It loads no model and makes no network calls, so it works before any runtime is installed. Flags: `--context <tokens>` (default: the configured context, else 32768; capped by the model's native maximum), `--kv-bits <2|3|4|6|8>` (estimate a quantized KV cache; default fp16), `--memory-gb <GiB>`, and `--json`. Each row shows weights, KV cache at the requested context, resident total, the largest context that stays within the comfortable budget, and recorded reliability. Estimated resident size is weights + KV cache + 1.5 GiB runtime allowance + 5% of weights. A model is `fits` at or below 60% of RAM, `tight` up to 75%, and `wont_fit` above that; `unknown` when memory cannot be read. Suggested downloads are listed by RAM tier and are not measured on this machine. Recorded reliability is shown only as `passed/total` with its age; reports older than seven days are marked stale, and the column never certifies coding accuracy. The JSON output (`models`, `suggestions`, `memory_bytes`, `memory_source`, `context_tokens`, `kv_bits`) is the same data as the table.

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

**Compute locks** in **Settings → Model → Local worker** defaults to on. Turn it off
(`compute_locks = false` in TOML) to run local models, training, verification and maintenance
concurrently, without acquiring or waiting for the shared compute lock. This is a per-configuration
policy: other sessions and already-running workers retain their own settings; restart background
workers to apply a change. Concurrent work can exhaust RAM/GPU memory. Explicit background pause,
stop and cancellation controls remain active; data-integrity locks are not disabled.
Laya decision and judge roles never make a session acquire the shared compute lock.

Idle chats unload resident main and role models as soon as the turn and its child agents finish,
including when no other job is waiting. Queued follow-ups keep their models; the next turn reloads
an unloaded model. Verification and evaluation also yield for coding requests.
If training does not acknowledge a pause within two seconds, Dial terminates its subprocess and
resumes from its last committed step later. A checkpoint is committed at every step, so only an
unfinished step may need repeating. **Stop job now** targets a verified detached background worker
and its child processes, preserves the admitted adapter, and abandons that unfinished cycle; it
never terminates a foreground chat. This also provides recovery for older workers without preemption.

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

**llama.cpp and Ollama.** The same runtime check covers them: llama.cpp's `/props` reports the per-slot
context it serves (`-c` divided by `--parallel`; its `/v1/models` lists only the training context), and
Ollama's `/api/ps` reports the context a running model was loaded with. Ollama's default is small and it
truncates oversized prompts silently, so raise it with `OLLAMA_CONTEXT_LENGTH` (or `num_ctx` in a
Modelfile) for agent work; Dial learns the size after the first reply that loads the model.

Use any OpenAI-compatible server instead of the local MLX model (LM Studio, Ollama, llama.cpp, a hosted API): `dial --endpoint http://localhost:1234/v1 --api-key-env KEY -m <model-name>`. Weight updates need the local MLX model.

## Local MLX runtime tuning and cost accounting

**Local MLX runtime tuning.** `[local_runtime]` sets optional MLX-LM server flags for Dial-owned text models: `draft_model` (speculative decoding with a draft model; empty disables), `num_draft_tokens`, `kv_bits` (2, 3, 4, 6 or 8; KV-cache quantization), `kv_group_size` (32, 64 or 128) and `quantized_kv_start`. Each `"auto"` value leaves the mlx-lm default. A model profile may set its own `local_runtime` table, which replaces the global one for that model. While a draft model is set, tool grammar, thinking control and activation steering are disabled; steering is bypassed with a notice rather than failing. Quantized KV caches disable request batching, and quality loss below 8 bits can be severe: with mlx-lm 0.32 and Qwen2.5-7B-Instruct-4bit, `kv_bits = 4` produced corrupt output once a prompt passed `quantized_kv_start` (upstream server, independent of Dial), while `kv_bits = 8` answered correctly. Prefer 8 and measure anything lower. Vision models reject these settings because MLX-VLM does not accept them. `dial benchmark` accepts the same values as per-configuration overrides (`draft_model`, `num_draft_tokens`, `kv_bits`, `kv_group_size`, `quantized_kv_start`), applied only to MLX providers and recorded in its settings output. Keep a draft model only when the benchmark shows a gain.
`/cost` separates main and worker usage, logical requests and actual transport/retry attempts, with
per-model and usage-report coverage. Observed priced spend includes accepted maintenance/helper replies,
including discarded results; unknown prices or missing usage stay explicit rather than becoming free.
Subscription counters are observed activity, not an invoice or quota forecast. Reasoning may overlap
output and must not be added twice. The live ledger is not a reconstructed cross-process billing total.
