# Everyday UX acceptance

The everyday experience is a complete journey, not a count of controls or passing unit tests.
Keep deterministic fixtures, rendered component review, real terminal interaction and real-service
acceptance separate. A configured model, catalog response or successful text probe does not prove
coding quality, tool compatibility, cancellation, or crash recovery.

## Offline acceptance matrix

Use the existing owning Rust tests and `examples/benchmarks/interactive_smoke.py`. The latter
starts a loopback scripted provider and disposable project/private-state directories; it does not
exercise a real model, download weights, sign in, or change the user's configuration.

| Journey | Required observations | Evidence boundary |
| --- | --- | --- |
| Correct running work | Visible busy-send behavior; chooser cancellation preserves the draft; steering is recorded as pending/received/applied; queued work is not silently sent | Composer/controller tests plus real-PTY scripted provider |
| Verify a model | Existing text check remains text-only; coding check requires separate default-Cancel consent; structured invocation and tool-result continuation are distinct; failed/cancelled probes never become verified | Diagnostic tests and scripted provider, not coding-quality certification |
| Stop owned work | Current answer, workers and jobs retain separate scopes; requested stop differs from observed termination; retained output is inspectable; dismissing history does not stop running work | Background lifecycle tests and Jobs presentation; real external-service cleanup remains separate |
| Recover input | Preview identifies saved draft and queued requests; cancellation and Keep current preserve input; replacement is explicitly reviewed; restored queue stays held; changed input invalidates review | Session-bound recovery/controller tests and crash/resume fixture |
| Download outside Models | Closing the view preserves the session manager; hidden completion is observed once; explicit cancellation reaches the original task; downloading does not assign a model | Shared-manager tests; real download remains unverified without an approved service run |
| Recover provider failure | Authentication/refused connection is not success; the UI remains usable; completed effects are not replayed; explicit recovery can continue | Scripted errors and recovery fixtures; no claims about real account/provider availability |

Run focused tests only after their inputs settle. Build the binary separately before executing
PTY journeys; never substitute an old installed binary. Preserve explicit `CARGO_TARGET_DIR` and
pass its actual resulting binary path with `--dial`. The gallery command in
[tui-rendering.md](tui-rendering.md#visual-review-sheet) renders real terminal components with
synthetic data; screenshots do not establish native terminal key delivery or provider success.

The PTY suite includes plain-Enter after-turn and redirect preferences, queue removal, default-Cancel
diagnostic consent, a synthetic two-request roundtrip, an actual refused loopback connection repaired
only by explicit retry, scripted expired credentials, test-owned SIGKILL draft recovery, and native
process Stop/output/Dismiss. Runtime Cargo lease files are not model edits; the fixture rejects other
project files and symlinks. Refused-connection recovery uses the coding diagnostic, not ordinary chat.
The suite's captures are terminal text, not native screenshots or evidence of real-provider quality.

## Measurement

Retain the harness's raw result and any captures outside source files. Record observed journey
elapsed times, request counts, and correction-delivery intervals where the fixture can attribute
them. Unknown timing stays unknown. A scripted provider's timing measures orchestration, not model
speed. Compare matched build, machine, model, fixtures and correctness outcomes before claiming an
improvement. Report all failed trials; do not drop them from denominators. For repeated comparable
trials, report p50/p95 and sample count, rather than presenting one run as a latency guarantee.

## Real-service plan: approval required before execution

No live request, sign-in change, weight download, installation or real-service crash experiment is
authorized by this document. Before each live lane, present the exact model/provider, endpoint
(with credentials omitted), operations, cost/size bounds, disposable workspace/state paths and
cleanup steps. The user chooses the account and approves the concrete plan. Do not read or copy
private credentials into test fixtures or logs.

1. **Existing hosted account:** use a user-selected, already configured provider/model. Explicitly
   approve one text probe and one bounded coding-compatibility probe, then a tiny tool-using task
   confined to a disposable directory. Disclose the request/token limits and provider pricing when
   known; otherwise state cost unknown and obtain a user ceiling. Test expired credentials only
   with disposable invalid fixture credentials, never revoke or replace the user's working token.
   Real OAuth sign-in is a separate operator-performed step; do not accept terms on the user's behalf.
2. **Existing local endpoint:** name its exact loopback endpoint and loaded model before approval.
   Use the same bounded probes and disposable task. State that local inference uses compute and
   may load weights. Do not stop a user-owned server or start a competing load. A missing endpoint
   is a prerequisite failure, not permission to install or download.
3. **Actual download:** agree on an exact public repository, quantization/files, byte estimate,
   destination and network allowance. Start explicitly; close Models, inspect Activity, and reopen
   Models to observe the same work. Verify completed weights are discovered without assigning a
   role. Exercise cancellation separately; disclose retained partial cache and never delete shared
   cache files as cleanup. No download begins until size and destination are approved.
4. **Interrupted worker recovery:** launch Dial in disposable project/private-state directories
   against the explicitly approved endpoint. Create a uniquely named marker through an approved
   tool, interrupt only that test-owned Dial process, reopen the session, and review continuation.
   Assert the marker action is not replayed, completed work stays completed, restored input remains
   held, and unrelated processes are untouched. Never crash the user's active session or terminate
   external servers. Any uncertain mutation remains uncertain, not described as rolled back.
5. **Terminal matrix:** exercise the same draft/busy/approval/back/resize sequence in the user's
   selected terminal and tmux, with narrow and ordinary dimensions. Capture fresh pixels after
   final edits and check draft preservation, readable confirmation, safe default and explicit Stop
   scope. No hidden confirmation may be accepted below the existing minimum dimensions.

The historical live post-compaction timeout in [benchmark-measurements.md](benchmark-measurements.md#reliability-follow-up)
remains unresolved until an approved, retained real-model run attributes it. Passing scripted
journeys must not be presented as a fix or an improved live-model success rate.
