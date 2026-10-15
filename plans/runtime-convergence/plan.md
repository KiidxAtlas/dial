# Runtime convergence

Complete the current OMP comparison gaps while preserving the other chat's cleanup.

- [x] Make tool status, effect certainty and change evidence typed. Keep one panic boundary and test panic after a real effect; MCP transport loss also preserves uncertainty.
- [x] Persist streaming observations before publication; crash recovery retains interrupted prose without replaying partial calls, and startup/session switching display recovered prose.
- [x] Unify turn lifecycle, admission/recovery and mode hooks behind one controller contract for parent and worker. Preparation, permissions and persistence remain explicit role adapters.
- [x] Introduce explicit session, turn, inference and tool cancellation scopes, with mutation settlement.
- [x] Keep historical tool projections usable through deterministic, revision-bound compatibility transforms, including recognized native edit dialects and faithful training captures.
- [x] Move settings declarations into configuration ownership and derive defaults/validation/UI metadata from the same authority.
- [x] Add lifecycle parity, crash/commit boundary coverage and recorded runtime overhead in CI.
- [x] Coordinate document writers by affected documents with ordered multi-document leases, preserving transaction rollback.

Verification: focused regression tests per interface, then full library/integration suites, build, formatting, Clippy correctness/suspicious gates and real terminal/crash lifecycle. Measure baseline overhead without a noisy speed gate. No commits or destructive cleanup; inspect only the changes made for this refactor.

Final verification completed on the paused combined checkout on 2026-10-14. The complete Rust command passed: 3,483 library tests, 9 executable tests and 285 integration tests, with 15 explicitly ignored tests. Python support and benchmark fixtures passed 150 tests with 4 skipped. All-target build, formatting, diff whitespace checks, Clippy correctness/suspicious gates, dependency policy and offline installer checks passed. Real terminal verification passed all 11 scenarios, including immutable image history and SIGKILL recovery without committed-effect replay. See [verification.md](verification.md) for commands, evidence and limits.

Verification artifacts are in `/tmp/dial-runtime-refactor`. Rust/terminal verification used its private `target` directory to avoid the other chat's earlier shared-build cleanup. Source hashes stayed stable after the pause apart from documented changes made here. The other chat's cleanup and feature work were preserved; its external browser-relay driver remains available as an explicitly ignored live test. No commits, staging, reversions or feature removals were performed in this verification pass.
