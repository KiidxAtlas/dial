# Sharing, statistics, and telemetry

## Encrypted sharing

Set `[share].server_url` or `DIAL_SHARE_SERVER`, then run `dial share [session-prefix]` (or pass `--endpoint`). `dial share --gist` creates a secret GitHub gist using the `gh` CLI. The session is credential-redacted before serialization, gzip-compressed, encrypted with AES-256-GCM using a fresh random key, and uploaded as opaque bytes (gists store base64 ciphertext). The returned URL carries the key only in its fragment; HTTP requests never include the fragment/key. Upload endpoints must use HTTPS except for loopback development endpoints.

## Statistics

`dial stats [--json] [--range 7d]` reports cross-project requests, tokens, model/tool counts, duration, and costs from the private local SQLite ledger. Transcript synchronization counts sessions/messages/tools without retaining message bodies. `dial stats --serve [--port 3847]` serves a loopback-only local dashboard; it does not listen on external interfaces.

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
bounded (1,024 messages) and nonblocking: queue overflow is counted rather than delaying execution.
Ingress drops, local-statistics write failures and export losses have process-lifetime counters
and loss metrics. Delivery remains best effort, not a durable export queue; missing telemetry is
not evidence of successful execution. Existing SQLite and optional OTLP remain the only backends.
