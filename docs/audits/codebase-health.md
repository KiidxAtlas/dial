# Codebase health and flow audit

## Scope and method

Reviewed current architecture, CI/build/install entry points, core agent execution and verification,
worker lifecycle, MCP integration, terminal input/history, CLI routing and onboarding. Used existing
workspace tests, driver contracts, Python tests, offline benchmark tests and real-terminal scripted
workflows rather than introducing another checking framework. The working tree was clean at entry.

This is a cross-system audit with focused fixes, not a line-by-line certification of every feature.
The initial pass installed no dependencies, downloaded no model weights, made no external model
calls, and committed no changes. The follow-up below includes an explicitly approved project-local
SQLite dependency update. Existing approval, confinement, schema identities and public tool
parameters are preserved.

## Fixed findings

| Finding | Implementation | Regression coverage |
| --- | --- | --- |
| A successful Rust workspace check containing `running 1 test` and empty targets could be classified unverified. | `dial-agent` verification recognizes singular and plural libtest counts. | Mixed target orders, single-test target alone, and genuinely empty runs. |
| MCP non-object arguments were silently coerced to `{}`, potentially executing unintended remote work. | `dial-tools` rejects malformed arguments before obtaining a session, with no effects. | Null, array, string, number and boolean rejection; valid calls remain usable. Existing empty-argument coverage now sends an explicit object. |
| Malformed MCP capability declarations exposed unsupported surfaces. | Capability presence requires an object, not merely a non-null value. | Malformed scalar/array/null values, valid objects and absent capabilities. |
| Streamable MCP GET session termination did not close the shared connection. | A 404 with a known session uses the existing session-ended path and notifies the supervisor. Sessionless optional GET remains optional. | Local HTTP fixture checks shared closure and supervisor notification for known sessions, and no closure without a session. |
| Slash-command Enter bypassed bracketed-paste protection, including for `/exit`. | Shell delegates draft-aware Enter handling to the existing paste guard. | Pasted command trailing Enter is suppressed; later intentional Enter and buffered typed commands submit; multiline input retains newlines. |
| Reducing history limits while browsing left stale indexes and could panic. | History trimming rebases/clamps the selected index while retaining the unsent draft. | Surviving and removed selections, navigation in both directions, and draft restoration. |

The patches reuse existing owners and state transitions. No speculative orchestration rewrite,
capability removal, visual redesign, or warning suppression was applied.

## Observed verification

- `cargo fmt --check`: passed after formatting the new regression assertion.
- `cargo test --locked --workspace --no-fail-fast`: the final combined unit/integration run passed
  those targets, but five doctest targets failed with E0463 crate-resolution errors. A subsequent
  `cargo build --locked --workspace` and `cargo test --locked --workspace --doc` passed. The full
  command is therefore not reported as an uninterrupted green run. The build emitted a macOS
  compact-unwind size warning. The precise cause of the transient crate-resolution failure was
  not established.
- `make lint`: correctness/suspicious gate passed. Other warnings remain, including large argument
  lists, a large enum variant and local simplification suggestions; this is not warning-free code.
- `make test-drivers test-bench-offline`: passed.
- `make test-python PYTHON=python/.venv/bin/python`: 166 passed, four skipped (three missing
  `mlx_vlm` checks and one optional real MLX/tokenizer check).
- `python3 scripts/test-build.py`, `python3 scripts/test-packaging.py`,
  `python3 scripts/check-packaging.py`, `python3 scripts/test-windows-contract.py`, and
  `bash scripts/test-install.sh`: passed. Installer fixtures are not native Windows execution.
- `python3 examples/benchmarks/interactive_smoke.py --dial target/debug/dial --captures
  .tmp/audit-pty-captures`: passed all 12 declared workflows, using 27 scripted provider requests.
  Includes picker/resize, wrapped-input cancellation, review/revert/undo, streaming queue/help,
  refused restart, full blockers, cancelled switching, immutable image history, crash resume
  without effect replay, and populated worker hub.
- `make test-smoke DIAL=target/debug/dial`: artifact contracts passed; repeated recovery suite
  passed 8/8 cases (36 scripted provider requests).

## Coverage limits and remaining risks

- Source review and tests do not establish that every feature is fully integrated. Live third-party
  MCP services, hosted providers, hardware-dependent MLX/training, native Windows and Linux-specific
  isolation were not exercised here.
- Real-PTY captures establish terminal behavior, not pixel appearance. A desktop screenshot of an
  already-running Dial session was inspected, but its binary provenance was not established; it is
  not visual proof of the rebuilt changes. No visual-layout changes were made.
- The local tools resolve to Homebrew Rust 1.99.1; CI honors `rust-toolchain.toml` (1.99.0). This run
  does not replace CI on the pinned toolchain and supported OS matrix.
- No latency, memory, token-cost or real-model task-success improvement was measured. Preventing
  invalid dispatch and false verification failures removes concrete failure paths, but does not
  establish a broader efficiency claim.
- Clippy's remaining structural warnings warrant contextual review, not mechanical argument-bag
  abstractions or blanket allows. No evidence justified broad deletion or subsystem replacement.

## Follow-up implementation: lifecycle and integration

The subsequent audit and authorized repair pass preserved the already-modified working tree.
It addressed these additional findings:

| Finding | Repair and coverage |
| --- | --- |
| Relocated sessions reverted to the old project on rebuild. | `/move` and worktree transitions validate destination settings before relocation, retain same-session runtime state, and adopt the destination layout/configuration at the rebuild boundary. Regression coverage includes history, attached-root revocation, settings reload, and failure rollback. |
| LSP rename snapshotted sources too late and could replace a racing destination. | Snapshot before server startup/`willRenameFiles`, revalidate after response and approval, and use native no-replace rename for forward and rollback moves. Tests cover modified sources, racing destinations, dangling links, and recreated-source recovery. |
| Task persistence policy was applied too late. | Disabled persistence skips new durable transcript capture throughout live, failed, cancelled, and completed lifetimes; runtime history remains available. |
| Worker computation success obscured integration conflicts. | Conflicts publish an incomplete outcome; integration I/O errors fail. Candidate data and recovery copies remain available. |
| Restart discarded scheduled-watch baselines. | Persisted baselines bind to schedule definitions; startup preserves unchanged definitions, detects offline changes, and migrates legacy fingerprints. |
| Account failover could violate session/cache/replay identity. | Bound/native requests refuse automatic rotation; portable requests pin the new account and require compatible capabilities. Generated correlation IDs are not mistaken for session binding. |
| Admitted high-rank adapters could not resume. | Fresh rank remains limited to 128; validated resumed layouts permit up to 1024 with existing identity, shape, and finite-tensor checks. Managed MLX tests exercise the validation boundary, not full training. |
| Background work could exceed its compute budget while running. | Active leases include elapsed time; requests cancel, controlled workers checkpoint/pause or terminate within the existing grace period, and typed exhaustion does not retry. |
| Completion discovery blocked the UI and had no bound. | A cancellable background worker uses generation gating, cache expiry, and entry/time/byte limits. Partial coverage is disclosed; explicit paths remain usable. |
| RPC controller mixed protocol responsiveness and agent ownership. | A separate actor owns execution; each parked session retains its tools and mode in one record. ACP tests cover switching, forks, close/reopen, mode, and history. |
| Parallel storage tests deadlocked in SQLite Unix VFS teardown. | Captured stacks showed inverted global/inode mutex acquisition in bundled SQLite 3.51.1. With explicit user approval, rusqlite 0.38 → 0.39 updates bundled SQLite to 3.51.3. No application-wide serialization or test suppression was added. |

The SQLite diagnosis matches the upstream [lock-order report and fix](https://sqlite.org/forum/forumpost/a820be408a).
The browser repair described in `execution-and-web.md` also remains included: bounded asynchronous
network jobs, a finite default watchdog, preserved tabs after ordinary action errors, and explicit
warnings that a transport failure does not prove an action had no effects.

### Follow-up verification and limitations

- The initial combined run was stopped after capturing the SQLite deadlock; it is not a pass.
  After the dependency correction, all 287 storage tests passed under normal parallel execution.
- Initial regression failures exposed a missing learning-test use case, incorrect correlation-ID
  binding, inline test-module placement, and relocation assertions confusing project state with
  model-specific learning state and expecting revoked attached roots to survive. These were
  corrected rather than changing the existing confinement policy or suppressing tests.
- Final `cargo test --locked --workspace --no-fail-fast --quiet`: passed all unit/integration and
  doctest targets (5,461 passed, 18 ignored; excludes nested subprocess probe counts).
  `cargo fmt --check`, `make lint`, `cargo check --locked -p dial --bin dial --no-default-features`,
  and `git diff --check` passed. Lint/slim warnings remain; this is not warning-free code.
- A closed-output-reader test intermittently returned success during earlier full runs. Its
  fixture now shuts down the peer and directly verifies `BrokenPipe` before exercising queued
  output, with failure-state diagnostics. All 93 core tests passed 20 consecutive runs and the
  final full workspace run. The original intermittent cause was not conclusively established;
  no production output-path repair is claimed.
- Managed Python: 168 passed, four optional checks skipped. Real MLX tensors covered resumed
  adapter layouts; no model download or full training run was performed.
- Driver contracts: 24 Node tests passed. Offline benchmark contracts: 55 passed.
- The pre-SQLite-update debug binary passed artifact contracts, recovery 8/8 (36 scripted requests),
  and real-PTY workflows 12/12 (27 scripted requests). These are not hosted-model or pixel proof.
- Native Linux/Windows rename execution, live third-party services, hosted-provider quality, and
  full training remain unverified. Completion cancellation cannot interrupt an OS-blocked filesystem
  syscall. No latency, memory, cost, or model-quality improvement is claimed.
- No global tools were installed and no changes were committed. Existing structural warnings and
  the macOS compact-unwind size warning remain visible rather than suppressed.
