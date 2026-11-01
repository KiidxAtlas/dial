# Follow-up to the 2026-11-01 Dial audit

This is the disposition register for [the original audit](../dial-audit-2026-11-01.md),
not a replacement for its historical findings or a claim that every risk is closed.
The original §2 command results describe the audited revision, not this working tree.
Ownership follows [ARCHITECTURE.md](../ARCHITECTURE.md): platform primitives in
`dial-core`, transports/configuration in `dial-llm`, persistence in `dial-storage`,
tool policy in `dial-tools`, desktop integration in `dial-computer`, orchestration
in `dial-agent`, presentation in `dial-ui`, and application wiring in `dial-app`.

## Reading the register

- **Implemented** means the current source contains the mitigation/refactor. Tests named below
  are coverage owners, **not assertions that those tests have been executed successfully**.
- **Retained tradeoff** records an intentional limitation or owner policy; it is not a security fix.
- **Needs owner or external** requires a maintainer decision, repository administration, tooling,
  or an external service. These items remain open.
- **Unverified** means the relevant outcome has not been demonstrated. It can qualify an
  implemented change, particularly for native platforms and hosted release workflows.

Paths are repository-relative. A suite named `foo/tests.rs` belongs to the corresponding
`foo.rs` module; existing coverage is reused where appropriate. The verification section
at the end, rather than the presence of a test in a table, governs acceptance.

## Ranked findings (§3.1–3.25)

| Audit | Disposition | Current implementation and coverage owner | Remaining boundary / acceptance |
|---|---|---|---|
| 3.1 Safe-shell glob bypass | Implemented | `crates/dial-agent/src/agent/safety/approval/shell.rs` supplies a shared quote-aware scanner; `approval.rs` consumers keep separate authorization rules. `approval/tests.rs::shell_inspection_is_conservative`, `shell_consumers_share_lexical_boundaries_without_sharing_authority`, and `approval/shell/tests.rs` cover expansion/quoting boundaries. | Nonliteral expansion and malformed syntax fail conservative inspection; the lexer does not turn different consumers into equivalent grants. |
| 3.2 Git metadata writes / host Git execution | Implemented; local regression suites passed | `crates/dial-tools/src/tools/mod.rs` rejects native mutations of Git metadata. `tools/vcs/git.rs` checks effective executable filter/textconv configuration, including includes/worktree settings, disables textconv/external diff on reads and controls history-read signature verification. Git-panel and commit inspection delegate to that owner; approved commit mutations remain separate so hooks/filters retain their approved meaning. Coverage: `tools/tests.rs::native_mutations_refuse_git_metadata_but_allow_inspection`; `tools/vcs/git/tests.rs` executable-filter, textconv, included-config, untracked-attributes and signature-verifier regressions; `dial-agent` commit suites. | Host Git reads are not generally sandboxed. Preserve the guards for explicit empty overrides, malformed/config-query failure, textconv and forced signature verification; do not infer safety merely from `--no-ext-diff`. |
| 3.3 Collab view-to-write/host impersonation | Implemented; live verification outstanding | `crates/dial-app/src/collab/{crypto,protocol,host,guest}.rs` authenticates host frames with Ed25519 and write authority with fresh per-peer challenge/HMAC proofs, including mutation sequence/kind/text. Browser owner: `assets/collab-web/{index.html,viewer.test.cjs}`. | Protocol 1004 requires newly generated links and matching clients; old links are rejected. Browser fixtures are not a live host/relay/guest interoperability or adversarial-network test. See `docs/collab.md`. |
| 3.4 Release gate and signing key | Implemented + needs owner or external | `.github/workflows/release.yml` requires main ancestry and successful exact-SHA push/main CI, isolates signing behind `release-signing`, uses restricted job permissions, private temporary key cleanup, and passes PowerShell refs via environment. Policy owner: `scripts/{release-policy.py,test-release-policy.py}`; operating contract: `docs/release-and-install.md`. | Required reviewers, environment secret scoping, tag/main rules and repository protections must actually be configured by an administrator. Hosted signing/publication is unverified; YAML is not proof of those settings. |
| 3.5 Session writer claim/unlink race | Implemented | `crates/dial-storage/src/storage/session.rs` rechecks the locked handle against the current inode, retries replacement and retains ownership through removal. `session/tests.rs`, including `writer_claim_rejects_a_replaced_inode`, owns regression coverage. | Preserve lock/identity ordering during later session changes. |
| 3.6 Recovered text tool calls | Implemented | `crates/dial-llm/src/llm/{mod,text_tool_calls}.rs` limits lifting to local/private endpoints and offered tools, preserves rejected source text and propagates recovered provenance. `crates/dial-agent/src/agent/safety/approval/worker.rs` requires one-shot human approval for recovered calls in parent/worker paths; dispatch/child/approval suites cover policy. | Automatic modes, reusable grants and batch tickets must not authorize recovered calls. Rule denial, read-only and cancellation still apply; approval is not an exemption. See `docs/llm-transport.md`. |
| 3.7 Subscription client identity/policy | Retained tradeoff (owner decision) | Retain subscription sign-in and existing client metadata in `crates/dial-llm/src/llm/{anthropic,codex_ws}.rs`. `docs/provider-catalog.md` warns about unofficial-client, account and policy risk. | **Warning-only; no additional opt-in.** This does not establish provider permission or remove suspension/compatibility risk. |
| 3.8 SSH login-shell quoting | Implemented; remote-shell verification outstanding | `crates/dial-tools/src/tools/integrations/ssh/connection.rs:131–140` transports script bytes in base64 through a fixed bootstrap to the verified POSIX shell. SSH connection/URL tests and `docs/ssh.md` own the contract. | No live remote-service test is implied. The remote shell and transfer capability remain explicit prerequisites. |
| 3.9 Registry additive contracts | Implemented | `crates/dial-tools/src/tools/surface/contracts.rs::registry_additive_compatible` walks release-ordered additive projections without weakening required fields/types/revisions. `contracts/tests.rs::registry_contracts_accept_each_intermediate_additive_release_shape` covers intermediate shapes. | New optional properties still require declaration in the additive registry. |
| 3.10 Timeout/telemetry overflow | Implemented | `crates/dial-agent/src/agent/model/laya.rs`, telemetry deadline handling, and `crates/dial-llm/src/config/{laya,validate}.rs` bound configured intervals/check arithmetic. `config/timeout_tests.rs::decision_engine_timeout_and_telemetry_flush_are_bounded` owns configuration coverage. | Not a claim that every unrelated timeout in the repository has been audited. |
| 3.11 Retry delay and synchronization | Implemented | Shared `crates/dial-llm/src/llm/retry.rs` honors bounded hints and uses bounded backoff/jitter. `retry/tests.rs` covers delay ceilings, short hints, cancellation and manual-release behavior; provider recovery retains its own attempt/auth state. | No live provider rate-limit behavior or latency measurement claimed. |
| 3.12 Relay resource exhaustion | Implemented | `crates/dial-app/src/collab/server.rs` caps connections (including handshakes) at 64 and rooms at 16, releases admission permits, logs/continues on spawn failure and warns on nonloopback binding. `server/tests.rs` owns local coverage. | These limits do not constitute Internet-facing DoS protection; public deployment remains operator-owned. |
| 3.13 Termination cleanup | Implemented; partial runtime acceptance | `crates/dial-core/src/platform/terminal.rs`, `crates/dial-ui/src/ui/{term,input/pump}.rs` and app relay cleanup route termination through normal teardown. Coverage: `ui/input/pump/tests/signals.rs` and `tests/integration/terminal_lifecycle.rs`. | Actual-binary PTY restoration and relay-listener closure are distinct from independent child-reaping proof. Linux/native signal acceptance remains separate; see verification. |
| 3.14 Git panel blocking | Implemented | `crates/dial-ui/src/ui/overlay/git/load.rs` and `git_panel` use bounded/coalescing asynchronous reads and reject stale results. `load/tests.rs` covers completion/disconnection behavior. | No large-repository performance benchmark claimed. Git subprocess policy is also governed by 3.2. |
| 3.15 RPC actor panic | Implemented | `crates/dial-app/src/rpc/actor.rs::run_boundary` catches unwind, replies once to the in-flight request, emits fatal and stops without exposing the panic payload. `actor/tests.rs` covers panic and ordinary-failure ordering. | The boundary terminates the damaged session; it does not attempt unsafe continuation. |
| 3.16 Reaped process-group leader | Implemented | `crates/dial-core/src/runtime/process.rs:10–80` makes the child private, exposes non-reaping observation, pipe-taking and owned wait/try_wait/cleanup APIs, with explicit release. `process/tests.rs::group_wait_apis_reap_once_and_disarm_signalling` owns regression coverage. | Distinct from the best-effort deserialized `ProcId` group-signaling race below. |
| 3.17 Linux launcher failure | Implemented; Linux runtime unverified | `crates/dial-core/src/platform/unix/process.rs:39–95` catches spawn panic and restores the caller's `Command` across send/receive failure. `unix/process/tests.rs` covers command preservation, subsequent spawning and children outliving a short-lived spawning thread. | Launcher loss is reported; native Linux execution, not host compilation alone, is required for platform acceptance. |
| 3.18 Linux desktop launch inheritance | Implemented; Linux runtime unverified | `crates/dial-computer/src/computer/desktop/linux/applications.rs` clears inherited environment, uses the shared allowlist, creates a separate process group and owns reaping. | Desktop/app launch behavior needs a real Linux session. Explicit user-configured environment is a separate trust input. |
| 3.19 Node worker environment / read_only | Implemented + retained tradeoff | `crates/dial-computer/src/computer/supervisor.rs` clears inherited environment and uses shared process/environment helpers. `docs/tools.md` explicitly describes the unrestricted Node runtime and helper-level `read_only`. | `require`/Node APIs remain available. `read_only` is **not a sandbox**; every run still needs approval. |
| 3.20 macOS spawn-lock bypasses | Implemented; native verification outstanding | `computer/desktop/macos/capture/screen_capture_kit.rs` and `computer/rpc.rs` use `platform::process::spawn`. | Compilation/source review is not proof of real ScreenCaptureKit/RPC pipe behavior. |
| 3.21 Duplicated worker approval | Implemented | `crates/dial-agent/src/agent/safety/approval/worker.rs::authorize_worker_call` owns shared worker authorization, used by subagents/live tasks. Worker approval and caller suites cover provenance, cancellation and denial. | Caller-specific presentation stays separate; authorization must not be reimplemented there. |
| 3.22 SQLite evolution | Implemented | `crates/dial-storage/src/storage/sqlite.rs` applies ordered migrations in an IMMEDIATE transaction, records `user_version`, rejects invalid/newer versions and offers observational compatibility checks. `sqlite/tests.rs` and owning store suites cover migration/open behavior. | Versioning does not solve retention or mean arbitrary external databases are supported. |
| 3.23 Platform lint/feature CI | Implemented; hosted/native acceptance outstanding | `.github/workflows/ci.yml` declares native Linux/macOS/Windows Clippy and Linux feature combinations; `scripts/windows-contract.txt` expands focused native contracts and documents skips. | 32 requested feature subsets produce 24 effective subsets because Wayland implies desktop. Authored CI is not a successful hosted run; Windows focused coverage is not the entire workspace suite. |
| 3.24 Unsafe documentation/platform ownership | Implemented; native safety remains review-owned | `Cargo.toml` denies `undocumented_unsafe_blocks`; SAFETY contracts and shared `dial-core/src/platform` filesystem/process primitives replace duplicated raw operations. | Lint checks the presence of explanations, not their correctness. No Miri/native-platform safety proof is claimed. |
| 3.25 Secret inheritance | Implemented | `crates/dial-tools/src/tools/exec/sandbox.rs::child_env_allowed` defines inherited runtime/toolchain allowlist policy, also reused by desktop/Node launch paths. Sandbox tests own unknown-name and credential cases. | Existing secret-name checks are defense in depth, not the inheritance boundary. Explicit configured environment remains user-authorized input. |

## LOW / hygiene: correctness and robustness

| Audit bullet | Disposition | Owner / coverage / limitation |
|---|---|---|
| Status hashes under lock and mutates acceptance | Implemented | `crates/dial-agent/src/agent/workers/live_tasks.rs:279–305` snapshots handles then projects status without filesystem revalidation. `verify/worker_acceptance.rs` retains explicit verification; `live_tasks/tests.rs::status_polling_does_not_revalidate_or_mutate_acceptance` covers observational status. |
| Three poisoned-lock expectations | Implemented | `agent/workers/live_tasks.rs` consistently recovers poisoned registry/active-call state with `into_inner`; owning live-task tests remain the coverage suite. Recovery does not erase a worker's unknown-effects outcome. |
| ProcId PID validation and group check/signal race | Implemented in part + retained tradeoff; open | `crates/dial-core/src/platform/procinfo.rs` rejects pid ≤ 1 and invalid start identity; Linux single-process signaling uses pidfd. Portable Unix group signaling still checks identity then calls `killpg`: **the group race remains best-effort, not atomically fixed**. Runtime/platform owners must evaluate a stronger portable group-ownership design. |
| CR-only SSE | Implemented | `crates/dial-llm/src/llm/stream.rs` handles LF/CR/CRLF. `stream/tests.rs` tables cover line endings and split CRLF boundaries. |
| Bedrock region prefixes / gateways | Implemented | `llm/bedrock.rs:36–75` infers only AWS service-host regions; gateways require explicit AWS region environment. `bedrock/tests.rs::signing_region_is_inferred_only_from_aws_service_hosts` owns coverage. |
| Unlink during directory iteration | Implemented | `crates/dial-core/src/platform/unix/fs.rs` snapshots entries before descriptor-pinned removal; `storage/memory_backend/files.rs` delegates. Platform filesystem and memory-files suites own cleanup coverage. |
| SQLITE_BUSY heartbeat | Implemented | `storage/memory_backend/local.rs` distinguishes busy/locked renewal from expired/reassigned ownership, with bounded waits. Local backend tests include `renewal_distinguishes_busy_from_expired_or_reassigned_lease`. |
| Session index permissions / rewritten offsets | Implemented | `storage/session.rs` uses private owner-only index files, checks ownership/link identity, propagates chmod errors and invalidates offsets on changed session identity. Session tests own permission/rewrite coverage. |
| Premature relay Listening | Implemented | `crates/dial-app/src/app/commands/maintenance.rs::await_relay_ready` waits for the child's post-bind readiness announcement; failure kills/waits. `maintenance/tests.rs` owns startup cases; an unrelated listener is not readiness evidence. |
| Startup scrollback erasure | Implemented | `crates/dial-ui/src/ui/term.rs` clears the visible display without emitting `ESC[3J`. Terminal lifecycle coverage owns teardown, not a general terminal-emulator matrix. |
| Predictable desktop input lock | Implemented | `crates/dial-computer/src/computer/desktop/control.rs` places the lock in private user state and validates owner-only regular-file properties. Desktop control tests own validation coverage. |
| SkyLight guessed layout / transmute size | Implemented + retained tradeoff | `computer/desktop/macos/skylight.rs` replaces guessed CGEvent offsets with public API use; symbol conversion checks size and states ABI preconditions. Private SPI compatibility remains native-macOS, version-sensitive and unverified. |
| reis private-layout Send | Implemented; Linux/Wayland runtime unverified | `computer/desktop/linux/wayland/libei.rs` keeps non-Send state on its owning thread and communicates through compiler-checked Send channels, rather than a private-layout unsafe Send assumption. |

## LOW / hygiene: security

| Audit bullet | Disposition | Owner / coverage / limitation |
|---|---|---|
| Elidable token wipe | Implemented | `crates/dial-llm/src/llm/auth.rs::SecretToken` uses `zeroize`. This is not a guarantee against all prior copies, paging or crash dumps. |
| Debug leaks literal secrets | Implemented | `crates/dial-llm/src/config/secrets.rs` has redacted Debug; `secrets/tests.rs` owns literal-secret regression coverage. |
| Semantic forget leaves free pages/WAL | Implemented; erasure scope limited | `storage/memory_backend/semantic_store.rs` enables SQLite and FTS secure-delete and checkpoints redaction. `semantic_store/tests.rs` checks database/WAL plaintext absence and reports incomplete WAL erasure under a busy reader. Backups, filesystem snapshots and forensic storage recovery are outside this guarantee. |
| Path-rule scope hidden from UI | Implemented + retained tradeoff | `/permissions` add/list/remove/help and removal picker explain native path-rule scope; `crates/dial-app/src/app/shell/command/tests.rs` owns UI coverage. Rules do **not** block equivalent `bash`, `grep` or `glob` access; use tool denial/confinement for that boundary. |
| Contract identity depends on map order | Implemented | `tools/surface/contracts.rs::sort_object_keys` recursively canonicalizes keys before hashing; contract tests cover insertion order. |
| file_identity reopens unsafe path | Implemented | `tools/mod.rs` uses confined no-follow `edit_transaction::open_read` and a 16 MiB bound; `tools/tests.rs::recovery_identity_obeys_workspace_confinement_and_size_bound` owns coverage. |
| Compose unconfined default / .env leakage | Implemented | Default `docker-compose.yml` is separated from opt-in `docker-compose.sandbox.yml`; `.gitignore`/`.dockerignore` exclude `.env` variants except examples/samples. `docs/docker.md` explains the opt-in weakened seccomp profile. Explicit opt-in remains an operator tradeoff. |

## LOW / hygiene: supply chain and CI

| Audit bullet | Disposition | Owner / coverage / outstanding work |
|---|---|---|
| nextest unchecked download / pip unpinned | Implemented in part | CI verifies pinned nextest archive checksums before extraction; `scripts/requirements-ci.txt` pins direct Python tools. It is **not a transitive hashed Python lock**; dependency-chain reproducibility remains open to CI/tooling owners. |
| No dependency bot | Implemented; hosted behavior unverified | `.github/dependabot.yml` schedules Cargo, Actions, pip/scripts and Docker updates. Installation/settings and actual bot operation require GitHub. |
| Floating runner images | Retained tradeoff | Workflows use versioned OS labels rather than `*-latest`; these are still mutable hosted images. Immutable runner-image ownership remains external. |
| No provenance / SBOM / image signing | Implemented for container; unverified; archives open | Release workflow requests BuildKit provenance/SBOM and signs/verifies the immutable image digest via GitHub OIDC with pinned Cosign installer 4.1.2 / binary 3.0.6. No hosted execution has established the result; standalone release archives still lack equivalent attestations. Release/security owners retain that gap. |
| SOURCE_DATE_EPOCH / reproducibility | Implemented in part; reproducibility unverified | Release/container builds supply the commit epoch; `crates/dial-build-info/build.rs` uses stable epoch/zero fallback instead of current wall-clock time. No independent bit-for-bit rebuild comparison has been performed. |
| Older tags move latest | Implemented; hosted outcome unverified | Release policy enforces semantic-version eligibility and serialized publication, including `queue: max`. Policy tests own local cases; GitHub publication remains external. Local actionlint 1.7.12 rejects the documented queue key: validator incompatibility remains explicit, not suppressed. |
| Duplicate crates / fs2 / targets / MPL comment | Implemented in part + retained tradeoff | Direct `fs2` use is replaced by standard file locks; `deny.toml` covers eight shipping targets including aarch64/Linux/musl and correctly explains MPL file-level copyleft. Transitive duplicate packages remain and duplicate policy is warning-only; do not blindly upgrade unrelated dependency trees. |
| Markdown skips CI although CHANGELOG is compiled | Implemented | `.github/workflows/ci.yml` removes the markdown paths-ignore bypass. Hosted triggering is not locally demonstrated. |
| Missing flake.lock | Needs owner or external; open | No genuine `flake.lock` has been generated. Nix is unavailable in the current environment; Nix/tooling owners must generate and verify it with the actual resolver, normally requiring network access. No fabricated lock is acceptable. |
| Broad build-info watch / now() | Implemented in part + retained tradeoff | `crates/dial-build-info/build.rs` removes nondeterministic time, but still watches `../../crates` to preserve current dirty-metadata correctness. Rebuild cost is unmeasured. Build-info owners must measure it before changing invalidation architecture. |

## Architecture, suppressions and duplication

| Audit item | Disposition | Current owner / evidence / boundary |
|---|---|---|
| Workspace-qualified imports bypass rules | Implemented | `tests/integration/architecture.rs::workspace_qualified_paths_cannot_bypass_module_rules` covers `dial_*` paths as well as crate-relative paths. |
| git_panel outside UI rule | Implemented | `tests/integration/architecture.rs:619–625` gives `git_panel` its own rule, deliberately allowing `tools::vcs` and `config::types` but not UI/agent/unrelated tool domains. |
| Serde rule falsely described as enforced | Implemented documentation correction; retained review convention | `AGENTS.md:16–26` distinguishes executable architecture/lint guards from default/deny_unknown_fields review conventions and owning deserialization tests. Required records/tagged representations are not forced into an inappropriate universal attribute scan. |
| unused_imports test-module suppression | Implemented | `crates/dial-tools/src/tools/code/lsp/servers.rs:695–696` uses a Unix-gated test module without the suppression. |
| Three dead_code field suppressions | Implemented | `crates/dial-tools/src/security/scan.rs:227–258` explicitly deserializes unused wire metadata into underscored fields with serde names; the three allowances are removed without dropping the wire contract. |
| Four OAuth loopback servers | Implemented shared lifecycle; retained protocol parsing | `crates/dial-llm/src/llm/oauth/util.rs::{bind_callback,wait_callback,Callback}` owns binding, accept/cancel/deadline lifecycle. OpenRouter `auth.rs:117–137` uses it while retaining its provider-specific parser; provider redirect/state/reply contracts are not artificially unified. `oauth/util/tests.rs` owns shared callback cases. |
| Claude usage retry copy | Implemented | `llm/usage_report/claude.rs:97–134` uses shared `llm/retry` Policy/decide/RetryControl; endpoint-specific 429/unsupported-response behavior stays local. |
| Hand-built ureq policies | Implemented shared owners; retained policy distinctions | `llm/mod.rs::agent_with` owns provider timeout/redirect/proxy policy; OAuth uses that owner. `tools/integrations/mcp/fetch.rs::agent` owns metadata-client construction. Credentialed protocol transports intentionally keep no-redirect policy distinct from public metadata fetches; no single universal agent is claimed. |
| UTF-8 truncation copies | Implemented shared byte-boundary owner | `crates/dial-core/src/runtime/presentation.rs::truncate` is re-exported by `dial-agent/src/agent/base/helpers.rs`. Domain-specific tail, character-count and payload-retention policies remain distinct; no repository-wide assertion that all truncation is identical. |

## Maintainability: all eight largest functions (§4)

These are phase extractions within existing owners, not new parallel pipelines. The historical
line counts are not current measurements. Extracting code and passing rustfmt alone do not
establish semantic equivalence or close every function-size concern in the repository.

| Original function | Disposition / current phase owners | Coverage owner / acceptance limit |
|---|---|---|
| `turn_step` | Implemented: `agent/run/turn/step.rs` and `step/{admit,prepare,generate,recover,reply}.rs` share turn context. | `step/tests.rs` and existing run/turn suites; preserve retry, stop, guidance and settlement ordering. |
| `child::run_inner` and enormous closure | Implemented: `agent/workers/child.rs:1901–1918` drives `child/turn.rs` typed prepare/step/finish lifecycle. | Existing child/worker suites; the oversized closure is removed, not hidden behind rustfmt acceptance. |
| `dispatch_calls` | Implemented: `agent/run/turn/round/{prepare,dispatch,settle,publish}.rs`. | Existing round/run/turn suites, especially approval, effects and publication boundaries. |
| `turn_inner` | Implemented: `agent/run/turn_loop.rs` uses `turn_loop/{admission,context,continuation,settlement}.rs`. | Existing turn-loop suites; backend restoration precedes fallible settlement. |
| `chat_with_control_events_inner` | Implemented extraction: `llm/provider/recovery.rs` owns RetryState/OAuthRetry/AuthAttempt/FailedAttempt. | Existing provider suites; credential re-resolution after failure must not reuse removed fallback keys. Surrounding function remains sizeable; no blanket size claim. |
| Slash `command` | Implemented: `app/shell/command.rs` separates normalization/gating and command handlers. | Existing shell tests plus `command/tests.rs` permission UI cases; those cases do not cover the entire router by themselves. |
| `child::call_tool_inner` | Implemented: `agent/workers/child/call.rs` separates prepare/rewrite/execute/finish. | Existing child/tool-call suites; preserve authorization and unknown-effects behavior. |
| RPC actor `run` | Implemented: `rpc/actor.rs` separates actor state/request handlers and panic boundary. | `rpc/actor/tests.rs`, including panic single-reply/fatal-stop and ordinary failure; not a claim of exhaustive ACP/RPC interoperability. |

## Process, documentation and test hygiene (§§2 and 5)

| Audit item | Disposition | Owner / current action / outstanding work |
|---|---|---|
| One failed binary hides remaining tests | Implemented | `Makefile` workspace test/nextest targets use `--no-fail-fast`. A prerequisite failure deliberately occurs before test execution, not as a false passing skip. |
| Sandbox prerequisite failures look unrelated | Implemented | `scripts/test-prerequisites.py` checks unprivileged Linux execution, exact `/usr/bin/bwrap` and a bounded namespace probe; Makefile invokes it before the suite. Native Rust enforcement tests still run when prerequisites pass. Host FUSE/overlay restrictions remain prerequisites, not evidence of a product defect or a passing isolation test. |
| cli_print depends on host D-Bus | Implemented fixture boundary | `tests/integration/cli_print.rs:287–302` accepts only the specific operational sleep-inhibition warning while retaining exact answer/JSON stdout/empty stderr requirements. It does not ignore arbitrary notices. |
| Historical PATH/SSL/fixture permissions failures | Unverified environment prerequisites | Re-run the owning suites with correct unprivileged PATH, readable certificates and writable isolated fixtures; historical root passes do not substitute for current normal-user acceptance. |
| Direct-main / oversized vague commits | Needs owner or external; open | Maintainers own reviewable commits, review policy and branch protections. Documentation or a local working-tree refactor cannot retroactively repair history or enforce GitHub settings. |
| Release cadence | Needs owner or external; open | Release owners must decide review/soak cadence. Main ancestry, curated notes and signing gates improve mechanics but do not establish adequate human review. |
| Oversized Unreleased/release notes | Implemented editorial/tooling change; usability remains review-owned | `CHANGELOG.md` separates historical release notes from current Unreleased work. Release-policy tooling selects the curated version section for publication rather than generated commit notes. Release owners still review accuracy/readability before publishing. |
| README signature setting drift | Implemented | `README.md` describes signature-by-default and `DIAL_ALLOW_UNSIGNED_INSTALL`, not nonexistent `DIAL_REQUIRE_SIGNATURE`. |
| OpenSSL instructions drift | Implemented | `docs/release-and-install.md` describes current signing/verification alternatives and prerequisites. |
| Architecture omits config::image | Implemented | `ARCHITECTURE.md` and the UI rule in `tests/integration/architecture.rs:603–617` include the intentionally allowed image settings API. |
| Test-module naming drift | Implemented documentation correction | `AGENTS.md:20–22` documents named out-of-line modules and declared paths; sibling `tests.rs` remains preferred. The architecture guard, not a prose-only naming claim, defines enforced layout. |
| Sleep / tight elapsed-time test inventory | Implemented in selected cases; general inventory unverified/open | Signal tests use process isolation/readiness; concurrent fixtures have focused repairs. Historical counts (165 sleeps, 128 elapsed assertions) are not a fresh census or proof of flakes. Owning suites/test maintainers must measure remaining timing risk; do not remove valid timing boundaries simply to obtain green runs. |
| 31 reasoned ignored tests | Retained tradeoff; host census confirmed | The integrated host run reported 31 ignored tests. Optional platform/model/device/service tiers remain separately reported. Owners must retain reasons and periodically review applicability; ignored is not passed. |
| No default real external networks | Retained strength | Keep deterministic fixture/local-service tests in the default suite. Live provider, desktop, hosted-release and collab checks are explicit optional acceptance tiers, never silently substituted by mocks. |
| Store growth/retention (audit overview) | Needs owner decision; open | Stats/raw-learning rows have no general expiry; immutable version history is intentionally retained. Storage/learning owners must define retention and migration semantics before destructive defaults. SQLite migration and WAL cleanup are not retention policy. |

## Verification and outstanding acceptance

### Integrated local results

Environment: macOS arm64, pinned Rust 1.99.0, default workspace features. No live-provider,
remote SSH, host-desktop input, installation, repository-settings change or release publication
was performed. Commands below are actual executions; cross-compilation is not native execution.

| Command / scope | Observed result |
|---|---|
| `python3 scripts/test-prerequisites.py` | Exit 0; Linux-specific prerequisites are not applicable on this macOS host. This does not verify Linux confinement. |
| `cargo test --locked --workspace --no-fail-fast` | Exit 101: **8,345 passed, 1 failed, 31 ignored** across primary test binaries. All ten doc-test targets completed with zero tests. The sole failure was `architecture::modules_respect_their_dependency_boundaries`: the Git-panel grouped import hid the deliberately allowed `tools::vcs` prefix from the guard. The import was subsequently rewritten with that prefix; the guard was not changed. |
| `cargo test --locked --test integration architecture::` after that correction | Exit 0: **23 passed**, including the previously failing dependency-boundary test. The whole workspace test command was not repeated after this import-only correction; its other passing results are reused, not reported as a new all-green full run. |
| `cargo fmt --check` after the final Rust edit | Exit 0. |
| `cargo build --locked --workspace --all-targets` after the final Rust edit | Exit 0. macOS linker warning: `__eh_frame` exceeds 16 MiB and may affect exception-handling performance; not suppressed or measured. |
| `cargo clippy --locked --workspace --all-targets -- -D warnings -D clippy::undocumented_unsafe_blocks` after the final Rust edit | Exit 0. |
| `cargo test --locked -p dial-tools --lib tools::vcs::git::tests -- --nocapture` | 24 passed. Local canaries demonstrate direct Git invoking configured verifier/external-diff commands while guarded reads leave their markers absent. Included textconv/blank-override regressions pass. Also included in the later tools suite. |
| `cargo test --locked -p dial-agent --lib commit::` | 109 passed. Approved clean-filter/hook behavior, private-index/dry-run preservation and guarded HEAD inspection covered. Also included in the later agent suite. |
| `python3 scripts/test-release-policy.py` | 5 passed; actual `v0.2.10` curated-note extraction also succeeded. Hosted gates/signing/publication were not exercised. |
| `node --test assets/collab-web/viewer.test.cjs` | 12 passed, using real crypto with fixture DOM/socket boundaries. Real Chromium also rendered missing/legacy-link rejection; no WebSocket attempt was observed there. This is not live collaboration interoperability. |
| Windows GNU/Zig: `cargo clippy --locked -p dial-agent -p dial-computer --target x86_64-pc-windows-gnu --all-targets -- -D warnings -D clippy::undocumented_unsafe_blocks` | Exit 0 with Rust 1.99.0 and explicit Zig C/archive tools. Default features, all package targets, not all features or native Windows runtime. |
| Local actionlint 1.7.12 against release workflow | **Failed**: rejects `queue: max` in `concurrency`. The queued-tag behavior is retained; the validator incompatibility is not suppressed or represented as passing. A compatible workflow validator/hosted validation remains outstanding. |
| `git diff --check` | Exit 0. |

The integrated test counts, excluding nested subprocess helpers, were:

| Target | Passed | Failed | Ignored |
|---|---:|---:|---:|
| Root library | 0 | 0 | 0 |
| Root binary | 59 | 0 | 0 |
| Integration (before import correction) | 346 | 1 | 3 |
| `dial-agent` | 2,157 | 0 | 5 |
| `dial-app` | 655 | 0 | 1 |
| `dial-build-info` | 0 | 0 | 0 |
| `dial-computer` | 146 | 0 | 0 |
| `dial-core` | 148 | 0 | 2 |
| `dial-llm` | 1,173 | 0 | 5 |
| `dial-storage` | 448 | 0 | 0 |
| `dial-tools` | 2,154 | 0 | 13 |
| `dial-ui` | 1,059 | 0 | 2 |

The actual-binary terminal lifecycle test passed for SIGTERM, SIGHUP and SIGQUIT, checking
terminal restoration and relay-listener closure before fallback cleanup. It does not independently
prove relay-PID reaping. The named-shell-session test passed in the integrated suite; a separate
exact child-body invocation also reported **one passed test**, ruling out a zero-test false pass.

Source accounting: HEAD remained `167e8fb5f665cbd9c7c175561ab8aa374b518c65`. The full-suite
runner recorded 2,313 input files before/after. Five files changed during execution: Windows
ProjFS/process helpers and Unix-gated test imports/helpers. Reviewed changes were cfg/comment,
type naming and equivalent Windows condition restructuring, not observed host behavior changes.
All recorded inputs were subsequently revalidated against the after-snapshot: only the one-line
Git-panel import correction differed. Final host format/build/Clippy and the architecture suite
ran after those Rust edits. This source accounting is not native Windows or Linux safety proof.

### Still outstanding

- Native Linux launcher, sandbox/overlay, signal cleanup and Wayland/libei; native Windows
  contracts/lint/runtime; macOS native capture/input/private SPI. Earlier Linux cross-checks are
  not a final native acceptance run. Wayland/PipeWire compilation is blocked by missing
  `libpipewire-0.3` / `libspa-0.2` and a cross pkg-config/sysroot.
- Actual child reaping independent of relay-listener closure; large-repository UI performance;
  live protocol-1004 host/browser/relay interoperability and adversarial role/write attempts.
- Hosted release gate, protected environment configuration, signing/key cleanup, OIDC image
  signature/provenance/SBOM verification and latest-tag publication. Local policy tests are not
  a published release. The actionlint `queue: max` tooling incompatibility remains unresolved.
- Owner decisions and external gaps listed above: portable ProcId group race, genuine Nix lock,
  standalone archive attestations, transitive Python locking, mutable runner images, dependency
  duplicates, dirty-watch measurement, retention, review protections and release cadence.
- Ignored model/device/network tiers and Miri were not run. A previously observed intermittent
  Git-load fixture failure was not reproduced in the later full UI pass; its cause remains unknown.

No blanket “all audit findings fixed” or “all platforms verified” conclusion is warranted.
