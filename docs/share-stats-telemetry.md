# Sharing, statistics, and telemetry

## Encrypted sharing

Set `[share].server_url` or `DIAL_SHARE_SERVER`, then run `dial share [session-prefix]` (or pass `--endpoint`). `dial share --gist` creates a secret GitHub gist using the `gh` CLI. The session is credential-redacted before serialization, gzip-compressed, encrypted with AES-256-GCM using a fresh random key, and uploaded as opaque bytes (gists store base64 ciphertext). The returned URL carries the key only in its fragment; HTTP requests never include the fragment/key. Upload endpoints must use HTTPS except for loopback development endpoints.

## Statistics

`dial stats [--json] [--range 7d]` reports cross-project requests, tokens, model/tool counts, duration, and costs from the private local SQLite ledger. Transcript synchronization counts sessions/messages/tools without retaining message bodies. `dial stats --serve [--port 3847]` serves a loopback-only local dashboard; it does not listen on external interfaces.

## OTLP

OTLP/HTTP JSON export is disabled unless `[telemetry].enabled = true`. Configure `endpoint`, `headers`, traces/metrics/log toggles, and batch/flush intervals under `[telemetry]`; `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, and `OTEL_SERVICE_NAME` are supported fallbacks. Exported events include names, counts and durations, not prompts, replies, or tool arguments.
