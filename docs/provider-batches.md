# Explicit paid provider batches

`dial provider-batch` submits **tool-free** independent requests to the native paid OpenAI Batch API or Anthropic Message Batches API. It does not change settings, enable a scheduler, use subscription tokens, run tools, or submit anything at startup. OpenAI-compatible proxies and custom URLs are deliberately unsupported. These commands may incur provider charges; they are not the existing agent benchmarks or distillation pipeline.

## Prepare, review, submit

A plan is JSON:

```json
{
  "provider": "openai",
  "model": "YOUR_PAID_API_MODEL",
  "max_tokens": 1024,
  "items": [
    {"custom_id": "document_1", "prompt": "Summarize this approved text..."}
  ]
}
```

```sh
dial provider-batch prepare plan.json
# Prints a local ID. Preparation does not use the network.
dial provider-batch inspect LOCAL_ID
dial provider-batch submit LOCAL_ID --yes-paid
dial provider-batch status LOCAL_ID
dial provider-batch results LOCAL_ID
dial provider-batch cancel LOCAL_ID
```

Use `anthropic` for Anthropic. Current native API credentials are resolved through Dial's existing credential resolver (including `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`); OAuth/subscription credentials are rejected. No API credentials or custom headers are written to the ledger. Preparation and collection use the existing credential-pattern/environment-secret redactor, and credential-shaped identities are refused. This is not a guarantee that arbitrary sensitive prose is detected. Review input before explicitly authorizing upload; provider retention/privacy policies apply. Paid work is authorized by the explicit CLI submission, not inherited from a prior agent/tool approval.

## Useful offline consumer: archive summary snapshots

```sh
dial provider-batch archive-summaries --provider anthropic --model YOUR_PAID_API_MODEL SESSION_ID OTHER_SESSION_ID
```

This prepares independent summaries from the existing archive's session metadata, up to 100 recent prompts and existing recaps. **It is not a full-transcript summarizer.** The snapshot is immutable once prepared. Inspect the local ID before submitting: archive history can contain sensitive information. Collected summaries are untrusted generated text, printed for explicit review; they never mutate memory, recaps, source files or model training data. A late result therefore cannot resurrect forgotten memory or overwrite newer archive state.

## Durability and limits

Private job records live under `<user_dir>/provider-batches`, outside the workspace. References, requests and collected results survive Dial restart; there is no daemon and no automatic polling, resubmission or application. Requests (including the native JSON/multipart wire envelope) and combined result downloads are limited to 4 MiB, with hard ceilings of 1,000 items per job and 16,384 requested output tokens per item. **Those ceilings are not independently usable maxima:** preparation and submission also require `items × (16 KiB + max_tokens × 1,536 bytes) + 2 bytes ≤ 4 MiB`. This conservative result budget assumes at most 256 UTF-8 bytes per output token, up to sixfold JSON escaping, and 16 KiB per item for provider metadata, IDs, errors and framing; two bytes cover OpenAI output/error file separators. These are explicit admission safety assumptions, not guarantees about arbitrary provider metadata or future tokenizers. For example, 1,024-token archive summaries permit at most two items per job, and even a single 16,384-token request is unsupported. Reduce `max_tokens` or create separate smaller plans before submitting; Dial never automatically splits, retries or pays. Providers exceeding these conservative assumptions are still rejected by the protective download bound, never silently truncated. Existing larger ledger records remain readable, but cannot be newly submitted without satisfying admission. The ledger permits at most 1,000 jobs; remove old private job records explicitly when no longer needed. Input/output text is private state, not a public artifact.

Submission intent is fsynced **before** the network operation. If the receipt is lost (or Dial stops), the local record remains `submission_unknown`. Do not retry with a new job assuming the first failed: inspect the provider console and reconcile charges there. Dial deliberately refuses to replay that local job. A successful OpenAI upload followed by a failed create can leave an input file on the provider; inspect/delete it in the provider console. There is no automatic provider-file cleanup.

Cancellation is a request, not a terminal state. Providers may finish and charge some items before cancellation settles. `results` requires a terminal provider state, retrieves output and error files where applicable, correlates by `custom_id` rather than ordering, rejects duplicate/unknown IDs and reports absent items as `missing`. Individual failures, expiry and cancellation are not successes. Only OpenAI `stop` and Anthropic `end_turn`/`stop_sequence` are complete text success; token limits, unknown/missing stop reasons and unsupported tool completions are `incomplete`, and content filtering/refusal is `refused`. Both retain partial output and a reason for review, with the same private redaction as successful text. Previously persisted successful strings remain readable. Collected results are cached privately so repeated collection does not apply anything twice. Remote result URLs are never followed with credentials: only fixed native API paths are used, with redirects disabled and bounded HTTP timeouts.

## Verification boundary

Local fixture tests exercise protocol routes, multipart upload, result correlation and private restart records without a paid account. They do not establish live model eligibility, pricing, retention guarantees or provider availability. No live paid submissions are part of the default test suite.
