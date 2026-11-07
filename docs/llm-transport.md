# LLM transport behavior

HTTP construction policy is explicit in the existing LLM owner: redirect handling, environment
versus direct proxy routing, whole-request deadlines versus streaming patience, and cancellation.
Construction installs TLS before use. Provider clients refuse redirects; unauthenticated model
catalog discovery retains its explicit ten-redirect limit. Guarded web DNS/resolvers, provider
authentication and WebSockets retain their specialized owners.

Streaming parsers bound bytes and inspect patience between reads. They cannot interrupt an
arbitrary blocking `Read`; network callers must supply the cancellation-aware reader, which polls
below TLS and enforces the shared patience clock. A parser byte cap alone is not a deadline.

The shared LLM layer provides bounded streaming, provider-aware error classification, retries, and request budgeting. Context-overflow classifications are kept attached to errors before any header redaction so compaction remains reliable. HTTP 413 is classified as a payload failure unless the body identifies a token/context limit; it must not trigger token compaction merely because its status is 413. oMLX's "prefill memory guard rejected this prompt" refusal (prefill would exceed the server's memory ceiling) is classified as context overflow, as an HTTP body or a stream error, so the turn compacts and retries a smaller prompt instead of failing.

`[retry]` settings bound attempts and delays. Unhinted retries use bounded exponential backoff
starting at no more than two seconds, with up to 25% positive jitter; the doubling stops at 60
seconds per attempt, and every wait is capped by `retry.max_delay_secs`. Explicit `retry-after`, `retry-after-ms`, and `x-ratelimit-reset[-ms]`
delays replace the automatic interval instead of being inflated to five minutes. A hint beyond
the configured cap stops the request rather than silently retrying earlier than requested.
Window-reset headers remain quota information, not mandatory waits. A throttle (including HTTP
529) remains distinct from account quota exhaustion. Configured failover retains its shorter
wait policy. Incomplete/truncated streams are discarded and retried within the same attempt
bounds; partial tool calls are never executed. Claude usage polling shares this retry policy.
A response carrying `x-should-retry: false` is not resent: the request fails at once. The failure
keeps its class, so API-key and OAuth account rotation and a configured fallback model still apply.

Native replay is owner-scoped. Responses items replay only to the account/endpoint/credential (and,
for compaction state, the model) that produced them, Anthropic signed blocks only to the host that
produced them, and Gemini function-call parts only with a verified signature for the endpoint and
model. Anything else is sent in portable form: text and tool calls, with Gemini tool turns from
another route replayed as plain text so no signature is invented. A tool call cut off by the
output limit is dropped and reported as `length` on every transport, so the turn raises its output
budget instead of failing. Chat-completions finish reasons such as `max_tokens`, `model_length`
and `model_context_window_exceeded` map to `length`; `error` and `network_error` are incomplete
streams and retried. A streamed Responses `context_length_exceeded` is a context overflow, which
compacts instead of resending the same prompt.

While a turn is running, `/retry` skips the remaining provider retry wait and requests the next
attempt immediately, including any remaining server-requested delay. It does not cancel the turn,
queue a follow-up, or add retry attempts. With no pending provider retry wait, it reports that
explicitly and does nothing; requests made during active inference are not saved for later waits.
The command takes no arguments. When idle, `/retry` keeps its existing meaning: retry the last
failed, unanswered turn.

Streaming reads have bounded raw and line sizes. Reasoning markup leaked into visible text is routed to reasoning events where recognized, and tool-call markup is kept out of user-visible output. Remote model context is taken from configuration or the bundled catalog, falling back to a conservative 32768 tokens. Image inputs are resized to model limits before transmission. Provider-specific OAuth credentials are refreshed per request/retry, without exposing secrets in errors.

Native first-party Anthropic client-tool definitions set `eager_input_streaming: true` so
large code arguments can arrive incrementally rather than waiting for server-side JSON buffering.
Nonstreaming and cache-warm requests keep the same definitions; proxies keep their existing wire
format. Dial still accumulates and validates complete arguments before admitting a tool call;
partial or truncated JSON never executes. Client-observed silent gaps do not establish provider
inactivity, and fine-grained delivery alone does not reduce generated tokens or prove faster work.

Text/reasoning recovery of tool calls is limited to local/private endpoints and names offered
in that request. A fenced JSON example embedded in prose is not executable. Recovered calls
carry runtime provenance into the parent and delegated approval paths: they require actual
one-shot confirmation, cannot consume saved grants or speculative/parallel/batch shortcuts,
and never save a reusable permission. Denials and cancellation still win; read-only approval
mode conservatively refuses recovered calls, including recovered reads. Structured tool calls
retain their ordinary approval policy.

Logical-request recovery preserves one-shot model profiles, local hints and reasoning controls
across rejected attempts, without making them defaults for later requests. Codex WebSocket streams
that stall, like incomplete streams, select HTTP Responses streaming on the next exchange for that
session/account/model. Remote stall recovery stays inside the transport's bounded retry policy;
the agent does not race it with a second silence-cancellation clock. Neither path replays
completed tools. User cancellation remains interruption, not a transport-fallback signal;
history/account validation and retry bounds still apply. Codex and GitLab Duo Agent
WebSockets honour the same `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` and `NO_PROXY` settings as HTTP
requests through an `http://` CONNECT proxy (other proxy kinds fail with a clear error; Codex then
uses HTTP streaming); loopback and LAN hosts are reached directly.

Model requests to a remote host reuse a warm connection instead of opening a new TCP and TLS
connection for every request. A connection returns to a process-wide pool only after its response
was read to the end with nothing left over and the request was not interrupted. The SSE reader reads
the few bytes that follow a stream's final event, for at most 250 ms. The next request to the same
origin borrows the connection with its own cancellation and silence limits. A borrowed connection
is probed again before use, and an idle one is dropped after 30 seconds. Connections to loopback
and LAN hosts, connections through a proxy, and agents with their own DNS resolver are never shared.

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

Dial's local chat-completions requests (including LM Studio and other OpenAI-compatible endpoints
on loopback or private hosts) use a single initial system message. Leading system/developer
instructions are merged into that prefix; later and transient instructions are sent as user
messages at their original position. This avoids strict chat-template errors such as
`System message must be at the beginning` without moving guidance ahead of earlier history.
The projection changes only the outgoing request, not saved conversation history. Hosted
chat-completions routes and other API protocols keep their existing message handling.

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
- Dial's MLX vision runtime (`mlx_vlm.server`, selected from the checkpoint's vision metadata):
  top-level `enable_thinking`. Dial translates a boolean profile
  `local_sampling.chat_template_kwargs.enable_thinking` to that wire field; MLX-LM keeps the nested
  field. Explicit profile reasoning controls and an existing native request field retain priority.
  An unset choice stays unset, so the server's default still applies. This mapping does not force
  reasoning or create prose the model did not emit. Source: the installed mlx-vlm 0.7.6
  `server/request_normalization.py` and `server/generation.py`.
- LM Studio (OpenAI-compatible API): `ttl` (`keep_loaded_secs`; LM Studio applies it to
  just-in-time loaded models), `presence_penalty` and `repeat_penalty`. No request-level thinking
  switch is documented, so none is sent. Source: [LM Studio docs](https://lmstudio.ai/docs/developer).
- Ollama native `/api/chat`: `keep_alive` (`keep_loaded_secs`, opt-in), `think` (models with the
  `thinking` capability), and opt-in `options.presence_penalty` / `options.repeat_penalty`.
  Automatic context uses the host's loaded window without sending `options.num_ctx`; explicit
  Dial context limits still send it. Memory estimates are advisory only. Model unloading at exit
  applies only when Dial was explicitly asked to extend residency and loaded the model itself.
  Source:
  [Ollama API](https://github.com/ollama/ollama/blob/main/docs/api.md); the `thinking` capability
  name is from `types/model/capability.go`. Reply durations (`load_duration`,
  `prompt_eval_duration`, `eval_duration`, nanoseconds) supply load time and speed.

None of these fields has been measured against a live server by Dial's tests; the tests check the
request bodies built for each server.

For ordinary local-host agent requests, unset generation controls inherit the serving API's defaults
by omission, not by copying guessed values. This includes temperature, output allowance, thinking,
residency and cache/penalty overrides. Explicit Dial settings still win; internal bounded requests
can retain their own limits. A host's API defaults need not be identical to every GUI chat preset.
For a field Dial omits, the status bar (`temp … · out …`) and the `/policy` local request settings
show the value the server reports applying to it, marked `(provider)`: llama.cpp's `/props`
`default_generation_settings` and Ollama's `/api/show` Modelfile `parameters` (temperature, top-p,
top-k, min-p, output allowance; a negative count shows as `unlimited`). They are read once per
session. LM Studio's API reports no sampling defaults and an Ollama model without a `PARAMETER`
line reports none, so those fields show only `provider`; Dial does not substitute a server's
undocumented built-in value.

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

- Eligible direct HTTP connections are shared across per-request agents after the previous response is fully consumed. The pool keeps at most four idle connections per origin and rejects connections idle for 30 seconds or more when borrowing them. Each request rebinds cancellation and silence controls. Loopback/LAN targets, proxies, and caller-supplied guarded resolvers do not share this pool. This is reuse after traffic, not a hosted-model startup ping or preconnect.
- `remote_model_idle_timeout_secs` defaults to 60 seconds for remote routes (ChatGPT, Anthropic, and OpenAI-compatible hosts that are not localhost or LAN). It bounds the wait for the first event and silence between events. Only actual stream progress resets it; keep-alives do not, with one exception: while a native Anthropic tool call or `thinking` block is open, `ping` events reset it for up to 10 minutes after the last real progress. Anthropic may buffer a large tool call's arguments and send only pings (every 30 seconds) until the call is complete, so a long `write` must not be cut off and replayed identically. A thinking block whose text is omitted streams the same way until its signature arrives.
- Requests to `api.anthropic.com` ask for `thinking.display: "summarized"` on models whose catalog record says thinking text is omitted by default (`summarized-thinking`), when adaptive thinking is on and no display is set. Long thinking then streams readable summaries instead of empty blocks. Thinking is billed the same either way; proxies and other hosts are left unchanged.
- A stall maps to an incomplete stream, which is transient. It retries under `retry.max_attempts`, and the partial reply and its tool calls are discarded before the retry.
- Sockets opened by the cancellable HTTP transport and WebSocket TCP connector enable TCP keepalive probes after 30 seconds of silence (every 10 seconds, 3 probes). For WebSockets this includes the socket underneath an HTTP CONNECT tunnel. A path that dies while the model is silent (sleep/wake, a network or VPN switch, an expired NAT mapping) can fail sooner than the model idle limit and retry on a fresh connection. A live server answers probes however long the model takes, so slow replies keep the full limit. These are kernel probes, not billed requests or prompt-cache refreshes, and they do not renew model-progress deadlines. Retry notices show how long the failed attempt ran.
- Local model servers (localhost or LAN) keep an unlimited first-event wait. `worker_timeout_secs` (default `0`) still governs local workers. `0` on `remote_model_idle_timeout_secs` is an explicit opt-out.
- The agent's `request_recovery` silence watchdog is local-generation-only and applies to parent and worker requests. It does not interrupt model loading or prompt processing before generation begins. Once generation stalls, `silence_secs` cancels only that inference and `retry_once` permits one same-context retry without replaying completed tools. Remote requests use the transport's `remote_model_idle_timeout_secs` and `[retry]` policy instead, avoiding competing cancellation and retry deadlines. Loading/prefill may remain silent when an engine cannot report measured progress; user cancellation and configured transport/startup limits remain active.
- OpenAI-compatible usage reports with `completion_tokens: 0` are processing telemetry, not evidence that generation has begun. They cannot arm the local generation-silence watchdog before output; positive output counts still report generation. Token accounting is unchanged.
- The silence watchdog requires advancing telemetry (phase or token-count high-water marks), nonempty text/reasoning, or tool argument progress. Repeated status/rate reports and notices do not renew its deadline. Once generation starts, stale loading/prefill reports cannot disarm it; a provider retry resets the attempt's watchdog state.
- A failed or cancelled request to a Dial-managed server stops that server and its descendants before reuse. The next request starts a fresh process rather than queueing behind abandoned inference. External servers are never killed. Non-cancellation failures include a bounded stderr tail from the failed request, preserving the original error classification.
- Local startup health checks and embedding warm-up observe cancellation, including when the startup timeout is unlimited. The supervised server's one-shot generation/embedding warm-up is distinct from optional `preparation.local_prefix` and paid hosted prompt-cache refresh.
- ChatGPT/Codex Responses requests (HTTP and websocket) send `prompt_cache_key` whenever the session has one, unless the profile sets `prompt_cache = false`. The key is derived from the session identity, account, and history generation, so it is stable within a session. API-key Responses requests to `api.openai.com` also send it unless `prompt_cache = false`, since it is only a cache-routing hint there. Other API-key Responses hosts keep the declared `capabilities.prompt_cache` gate, and all API-key routes keep the `prompt_cache` breakpoint policy.
- The Codex backend routes prompt-cache affinity by the `session_id`/`conversation_id` headers, not by `prompt_cache_key`. The websocket handshake and HTTP SSE requests both send them (the dial session id), so a websocket-to-SSE fallback keeps its cache. A cached websocket idle for more than 30 seconds is replaced before use, because the backend evicts idle sockets without a close frame. A reused socket that fails before any event (a broken-pipe write, or a close with nothing received) is replaced inline and the request replayed in full. Only a fresh socket's failure moves that session/account/model to SSE for the process.
- Codex WebSocket setup uses one elapsed timeout budget across address attempts and TLS/HTTP upgrade, rather than restarting the budget for each address or read. Handshake network I/O checks cancellation at intervals of at most 100 ms; cancellation propagates without selecting SSE. System DNS, native certificate loading, and an in-flight blocking TCP connect cannot be preempted. Their elapsed time counts against the budget, but DNS/certificate loading can exceed it; with a zero timeout, TCP connect retains the OS timeout.
- Cached Codex WebSockets bind cancellation and no-progress limits to each active request, including reads inside a partial frame. Ping/pong traffic does not renew model-progress deadlines. Request controls are detached before returning a connection to the cache, so an earlier request's cancellation cannot poison reuse.

### Anthropic cache-control refusal and cache warming

- Anthropic native requests mark cache breakpoints with `cache_control`. If the endpoint refuses that field (HTTP 400 whose body names `cache_control` and a rejection), Dial retries the request once with the next narrower form: first without the 1-hour TTL, then with no markers. Unrelated 400s are not treated as refusals. A refusal is remembered per endpoint (URL without query) and model for the process, so later requests start at the narrower form.
- `cache.warm` (default `off`) can keep the settled prompt prefix warm after a turn ends, so a follow-up within the cache lifetime reads the cache instead of rewriting it. Each replay is a real billed request recorded under the `cache_warm` purpose in usage and cost; it costs the cache-read price on a hit. Native Anthropic replays use the documented pre-warm form: an unstreamed request with `max_tokens: 0`, built exactly like the live request (same `thinking`, effort and tools), which generates no output. Budget thinking (`thinking.type: "enabled"`) is rejected with `max_tokens: 0`, so a replay of a budget-thinking request omits it and refreshes only the tools and system entries. A replay sent without usage accounting falls back to `max_tokens = 1`.
  - `off`: no replays.
  - `streaming`: replays only while an agent run is streaming a reply, for up to 30 minutes from the last settled prefix.
  - `idle`: also replays between runs, for up to 60 minutes from the last settled prefix.
- A replay is armed only when the economics favor it: the expected savings over the next replay must exceed $0.001 at catalog prices. All replays after one real turn buy the same single avoided miss, so warming also stops once their combined cost would exceed that expected saving. Without catalog pricing for the model, warming does not start. The successful native request supplies its effective cache lifetime, including cache-control refusal fallback: five-minute prefixes refresh after four minutes; one-hour prefixes after 59 minutes. Unknown or disabled retention disarms the previous prefix rather than guessing.
- Warming stops when the mode changes to `off`, the active route/configuration changes, the window ends, a replay wrote the cache instead of reading it, or the replay backend is unavailable. Replays are bound to the configuration of the route that actually served the request, including session switches and fallbacks, rather than the initial default model. Accepted tool-call and text-only replies both replace the armed prefix with the request that produced them, not the newly generated reply. Worker stop reports appear as notices at the start of the next turn; route changes discard the old worker. Hermetic evaluation runs never replay.
- Warming applies to native Anthropic routes only. Other providers and local servers have no equivalent cache control and never replay.
- Pending invalidations and run-end events are applied before a due replay. If the bounded event queue overflows, warming stops rather than silently losing a stop event; a later request can start a fresh worker. Dropping the session's warmer prevents new replays even if a run guard remains alive. A request already in flight may finish within its bounded replay timeout.

## Stable prompts and immutable media

System assembly separates frozen guidance, stable project/tool context, and dynamic admitted memory. Transient guidance stays at the conversation tail; history is never sorted. Custom templates are treated as dynamic because their interpolation may include changing data. Parent and worker assembly use the same band ordering. Bookkeeping fields are removed before provider serialization and training-template validation.

New image-bearing history owns verified pixels before inference. Persistent sessions store content-addressed artifact references with source-path provenance; no-session conversations retain bytes in memory. Deleting or overwriting the original file does not alter the historical image. Legacy path-only references still verify their recorded digest, fail safely when stale, and produce bounded per-conversation notices. The harness records capture failures instead of substituting newer pixels. Ordinary artifact GC recognizes the retained image references.

`Backend::wire_observation()` reports hashes and byte counts after provider transformations and image encoding, at HTTP serialization or the outgoing WebSocket request. Signed requests are measured without reserialization. Parent journals retain `provider_wire` observations. Fingerprints retain no prompt text, media, credentials or URLs; common prompt bytes are not token counts or proof of provider cache hits. Actual provider usage remains the authority for cache accounting. HTTP retries are transport attempts. A WebSocket continuation (`chained`) hashes the transmitted delta for `payload_*` but compares the full logical prompt, so an append still reads as `invalidation = none`; a prompt that held across an HTTP/WebSocket switch reads as `transport_changed`. Each accepted request's `transport`, `invalidation`, `common_prompt_bytes` and `prompt_bytes` are also stored in the stats ledger's `requests` table. A low `cache_read` with `invalidation` `none` or `transport_changed` is a provider-side miss (routing or eviction); `prompt_changed` means dial changed the prompt, and `changed_part` names the request section where it first differed (`system`, `tools`, `messages`, or the provider's equivalent such as `instructions` or `input`). A change in `system` or `tools` invalidates the whole cached prefix; a change in `messages` keeps the part before it.

The same rows also record `first_output_ms` and `effort`. `first_output_ms` is the wall time from the final attempt's dispatch to its first streamed text, reasoning or tool-argument output; retries and backoff before it are excluded. It is NULL for replies that were not streamed. `effort` is the reasoning effort the request asked for (`off` when thinking was disabled). It is NULL when no profile set one and the provider default applied. It records what was requested, not what the provider spent; `reasoning` holds reported thinking tokens when a provider reports them.

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

Devin also requires its successful completion trailer and complete framing before releasing text or tool calls. The session resolved by `GetUserJwt` is reused across requests in the process until shortly before its user JWT expires (five minutes when no expiry is readable); an authentication failure discards it so the next attempt signs in again. Connect responses from Devin and Cursor enforce expanded-byte limits as well as wire-byte limits. Cursor reuses one connection for the appends and heartbeats of a request. Endpoint API-key credentials are resolved again for each retry attempt, so replacement or removal during backoff takes effect before the next dispatch.

**Rejected reasoning parameter.** If a request fails with HTTP 400 and the body names `reasoning_effort`, `reasoning.effort`, or an unsupported parameter involving `reasoning`, Dial retries once without the reasoning parameter (including an explicit thinking-off). A retry that succeeds is remembered for that endpoint URL and model for the life of the process, so later requests omit the parameter up front. Nothing is written to configuration. An unrelated 400, or a request that sent no reasoning parameter, is not retried.

**API-key pools.** A provider can name several keys: `api_key_env` plus `api_key_envs = ["KEY_B", "KEY_C"]` on `[providers.<id>]`. When the active key is rate limited or out of quota, that key is blocked until the Retry-After or quota reset time (60 seconds when the provider gives none, at most one hour) and the same request is retried at once with the next key whose variable is set. This consumes one attempt. The key that works keeps serving later requests, and blocks are shared by every session in the process. If no other key is available the ordinary backoff applies. Pool state is process-local and never persisted; only variable names are tracked, so key values are never logged. Each variable must belong to the provider's origin, as for `api_key_env`. A pool needs a key-authenticated endpoint; OAuth and subscription routes do not use one.

Transient provider errors use fixed retry waits with server retry hints and visible notices (up to `[retry].max_attempts`
attempts, default 10; unhinted waits double from 2 seconds up to 60 seconds, every wait is bounded by `[retry].max_delay_secs`, default 300, and longer server hints stop the request). This includes macOS socket `Invalid argument (os error 22)` and
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

Unprofiled chat-completions requests to `api.openai.com` for a model the catalog records as an
effort-based reasoning model send the output allowance as `max_completion_tokens`, since OpenAI
documents `max_tokens` as deprecated and incompatible with its reasoning models. Other hosts keep
`max_tokens`.

Native Anthropic profiles support declared budget/adaptive thinking and `output_config.effort`.
Where the exact model's catalog record documents a request rule, the Messages body follows it:
`no-sampling` models never receive `temperature`, `top_p` or `top_k`; thinking off becomes no
`thinking` with `low` effort (unless an effort is set) on `thinking-always-on` models,
`{type: "between_tools"}` on `thinking-off-between-tools` models, and stays `{type: "disabled"}` with
effort capped at `high` on `thinking-off-up-to-high` models. Models without a record keep the
generic request.
Native Anthropic Messages requests automatically cache the stable tool/system prefix and rolling
conversation history (at most four breakpoints: the last tool, the system prefix, and two rolling
history anchors). `system` is sent as one block per prompt band (frozen, stable, dynamic) and the
system breakpoint sits at the end of the last frozen or stable band, so a change to per-turn notes
or memory in the dynamic band no longer invalidates the cached tools and stable system prompt.
Subscription requests keep their billing and Claude Code identity blocks first. A prompt with a
single cacheable band is still sent as one plain string. Native compaction requests carry the same
breakpoints, so summarizing a long history reads the cached prefix. A live-prefix Soft or Handoff
summary on Anthropic Messages (`Backend::live_summary_profile`) resends the live request's wire
messages unchanged with one transient instruction after them and keeps the live `tool_choice`, so
both rolling breakpoints land where the live turn put them; forcing `tool_choice: none` there would
invalidate the cached messages. Request-local task packets and
reliability guidance stay outside the reusable prefix; signed thinking blocks are never cache
anchors. Tool schemas are ordered deterministically, and changing harness guidance no longer
rewrites tool descriptions.

### Cache-stable tool surfaces

Editing `tools` between requests invalidates every cached prefix after it, so on two verified
routes the main loop sends one tool surface and changes only which tools are callable
(`llm::tool_offer`, `Backend::tool_exposure`). Dial's admission remains the authority: a call to a
tool the request does not offer is refused before schema validation, approval or execution, and
loading still grants no permission.

- **Anthropic Messages on `api.anthropic.com`** (API key or subscription), with no proxy body hook,
  for a model the tool search documentation lists (Fable 5/5.1, Mythos 5/5.1/Preview, Opus 4.5-5.5,
  Sonnet 4.5-5.5, Haiku 4.5 and 5.5): the stable core is sent loaded; every other eligible catalog
  tool is sent with `defer_loading: true`. A successful `tool_load` (or `tool_search` with `load`)
  whose every name became callable returns `tool_reference` blocks as its `tool_result` content,
  which the API expands inline, so the `tools` array stays byte-identical and the tools, system and
  history caches survive the load. Deferred tools sort after the loaded ones and the tool cache
  marker sits on the last loaded tool (deferred tools cannot carry `cache_control`). A partial load,
  an unload, or a load with a known readiness blocker keeps its text result; an offered tool with
  no kept reference (a controller load, a mode tool, a compacted result) is sent loaded. Planning-
  gate and single-call narrowing keep the surface and are refused by state. A 400 naming
  `defer_loading` or `tool_reference` resends the plain callable set once and latches that
  endpoint and model to it for the process. No beta header is sent: the feature is documented as
  stable.
- **OpenAI Responses on `api.openai.com`** (API key, not ChatGPT/Codex): the full offer plus
  `exit_plan` and `goal` is sent, and a request that offers fewer of them adds
  `tool_choice: {"type":"allowed_tools","mode":"auto","tools":[...]}`. Handoff's forced `none`
  wins. Loads still add schemas, as before.
- **Every other route**, subagents and hermetic runs receive exactly the callable set and plain
  tool-result text, as before. Recorded tool contracts are always checked against the callable
  set, so resumed sessions validate as they did.
A reply that ends with Anthropic `stop_reason: "refusal"` or `"pause_turn"` keeps that finish reason;
`Reply::provider_stop()` reports it as a refusal (with the `stop_details.category` label when the
API sends one) or a paused server-tool turn, so neither is mistaken for a finished answer.

In an Anthropic model profile, `prompt_cache=false` opts out (use this for a compatible endpoint
that does not support caching). Omission enables automatic caching; existing `prompt_cache=true`
still requires `capabilities.prompt_cache=true`. `cache_retention="auto"` (default) uses one-hour
retention for OAuth requests to the official Anthropic API, five-minute retention otherwise.
Set `cache_retention="short"` to force five minutes or `"long"` to request one hour from an endpoint
that supports it. One-hour retention applies to the tool and system breakpoints only; the rolling
history breakpoints always use the five-minute default, which keeps every longer-lived entry ahead
of the shorter ones and avoids paying the one-hour write price on every turn's new history. Long-lived cache writes can cost more for API keys; retention does not reduce
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

### Provider-specific routing and explicit caching

These settings extend the existing `model_profiles` contract: select the provider/model normally,
and use an exact matching profile (`--profile name`, `/profile name`, or `model_profile` in TOML)
when opting into these controls. A unique exact-model profile is also resolved automatically.
No extra agent tool calls or provider-owned agent loops are introduced. Dial still executes tools
and enforces approvals, history and cancellation.

**OpenRouter.** Only the `openrouter` provider with Chat Completions or Responses accepts
`openrouter_routing`. Omitting it leaves OpenRouter/account defaults unchanged:

```toml
provider = "openrouter"
model = "anthropic/claude-sonnet-4.5" # replace with your chosen exact OpenRouter model
model_profile = "private"

[model_profiles.private]
provider = "openrouter"
model = "anthropic/claude-sonnet-4.5"

[model_profiles.private.openrouter_routing]
sort = "latency"                   # price | latency | throughput
only = ["anthropic"]               # hard provider allowlist; use current upstream slugs
allow_fallbacks = false
require_parameters = true
data_collection = "deny"           # allow | deny
zdr = true

[model_profiles.private.openrouter_routing.max_price]
prompt = 3.0                       # USD per million input tokens
completion = 15.0                  # USD per million output tokens
```

Optional `order` prefers provider slugs in order; `ignore` excludes slugs. Optional `max_price.request`
and `.image` are USD per unit. Unknown fields, malformed/empty slug lists and nonfinite/negative prices
are rejected. Preferences, including explicit `false`, remain on each retry; they are never stripped
to recover a failed request. An active routing profile disables Dial's automatic model fallback and
compaction promotion, because those exact-model constraints cannot safely transfer to another route.
An explicit user model selection remains available. OpenRouter enforces upstream eligibility;
`zdr` is not an unconditional no-retention guarantee (account safety-retention policies still apply).
See [OpenRouter routing](https://openrouter.ai/docs/guides/routing/provider-selection).

**Gemini explicit caches.** Implicit caching is unchanged. Explicit caching is **off by default**;
it creates billed server-side storage and may cost more for short or infrequently reused prompts.
Enable it only for a suitable exact native-Gemini model profile, after checking current cache prices:

```toml
provider = "gemini"
model = "gemini-2.5-pro"
model_profile = "cached"

[model_profiles.cached]
provider = "gemini"
model = "gemini-2.5-pro"

[model_profiles.cached.gemini_cache]
ttl_secs = 300
min_tokens = 4096
max_entries = 4
max_prefix_bytes = 1048576
```

Supported identifiers are `gemini-2.5-pro`, `gemini-2.5-flash`, `gemini-2.5-flash-lite`,
`gemini-3-pro-preview`, `gemini-3-flash-preview` and `gemini-3.1-pro-preview` (also with `models/`).
Unknown/custom and Gemini 4+ identifiers bypass explicit caching. This conservative allowlist is not
a promise that a retired model is still available. Native Gemini protocol is required; use
`gemini_cache`, not generic `prompt_cache` or `cache_retention` overrides.

Dial first observes a prefix without creating storage. A repeated stable prefix is admitted only
after an automatic `countTokens` request confirms at least `max(min_tokens, 4096)` tokens. The
prefix contains stable instructions/tool policy and eligible immutable first-user text when followed
by later conversation; current tails, model thoughts, tool results and media are not frozen.
Changing instructions, tools, endpoint, credentials or model isolates the cache. Cache preparation
adds backend HTTP requests, not model-visible tool calls; preparation errors surface instead of
silently changing the cache policy.

TTL must be 30–3600 seconds, `min_tokens` 2048–1048576, `max_entries` 1–32, and the byte bound
1024–4194304. There are at most 32 process-local slots globally, also bounded per endpoint/auth/model
scope. Busy/full/small or nearly expired entries use ordinary inference. Successful resources remain
reusable until TTL, without renewal; local state is not persisted. Failure/cancellation deletes a
known resource with an independent two-second best-effort cleanup budget and no detached task.
Unknown create outcomes, failed deletion and process exit rely on the requested server TTL; storage
may remain billed until expiry. A tombstone prevents immediate recreation after an ambiguous create.
Cache rejection surfaces the original generation error, not an uncached replay. Cached token usage
continues through the normal usage parser; the usage ledger is not a full storage-charge invoice.
See the [Gemini cache API](https://ai.google.dev/api/caching) and current provider pricing before
opting in. No cache-hit, latency or cost improvement is guaranteed.
