# Sharing, statistics, and telemetry

## Encrypted sharing

Set `[share].server_url` or `DIAL_SHARE_SERVER`, then run `dial share [session-prefix]` (or pass `--endpoint`). `dial share --gist` creates a secret GitHub gist using the `gh` CLI. The session is credential-redacted before serialization, gzip-compressed, encrypted with AES-256-GCM using a fresh random key, and uploaded as opaque bytes (gists store base64 ciphertext). The returned URL carries the key only in its fragment; HTTP requests never include the fragment/key. Upload endpoints must use HTTPS except for loopback development endpoints.

## Statistics

`dial stats [--json] [--range 7d]` reports cross-project requests, tokens, model/tool counts, duration, and costs from the private local SQLite ledger. Requests billed through a subscription (ChatGPT, Claude subscription, subscription OAuth) are counted separately and shown as `subscription` instead of `unpriced`; rows recorded before this was tracked stay `unpriced`. Transcript synchronization counts sessions/messages/tools without retaining message bodies. `dial stats --serve [--port 3847]` serves a loopback-only local dashboard; it does not listen on external interfaces.

The CLI, `/stats`, the Stats panel, dashboard and RPC `stats` use the same rolling window: `7d` means the last seven days relative to report generation, not a calendar week. Hourly aggregates include the hour containing the cutoff.

The stats page uses the same stylesheet as the collaboration and share viewers. Its content security policy pins the embedded script and stylesheet by hash and allows data requests only to the dashboard itself.

The report also covers whole turns, which request and tool rows cannot show. Each finished turn of a top-level session records its outcome (`completed`, `answered`, `incomplete`, `needs_input`, `blocked` or `failed`), step count, duration, whether you interrupted it, the provider error class when one ended it, and how many files it changed. `/undo` and the Alt+Up / Alt+Down ratings are recorded as counts. `dial stats` prints the settled share (`completed` or `answered` and not interrupted), the outcome breakdown, interrupted, undone and rated turns, and p50/p90 turn time; `--json` carries the same under `turns`. Worker sessions are excluded. Turns that ran before this was recorded are not reconstructed.

Errored tool calls carry a fixed error class taken from typed result fields, never from output text: `edit_source_changed`, `edit_target_not_found`, `edit_refused`, `check_failed`, `verify_refused`, `command_not_found` (exit 127), `not_executable` (exit 126), `nonzero_exit`, `signalled`, `not_started`, `integration`, `effects_unknown` or `tool_error`. `check_failed` is a `verify` check that ran and reported failures, which is the tool working; `verify_refused` is a `verify` call that could not run its checks. The per-tool lines of `dial stats` list the classes, and `--json` carries them as `by_tool[].error_classes`. Rows recorded earlier have no class, so the classes can sum to less than the error count.

No turn, feedback or tool row holds prompts, replies, arguments, paths or output.

The dashboard answers only loopback `Host` names and serves these JSON routes (all GET). `/api/stats`, `/api/models`, `/api/tools`, `/api/requests` and `/api/frustration` take an optional `range` of `24h`, `7d`, `2w`, `30d` or `all` (default `all`). `/api/version` gives build info, `/api/live` the most recently active session (404 when none), `/api/request/<id>` one request row, and `/api/trace?session=` one session. `/api/requests?limit=` returns the newest requests (default 50, max 500). Invalid `range`, `limit` or `id` values return 400.

`/api/frustration` is computed on demand from the in-tree transcripts of sessions active in the range. It reports counts only: user messages, messages with a flagged signal (yelling, profanity, anguish, negation, repetition, blame) and the flagged percentage. No message text is stored. A model-judged frustration score is not implemented; the dashboard has no judge endpoint or cancel route.

The additive `tool_facts` ledger records sanitized controller facts alongside tool outcomes:
observation kind/satisfaction separately from child execution outcome; capture, retention and
presentation completeness and known byte counts; integration dispatch/settlement/failure class;
and independent connection, notification and catalog health. Historical rows without these facts
remain unknown, not retroactively successful. Health transition counters are claimed once across
concurrent calls; state snapshots are not generic error counts or independent task-success labels.

## OTLP

OTLP/HTTP JSON export is disabled unless `[telemetry].enabled = true`. Configure `endpoint`, `headers`, traces/metrics/log toggles, and batch/flush intervals under `[telemetry]`; `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, and `OTEL_SERVICE_NAME` are supported fallbacks. Exported events include names, counts and durations, not prompts, replies, or tool arguments.

The same controller facts feed OTLP fact, byte and integration-transition counters. They never
include tool payloads, credential material, changed paths or artifact IDs. Telemetry ingress is
bounded (1,024 messages). Event ingress is nonblocking: queue overflow is counted rather than delaying
execution. Flush and shutdown barriers are never dropped on a full queue: they wait up to 15 seconds
for room and for the worker's acknowledgement, and a barrier that still cannot be delivered is reported
on stderr and counted as a `control` loss. Ingress drops, control losses, local-statistics write failures
and export losses have process-lifetime counters and loss metrics. Delivery remains best effort, not a
durable export queue; missing telemetry is not evidence of successful execution. Existing SQLite and
optional OTLP remain the only backends.

When the export target changes (a reloaded `[telemetry]` endpoint, or telemetry being disabled),
turns still open on the old target are closed first as `interrupted` (`dial.agent.outcome`, not an
error) and exported there, so their child spans keep a parent. A turn that continues after the
change is traced on the new target as a new trace. Telemetry headers (credentials) are never written
into exported payloads.
