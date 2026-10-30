# LLM transport behavior

The shared LLM layer provides bounded streaming, provider-aware error classification, retries, and request budgeting. Context-overflow classifications are kept attached to errors before any header redaction so compaction remains reliable. HTTP 413 is classified as a payload failure unless the body identifies a token/context limit; it must not trigger token compaction merely because its status is 413.

`[retry]` settings bound attempts and delays. Ordinary automatic provider retries wait a fixed
300 seconds (five minutes), bounded by `retry.max_delay_secs`, rather than increasing an
exponential backoff. Explicit `retry-after`, `retry-after-ms`, and `x-ratelimit-reset[-ms]`
delays are still honored as minimum waits; a hint beyond `retry.max_delay_secs` stops the request
instead of waiting longer than configured. Window-reset headers are treated as quota information,
not mandatory waits. A throttle (including HTTP 529) remains
distinct from account quota exhaustion. Incomplete or truncated streams are discarded and retried;
partial tool calls are never executed. Maximum attempts and failover behavior are unchanged.

While a turn is running, `/retry` skips the remaining provider retry wait and requests the next
attempt immediately, including any remaining server-requested delay. It does not cancel the turn,
queue a follow-up, or add retry attempts. With no pending provider retry wait, it reports that
explicitly and does nothing; requests made during active inference are not saved for later waits.
The command takes no arguments. When idle, `/retry` keeps its existing meaning: retry the last
failed, unanswered turn.

Streaming reads have bounded raw and line sizes. Reasoning markup leaked into visible text is routed to reasoning events where recognized, and tool-call markup is kept out of user-visible output. Remote model context is taken from configuration or the bundled catalog, falling back to a conservative 32768 tokens. Image inputs are resized to model limits before transmission. Provider-specific OAuth credentials are refreshed per request/retry, without exposing secrets in errors.

Logical-request recovery preserves one-shot model profiles, local hints and reasoning controls
across rejected attempts, without making them defaults for later requests. Codex WebSocket streams
that stall, like incomplete streams, select HTTP Responses streaming on the next exchange for that
session/account/model. If the agent's earlier silence watchdog cancels a Codex attempt first, the
backend switches to HTTP streaming until rebuilt. Neither path retries inline after partial
callbacks or replays completed tools. User cancellation remains interruption, not a transport-fallback
signal; history/account validation and retry bounds still apply.

For GGUF startup failures, Dial reports the selected server executable and its stderr. An
`unknown model architecture` error means that llama.cpp build cannot load the architecture:
upgrade to a compatible build, point `DIAL_LLAMA_SERVER` at it, or serve the model with a
compatible LM Studio runtime and select that provider. The latest package-manager release
can lag architecture support; changing sampling or context size cannot fix it.

Copilot's `The requested model is not supported` rejection includes the selected model and
account-policy recovery guidance. A bundled model listing is not proof of account access:
enable the model in GitHub Copilot settings (and, where applicable, the organization policy),
or select an enabled model in `/models`. Dial never accepts model terms or enables billing
policies automatically. A nearby background-compaction notice does not by itself identify
compaction as the source of a foreground provider rejection.

## Local server request fields

Local-profile requests (`[assistance.local]`) add server-specific fields only when the route is a
locally served model (Dial's MLX runtime, LM Studio, Ollama, or llama.cpp on a loopback or private
host). Hosted routes never receive them, and a field already in the request body (an explicit
model-profile value) is never overwritten. Fields are sent only where the server's own docs or source
name them:

- llama.cpp `llama-server` (Dial-launched for GGUF models, the `llama-cpp` provider, or a recognised
  endpoint): `cache_prompt` and `id_slot` (`prompt_cache`; main session on slot 0, side requests on
  the last slot and workers on the others when the server has several slots: `/props`
  `total_slots` for endpoints, the launch layout below for Dial-launched servers),
  `return_progress`, `chat_template_kwargs.enable_thinking` (when `/props` `chat_template` reads
  it), and `presence_penalty` / `repeat_penalty` for loop recovery. Source:
  [server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md).
  `cache_prompt`, `id_slot` and `return_progress` are documented under `/completion`; the README
  says `/completion`-specific features are also supported on `/v1/chat/completions`. Replies'
  `timings` and streamed `prompt_progress` supply speed, cache and progress telemetry.
- Dial-launched `llama-server` layout: Dial runs `llama-server --help` once per binary path and
  process (offline, 5-second bound, a failure counts as an old build). When it lists
  `--kv-unified`, `--kv-unified-per-slot` and `--no-cache-idle-slots`, and the memory-fitted window
  W is known, Dial launches 2 slots over one unified KV pool: `--parallel 2 --kv-unified
  --kv-unified-per-slot W --no-cache-idle-slots --ctx-size W+R`, where the side reserve R is a
  quarter of W within 4096..=16384 tokens (never more than W). Each slot may use the whole window W,
  so the main session keeps its full context; the reserve lets a side request run without evicting
  the main prefix. If W+R does not fit the comfortable memory budget, or the binary lacks a flag,
  the launch stays `--parallel 1 --ctx-size W` as before (the main window is never reduced to make
  room). Why each flag: with `--kv-unified`, llama.cpp sizes every slot's context as the whole
  `--ctx-size` (without it, `--ctx-size` is divided by `--parallel`); `--kv-unified-per-slot` caps a
  slot at W; and with idle-slot caching on (the default), unified-KV builds clear every idle slot
  when a new task starts, so a request pinned to slot 0 would re-prefill each time. When a request
  needs more pool than is free, llama.cpp purges an idle slot (the side slot first if it is idle),
  so a main request up to W always fits. Sources: `src/llama-context.cpp` (`n_ctx_seq`) and
  `tools/server/server-context.cpp` (`n_ctx_slot`, `try_clear_idle_slots`, `cache_idle_slots`) on
  llama.cpp master, checked against Homebrew `llama-server` 0.7.0 (build 11669). Options and values
  are passed as separate arguments: llama.cpp matches option names exactly and rejects
  `--opt=value`.
- Dial's MLX runtime (`mlx_lm.server`): `chat_template_kwargs.enable_thinking` (when the model's chat
  template reads it), `presence_penalty` and `repetition_penalty`. Source: `mlx_lm/server.py` in
  [mlx-lm v0.32.0](https://github.com/ml-explore/mlx-lm/blob/v0.32.0/mlx_lm/server.py).
- LM Studio (OpenAI-compatible API): `ttl` (`keep_loaded_secs`; LM Studio applies it to
  just-in-time loaded models), `presence_penalty` and `repeat_penalty`. No request-level thinking
  switch is documented, so none is sent. Source: [LM Studio docs](https://lmstudio.ai/docs/developer).
- Ollama native `/api/chat`: `keep_alive` (`keep_loaded_secs`), `think` (models with the `thinking`
  capability), `options.presence_penalty` / `options.repeat_penalty`, and `options.num_ctx` limited
  to the memory estimate when no window is pinned. At exit, a model this session loaded is unloaded
  with an empty `/api/generate` request carrying `keep_alive: 0`. Source:
  [Ollama API](https://github.com/ollama/ollama/blob/main/docs/api.md); the `thinking` capability
  name is from `types/model/capability.go`. Reply durations (`load_duration`,
  `prompt_eval_duration`, `eval_duration`, nanoseconds) supply load time and speed.

None of these fields has been measured against a live server by Dial's tests; the tests check the
request bodies built for each server.

## Tool calls on local servers

- llama.cpp `llama-server`: Dial sends native `tools` on every request and leaves `tool_choice`
  unset (auto), with no `response_format`, `json_schema` or `grammar` on the first attempt. With
  Jinja templates on (`--jinja`, the default in current builds; Dial passes it to the servers it
  launches), llama.cpp parses tool calls itself and, for `tool_choice` auto, builds a lazy grammar
  that only constrains output once the model emits its tool-call marker, so plain answers stay
  free. A schema in the request would make that grammar non-lazy and force JSON, which is why
  `json_schema` is kept for the single repair retry. Sources:
  [function calling](https://github.com/ggml-org/llama.cpp/blob/master/docs/function-calling.md),
  [server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md), and
  the lazy-grammar rule in `common/chat-auto-parser-generator.cpp` (source, not docs).
- A server with Jinja off answers HTTP 500 `tools param requires --jinja flag`
  (`tools/server/server-common.cpp`). Dial treats that as fatal, not transient, and the error says
  to restart with `--jinja`. `/props` does not report whether Jinja is on, so it cannot be detected
  before a request.
- When `/props` `chat_template_caps` (see `common/jinja/caps.h`) reports `supports_tools` or
  `supports_tool_calls` false, Dial shows one notice per conversation suggesting
  `--chat-template-file` with a tool-use template. It reads only `/props` answers already fetched by
  local-server detection; this check sends no extra request. Older builds without
  `chat_template_caps` get no notice.
- `parallel_tool_calls` is sent only when a profile declares it. llama.cpp keeps it off by default
  and supports it only for some templates. The function-calling docs also warn that extreme KV-cache
  quantization (`kv_bits = 4`, q4_0) can substantially degrade tool calling.
- LM Studio and Ollama document native `tools` but no request field for grammar-constrained tool
  calls, so they get no first-attempt constraint; their repair retry uses `response_format`
  (LM Studio) or `format` (Ollama). LM Studio uses "native" tool use for models whose template
  supports it and a "default" prompt-based format otherwise
  ([tool use](https://lmstudio.ai/docs/developer/openai-compat/tools)); `GET /api/v1/models`
  reports `capabilities.trained_for_tool_use`, which Dial does not read yet.

## Subscription-provider transports

The extended dispatcher uses Dial's blocking protocol clients with bounded SSE parsing, cancellation,
retry/error classification, tool calls, and stream callbacks.

- **Copilot** uses Chat Completions at `/chat/completions` and sends the Copilot chat identity/version
  headers. The OAuth grant supplies the bearer token and can override the API base.
- **GitLab Duo** obtains a direct-access token from GitLab's `direct_access` endpoint for each
  request, then streams an Anthropic Messages request through the returned gateway token and headers.
- **Factory Droid** routes its explicit model map to Anthropic Messages, OpenAI Responses, OpenAI Chat
  Completions, or native Gemini SSE. It adds the WorkOS bearer, account/routing headers, stable
  conversation `x-session-id`, one `x-assistant-message-id` reused across retries, and Droid system
  identity; the OAuth access grant selects the regional API host and inference region.
- **Cursor** (`/providers` sign-in) runs Cursor's agent protocol over plain HTTP/1.1: a `RunSSE` stream of Connect frames carries the reply, and client messages (run request, tool results, heartbeats) go up as `BidiAppend` unary posts from a dedicated uplink thread, one post per message in order. Protobuf and Connect framing are shared with Devin (`crates/dial-llm/src/llm/connect.rs`), so no HTTP/2 or Node bridge is involved. Prior history is sent as JSON root messages because Dial does not keep a native Cursor transcript. Tool calls are handed back to Dial: each Cursor MCP call is returned as a Dial tool call and answered with a handoff notice, and Cursor's own shell/file/search tools are not served. Cursor-hosted approvals, questions, and plans are rejected, not approved. Image input is not supported yet and fails with a clear error. This path has been covered by local fake-server tests only; it has not been run against a live Cursor account, so treat it as experimental.

Factory routing metadata uses an explicit model wire/upstream map. Unknown models fall back to
OpenAI Chat Completions and Fireworks. Since the provider currently has no persisted upstream-lock
state, requests report `registry_default` routing.

## Remote idle limits and prompt-cache keys

- `remote_model_idle_timeout_secs` defaults to 300 seconds for remote routes (ChatGPT, Anthropic, and OpenAI-compatible hosts that are not localhost or LAN). It bounds the wait for the first event and silence between events. Only actual stream progress resets it; keep-alives do not.
- A stall maps to an incomplete stream, which is transient. It retries under `retry.max_attempts`, and the partial reply and its tool calls are discarded before the retry.
- Remote model connections send TCP keepalive probes after 30 seconds of silence (every 10 seconds, 3 probes). A path that dies while the model is silent (sleep/wake, a network or VPN switch, an expired NAT mapping) fails within about a minute and retries on a fresh connection instead of waiting out the idle limit. A live server answers probes however long the model takes, so slow replies keep the full limit. Retry notices show how long the failed attempt ran.
- Local model servers (localhost or LAN) keep an unlimited first-event wait. `worker_timeout_secs` (default `0`) still governs local workers. `0` on `remote_model_idle_timeout_secs` is an explicit opt-out.
- The agent's silence-recovery watchdog does not interrupt local model loading or prompt processing before generation begins. It still bounds silent generation and remote requests. Loading/prefill may remain silent when an engine cannot report measured progress; user cancellation and configured transport/startup limits remain active.
- The silence watchdog requires advancing telemetry (phase or token-count high-water marks), nonempty text/reasoning, or tool argument progress. Repeated status/rate reports and notices do not renew its deadline. Once generation starts, stale loading/prefill reports cannot disarm it; a provider retry resets the attempt's watchdog state.
- A failed or cancelled request to a Dial-managed server stops that server and its descendants before reuse. The next request starts a fresh process rather than queueing behind abandoned inference. External servers are never killed. Non-cancellation failures include a bounded stderr tail from the failed request, preserving the original error classification.
- ChatGPT/Codex Responses requests (HTTP and websocket) send `prompt_cache_key` whenever the session has one, unless the profile sets `prompt_cache = false`. The key is derived from the session identity, account, and history generation, so it is stable within a session. API-key Responses requests keep the declared `capabilities.prompt_cache` gate and `prompt_cache` breakpoint policy.
- The Codex backend routes prompt-cache affinity by the `session_id`/`conversation_id` headers, not by `prompt_cache_key`. The websocket handshake and HTTP SSE requests both send them (the dial session id), so a websocket-to-SSE fallback keeps its cache. A cached websocket idle for more than 30 seconds is replaced before use, because the backend evicts idle sockets without a close frame. A reused socket that fails before any event (a broken-pipe write, or a close with nothing received) is replaced inline and the request replayed in full. Only a fresh socket's failure moves that session/account/model to SSE for the process.
- Codex WebSocket setup uses one elapsed timeout budget across address attempts and TLS/HTTP upgrade, rather than restarting the budget for each address or read. Handshake network I/O checks cancellation at intervals of at most 100 ms; cancellation propagates without selecting SSE. System DNS, native certificate loading, and an in-flight blocking TCP connect cannot be preempted. Their elapsed time counts against the budget, but DNS/certificate loading can exceed it; with a zero timeout, TCP connect retains the OS timeout.
- Cached Codex WebSockets bind cancellation and no-progress limits to each active request, including reads inside a partial frame. Ping/pong traffic does not renew model-progress deadlines. Request controls are detached before returning a connection to the cache, so an earlier request's cancellation cannot poison reuse.

### Anthropic cache-control refusal and cache warming

- Anthropic native requests mark cache breakpoints with `cache_control`. If the endpoint refuses that field (HTTP 400 whose body names `cache_control` and a rejection), Dial retries the request once with the next narrower form: first without the 1-hour TTL, then with no markers. Unrelated 400s are not treated as refusals. A refusal is remembered per endpoint (URL without query) and model for the process, so later requests start at the narrower form.
- `cache.warm` (default `off`) can keep the settled prompt prefix warm after a turn ends, so a follow-up within the cache lifetime reads the cache instead of rewriting it. Each replay is a real billed request with `max_tokens = 1`; it costs the cache-read price on a hit and is recorded under the `cache_warm` purpose in usage and cost.
  - `off`: no replays.
  - `streaming`: replays only while an agent run is streaming a reply, for up to 30 minutes from the last settled prefix.
  - `idle`: also replays between runs, for up to 60 minutes from the last settled prefix.
- A replay is armed only when the economics favor it: the expected savings over the next replay must exceed $0.001 at catalog prices. Without catalog pricing for the model, warming does not start. The successful native request supplies its effective cache lifetime, including cache-control refusal fallback: five-minute prefixes refresh after four minutes; one-hour prefixes after 59 minutes. Unknown or disabled retention disarms the previous prefix rather than guessing.
- Warming stops when the mode changes to `off`, the model changes, the window ends, a replay wrote the cache instead of reading it, or the replay backend is unavailable. Accepted tool-call and text-only replies both replace the armed prefix with the request that produced them, not the newly generated reply. The reason is shown as a notice at the start of the next turn. Hermetic evaluation runs never replay.
- Warming applies to native Anthropic routes only. Other providers and local servers have no equivalent cache control and never replay.

## Stable prompts and immutable media

System assembly separates frozen guidance, stable project/tool context, and dynamic admitted memory. Transient guidance stays at the conversation tail; history is never sorted. Custom templates are treated as dynamic because their interpolation may include changing data. Parent and worker assembly use the same band ordering. Bookkeeping fields are removed before provider serialization and training-template validation.

New image-bearing history owns verified pixels before inference. Persistent sessions store content-addressed artifact references with source-path provenance; no-session conversations retain bytes in memory. Deleting or overwriting the original file does not alter the historical image. Legacy path-only references still verify their recorded digest, fail safely when stale, and produce bounded per-conversation notices. The harness records capture failures instead of substituting newer pixels. Ordinary artifact GC recognizes the retained image references.

`Backend::wire_observation()` reports hashes and byte counts after provider transformations and image encoding, at HTTP serialization or the outgoing WebSocket request. Signed requests are measured without reserialization. Parent journals retain `provider_wire` observations. Fingerprints retain no prompt text, media, credentials or URLs; common prompt bytes are not token counts or proof of provider cache hits. Actual provider usage remains the authority for cache accounting. HTTP retries are transport attempts. A WebSocket continuation (`chained`) hashes the transmitted delta for `payload_*` but compares the full logical prompt, so an append still reads as `invalidation = none`; a prompt that held across an HTTP/WebSocket switch reads as `transport_changed`. Each accepted request's `transport`, `invalidation`, `common_prompt_bytes` and `prompt_bytes` are also stored in the stats ledger's `requests` table. A low `cache_read` with `invalidation` `none` or `transport_changed` is a provider-side miss (routing or eviction); `prompt_changed` means dial changed the prompt.

## Runtime usage attribution

The runtime usage ledger keeps independent main/worker and model breakdowns, plus `by_purpose`:
`agent` (ordinary main or delegated inference), `retrieval` (semantic source ranking),
`compaction` (foreground/background summarization, including native and remote routes), `memory`
(recall, capture and maintenance inference), `repair` (constrained or coach repair), `judgment`
(model-backed judgments), and `auxiliary` (side questions/rule generation). Purpose follows the
operation, not the configured model role: the same `small` model can rank source or compact history.
Each breakdown partitions `all`; adding breakdowns together double-counts usage. Retries, failed
requests, cache counts and observed prices retain the request's original purpose and turn, including
late worker results. Purpose attribution does not alter shared worker budgets or permissions.

Missing provider usage and missing prices remain unknown, not zero-cost success. The ledger covers
the current runtime, not restored historical usage or an invoice. Cache/reasoning counters have their
own coverage fields and must not be interpreted as fully reported when those fields are absent.

Backends created during a fallback or cooldown return join the existing turn ledger before their
first request. Owner-rejected auxiliary replies and full-generation guidance/edit-preview retries
record discarded responses under the purpose that requested them; discarding does not erase their
physical tokens or cost. Partial batch interception is not a discarded full generation.

## Secret masking

`[secrets] enabled = true` (default `false`) keeps secrets out of what a hosted model sees. Each
request's messages (user text, tool results, replayed assistant text and tool-call arguments,
including strings inside nested JSON) are rewritten before wire serialization; the static system
prompt and tool definitions are not. Local (Dial-launched MLX/llama) and on-device routes never leave
the machine and are not masked.

- **Sources.** Credential-shaped tokens (the same patterns as transcript redaction, in
  `dial_core::credentials`: vendor API keys, JWTs, bearer tokens, PEM private keys, URL passwords);
  values of environment variables whose names contain `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`,
  `CREDENTIAL` or `AUTH` (`secrets.env`, default `true`, read at startup); and literals in
  `secrets.values` (settings.toml only). Values shorter than 8 bytes are never masked, and
  `secrets.values` rejects them.
- **Placeholder.** `$$SECRET_<12 hex>$$`, the first 6 bytes of HMAC-SHA256 of the secret under a random
  per-install key stored owner-only at `<dial home>/secret-mask.key`. The same secret always maps to
  the same placeholder, so prompt-cache prefixes stay byte-stable across turns and restarts, and the
  placeholder cannot be dictionary-tested without the key. Overlapping or duplicate secrets collapse
  to the longest span.
- **Restoration.** Placeholders the request carried are restored in the reply before anything else
  sees it: streamed text (chunk boundaries are handled), reasoning, tool-call arguments (nested JSON
  strings are restored structurally, so quotes in a secret stay valid) and native replay content. Tools
  therefore run with the real value, and the session journal stores real values. A placeholder the
  request did not contain is left untouched.
- **Failure.** If the key cannot be read or created, backend construction fails; Dial never falls back
  to sending raw text. Provider-owned opaque fields (signatures, encrypted reasoning) are not rewritten.
- Masking is best-effort pattern matching: a secret with no recognizable shape that is not in the
  environment or `secrets.values` is sent as written. Files the model reads are masked only on the way
  out; the model cannot tell you the real value, but it can still act on a placeholder you give a tool.

## Retries and image input

Devin also requires its successful completion trailer and complete framing before releasing text or tool calls. Connect responses from Devin and Cursor enforce expanded-byte limits as well as wire-byte limits. Endpoint API-key credentials are resolved again for each retry attempt, so replacement or removal during backoff takes effect before the next dispatch.

**Rejected reasoning parameter.** If a request fails with HTTP 400 and the body names `reasoning_effort`, `reasoning.effort`, or an unsupported parameter involving `reasoning`, Dial retries once without the reasoning parameter (including an explicit thinking-off). A retry that succeeds is remembered for that endpoint URL and model for the life of the process, so later requests omit the parameter up front. Nothing is written to configuration. An unrelated 400, or a request that sent no reasoning parameter, is not retried.

**API-key pools.** A provider can name several keys: `api_key_env` plus `api_key_envs = ["KEY_B", "KEY_C"]` on `[providers.<id>]`. When the active key is rate limited or out of quota, that key is blocked until the Retry-After or quota reset time (60 seconds when the provider gives none, at most one hour) and the same request is retried at once with the next key whose variable is set. This consumes one attempt. The key that works keeps serving later requests, and blocks are shared by every session in the process. If no other key is available the ordinary backoff applies. Pool state is process-local and never persisted; only variable names are tracked, so key values are never logged. Each variable must belong to the provider's origin, as for `api_key_env`. A pool needs a key-authenticated endpoint; OAuth and subscription routes do not use one.

Transient provider errors use fixed retry waits with server retry hints and visible notices (up to `[retry].max_attempts`
attempts, default 10; ordinary waits are 300 seconds, bounded by `[retry].max_delay_secs`, default 300, and longer server hints stop the request). This includes macOS socket `Invalid argument (os error 22)` and
TLS `BadRecordMac` alerts before a response arrives. Each retry opens a fresh connection; TLS certificate
verification remains enabled. Disconnected or prematurely closed ChatGPT, compatible chat, and
Anthropic streams also retry within that bound, discarding the failed attempt's partial reply and tool
calls. Completed tools from earlier requests are retained. Configured native Anthropic idle timeouts
and transient streamed errors also retry; malformed events and authentication failures stop immediately. Exhausted retries pause the
turn with a reason and instructions to continue. `@image.png` and image `read` calls attach actual
PNG/JPEG/GIF/WebP pixels, downscaled to fit 5 MiB when larger (sources up to 64 MiB). To supply an image outside the project, explicitly reference its
absolute path or home path, such as `@"/Users/me/Desktop/Screenshot 1.png"` or `@~/Pictures/example.jpg`.
Pasted standalone or quoted image paths also attach automatically without `@`. The model can reread
exactly those supplied images; other external tool reads stay confined. Sessions store file references and content hashes, not base64;
changed or missing attachments are skipped with a notice rather than silently replaced on replay.
Browser screenshots attach rendered pixels automatically, and Dial's visual-work guidance calls for
proactive inspection when looking would help. Visual tasks receive an initial inspection reminder;
edits to styles/templates/components trigger a bounded completion checkpoint when no fresh rendered
pixels have reached the model. Earlier screenshots become stale after subsequent edits. If visual
inspection remains unavailable, the final result explicitly reports that it was not completed.
Known non-vision models skip images with a notice to both the user and model. Downloaded local models
with a vision encoder in `config.json` use MLX-VLM and receive the image pixels; text-only models use
MLX-LM. `dial setup` installs both runners in Dial's private runtime. The vision runner retains at most
one image-feature cache and uses process-local prefix caching: at most 256 blocks and one exact
checkpoint, with a 1 GiB cache-admission budget and 256 MiB free-memory reserve. Disk caching is
disabled, so caches cannot cross server/profile/steering restarts. The runner checks cache compatibility
before use and falls back to uncached inference when it cannot validate it. These are cache admission
bounds, not a cap on the model's total or peak memory. Vision prefill reports measured token counts
where the pinned runner exposes them, otherwise remaining in the coarse processing phase with a
compatibility warning. External endpoints with unknown capabilities are attempted and may be rejected
by the provider; use a vision-capable model for visual work.

## Model profiles, prompt caching and request headers

Profiles declare capabilities for an **exact** provider/model; reasoning, sampling and vision behavior come only from that profile, never from model-name heuristics. Context windows resolve from an explicit profile or operator cap, provider catalog metadata, documented Anthropic model limits, or otherwise a local/cloud default chosen by provider host. Select with `--profile name` or `/profile name`; `/reasoning <level>`
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
Native Anthropic Messages requests automatically cache the stable tool/system prefix and rolling
conversation history (at most four breakpoints). Request-local task packets and reliability guidance
stay outside the reusable prefix; signed thinking blocks are never cache anchors. Tool schemas are
ordered deterministically, and changing harness guidance no longer rewrites tool descriptions.

In an Anthropic model profile, `prompt_cache=false` opts out (use this for a compatible endpoint
that does not support caching). Omission enables automatic caching; existing `prompt_cache=true`
still requires `capabilities.prompt_cache=true`. `cache_retention="auto"` (default) uses one-hour
retention for OAuth requests to the official Anthropic API, five-minute retention otherwise.
Set `cache_retention="short"` to force five minutes or `"long"` to request one hour from an endpoint
that supports it. Long-lived cache writes can cost more for API keys; retention does not reduce
generated reasoning/output tokens, and Claude subscription quota savings must be measured separately.

Endpoint/model selections can declare `protocol = "chat-completions"`, `"responses"`, `"anthropic"`, `"gemini"` or `"ollama"`, or another of the recognized wire protocols (`bedrock`, `vertex`, `vertex-anthropic`, `azure-responses`, `copilot`, `cloud-code-assist`, `antigravity`, `gitlab-duo`, `gitlab-duo-agent`, `factory-droid`, `devin-agent`). No provider-name guess converts a custom endpoint into a different wire
format. Native encrypted/signed history stays scoped to its real provider/model/endpoint/account;
changing that identity requires a clean or safely reconstructed portable view.

Custom request headers are ordered rows, inherited when omitted or `"inherit"`, and explicitly
cleared by `[]`. The model-route settings editor exposes them with F8. Literal values are explicitly
nonsecret; secret values must reference environment-variable names:

```toml
[endpoint]
headers = [
  { name = "X-Client", value = { literal = "dial" } },
  { name = "X-Proxy-Token", value = { env = "DIAL_PROXY_TOKEN" } },
]
```

At most 32 headers are allowed. Authentication, account/tenant routing and transport-owned names
cannot be overridden. Missing/invalid environment values fail closed without echoing secrets.
An explicitly empty credential environment variable clears a lower-precedence saved key.
ChatGPT account IDs and role accounts are pinned by the owning session and constrained by private
global account policy; project settings cannot silently rotate accounts or widen that policy.

Providers may automatically reuse prompt prefixes. Responses sends a session/account/route-scoped
cache key only when the exact profile declares support and does not opt out. Cached read/write usage
is reported separately; unknown counts/prices remain unknown. Output-limit recovery honors its
increased request budget.
Completion is the model's call: Dial does not add a review round after edits or shell commands.
When a turn ends, changed files are listed for the user. Approval, todo, and configured focused
verification gates remain.
