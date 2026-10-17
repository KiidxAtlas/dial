# Architecture

Dial is a Rust control plane (agent loop, tools, UI, storage, learning loop) with a small Python
worker used only for MLX inference and LoRA training.

## Workspace layout

The repository is one Cargo workspace. The root package `dial` holds only the binary (`src/main.rs`,
`src/cmd/`), a facade library (`src/lib.rs`) and the integration tests (`tests/`). Every other
member lives under `crates/`. The facade re-exports each crate's modules under their historical
paths (`pub use dial_agent::{agent, …}`), so `dial::<module>` keeps resolving for the binary, tests
and external callers.

Crates are listed lowest layer first. A crate may depend only on crates in the layers above it in
this table (see Dependency boundaries).

| Layer | Crate | Modules and contents |
|---|---|---|
| 1 | `dial-build-info` | `build_info`: revision, dirty state and build epoch (`build.rs` emits them) |
| 2 | `dial-core` | `platform`, `runtime` (shared result bodies, execution facts, cancellation, stream journal, schemas, arguments), `output`, `workspace_copy`; root helpers `hex`, `install_tls_provider`, `flush_stdout_on_exit` |
| 3 | `dial-llm` | `config` (types, defaults, settings declarations, layered views, validation, paths), `llm` (provider clients, acquisition, streaming, tool-call repair), `url`; `build.rs` builds the Apple Foundation Models helper |
| 4 | `dial-storage` | `storage`: sessions, checkpoints, memory, project understanding, skills, settings outside the workspace |
| 5 | `dial-tools` | `tools` (model-callable tools, grouped below), `setup`, `security`, `voice`, `embedded_shell`, `browser_relay` |
| 6 | `dial-computer` | `computer`: desktop capture and input; `build.rs` sets the desktop cfg and builds the macOS capture helper |
| 7 | `dial-agent` | `agent` (agent loop, grouped below), `learn`, `judgment`, `adaptive`, `steering`, `reliability`, `telemetry`, `stats`, `extensions`, `commit`, `pack` |
| 8 | `dial-ui` | `ui` (terminal interface: composer, transcript, overlays, themes), `git_panel`; bundled palettes live in `crates/dial-ui/themes/` |
| 9 | `dial-app` | `app` (application wiring; directory module `app/`), `commands` (slash commands), `cli` (subcommands, flags, plain-text tables), `benchmark`, `repl`, `rpc`, `acp`, `schedule`, `changelog`, `collab` |

The root `build.rs` only declares rerun inputs; per-crate build scripts own their own outputs.

### Tools sub-domains (`crates/dial-tools/src/tools/`)

| Group | Contents |
|---|---|
| `exec` | bash and its interceptor, sandbox backends, shell, background processes, kernel, eval, speculation, signals |
| `edit` | edit transactions, hashline tags, native edit engines, preview, syntax checks, AST edits |
| `fs` | file, search, walk, select, notebook, archive and document tools |
| `code` | LSP, DAP, diagnostics and navigation |
| `net` | web fetch and the browser tool |
| `vcs` | git and GitHub (`gh_*`) tools |
| `integrations` | external tool extensions (`ext`), MCP, plugins, SSH and resources |
| `knowledge` | codebase index, provenance, history and autoresearch |
| `surface` | tool catalog, contracts, outcomes and presentation |

The `tools` root keeps the `Workspace` confinement type, schema plumbing and the registry; the
groups expose the `dial::tools::{edit::hashline, exec::sandbox, fs::documents, integrations::ext,
knowledge::{codebase, provenance}}` paths that the binary and tests use.

### Agent sub-domains (`crates/dial-agent/src/agent/`)

| Group | Contents |
|---|---|
| `run` | turn loop, engine driver, tool-call dispatch, tool exposure, call summaries |
| `model` | backends and routing, Laya, local model profiles |
| `window` | context window, prompt assembly, request prefix |
| `compact` | compaction and compaction runtime |
| `verify` | focused verification, repair, completion and auto-escalation |
| `safety` | approvals and safety rules |
| `workers` | child agents, delegation, isolation, live tasks |
| `workflow` | todos and questions |
| `memory` | self-play and memory helpers |
| `ui` | execution-state reducers that feed views (`state_ui.rs`) and UI adapters |
| `base` | shared helpers, accessors and lifecycle |

`modes/` (goal, plan review, ratchet, loop runner and others) stays at the `agent` root and is
mounted by `agent/mod.rs`.

## Dependency boundaries

Enforced by `tests/integration/architecture.rs`.

- **Crate layers.** `CRATE_LAYERS` lists the workspace crates lowest first (`dial-build-info`,
  `dial-core`, `dial-llm`, `dial-storage`, `dial-tools`, `dial-computer`, `dial-agent`, `dial-ui`,
  `dial-app`). `crates_depend_only_on_lower_layers` requires every workspace-crate dependency of a
  crate, including dev-dependencies, to appear earlier in that list. An unlisted crate is a
  violation, so a new crate is placed in the order deliberately.
  `crate_manifests_are_checked_against_the_layer_order` tests that check against fixture manifests.
- **Module rules.** `modules_respect_their_dependency_boundaries` scans the production code (test
  modules exempt) of every `crates/*/src` root and rejects `crate::<module>` paths that cross these
  rules, by module name:
  - `ui` does not use `learn`, `tools` or `config`, except `config::schema`, `config::settings_store`,
    `config::paths` and `config::laya`.
  - `llm` does not use `ui`, `learn` or `tools`.
  - `tools` does not use `ui`, `learn` or `agent`.
  - `runtime` does not use `ui`, `learn`, `agent`, `tools` or `llm`. Shared argument/schema/template/structural engines and process execution have neutral owners. Journal reclamation uses private storage publication after the session fsync barrier.
  - `storage`, `agent` and `learn` do not use `ui`; `config` does not use `ui` or `learn`.
  - `workspace_copy` does not use `agent`, `benchmark`, `learn`, `config`, `llm` or `ui`.
    Evaluation and worker isolation consume this shared policy; benchmark re-exports retain the
    earlier public copy API. Private dependency materialization keeps its narrower allowlist.

Tests never live inline in `src/`: a `#[cfg(test)]` module is declared with `mod tests;` and its body
sits in the sibling `tests.rs` (`foo.rs` → `foo/tests.rs`). The same test rejects inline test module
bodies and `#[test]` functions outside such files.

Changing a rule means editing that test and stating why here.

## Invariants

- The session owns the active branch and execution tree. Conversation records are immutable;
  in-flight assistant, reasoning, tool and worker nodes have shared mutable result bodies.
  `crates/dial-agent/src/agent/ui/state_ui.rs` reduces execution callbacks into these bodies before notifying views.
  The terminal binds to the bodies, RPC projects revisioned updates, and request/tool boundaries
  journal changed snapshots as `runtime` records in the existing branch-aware JSONL file.
  Recovery marks unfinished nodes interrupted; rewind and fork restore only the active branch.
  These records never become provider chat roles. Structured tool data stays separate from its
  model-facing text, so protocol clients do not reconstruct objects from rendered summaries.
- Child configuration follows `Config::for_role`: inherit parent controls and model profile unless
  an explicit role assignment overrides them. Automatic steering/adaptation remain main-agent
  controls. Child permissions still use the parent policy; inheritance does not grant permissions.
- Transcript rows are cached by body revision, width, theme and display settings. Stable terminal
  rows reuse their encoding; changed rows are encoded once into one synchronized frame write.
  Resize, scrollback commits and image placement force a full repaint. Git status and per-file
  numstat queries run on cache workers, outside rendering.

- Tool execution always passes through schemas, approvals, workspace confinement and the
  checkpoint observer: schema validation (`crates/dial-agent/src/agent/verify/repair.rs`, `call_form_error`) precedes approval
  (`crates/dial-agent/src/agent/run/dispatch.rs`, `dispatch_inner` → `crates/dial-agent/src/agent/safety/approval.rs`), and confinement and checkpointing
  run in `crates/dial-tools/src/tools/mod.rs` and `crates/dial-tools/src/tools/exec/sandbox.rs`.
- Shell confinement is platform-owned: macOS retains `sandbox-exec`; Linux/WSL2 uses
  bubblewrap namespaces (`crates/dial-tools/src/tools/exec/sandbox/linux.rs`). Linux exposes only the system
  runtime, workspace and explicit extra-write directories. Nested private storage is
  excluded from native tools and sandboxed commands, allowing a containing workspace
  such as HOME. Linux masks existing private paths after writable mounts; missing
  denied paths beneath writable roots still fail closed rather than creating host
  directories. Unsafe runtime overlaps and enabled WSL Windows interop are refused.
  Both backends fail closed after real enforcement probes. There is no implicit unsandboxed
  fallback, and approvals never grant one; the only unsandboxed mode is an explicit operator
  opt-out (`Workspace::without_sandbox`), a separate trust mode rather than an approval. MLX remains
  macOS/Apple Silicon only; Linux/WSL2 uses hosted or OpenAI-compatible providers.
- Every completed live edit turn, including text-only completion, surfaces a bounded checkpoint diff; shell and approval-requiring extension calls are tracked as uncheckpointed and require staged/unstaged Git review (`crates/dial-agent/src/agent/mod.rs`, `crates/dial-storage/src/storage/checkpoint.rs`).
- Failed focused checks remain named repair obligations and are re-run alongside checks selected from new changed files; project-aware mappings use Python, Rust, Go and JavaScript test metadata (`crates/dial-agent/src/agent/verify/verification.rs`, `crates/dial-agent/src/agent/mod.rs`).
- Finalization is reentered after stop-hook continuations, so later edits cannot bypass focused verification. Repairs and hooks share the original turn budget and retain separate bounded retry counts. Rejected completion reports keep a clearly labeled candidate work summary alongside the actual failure, blocker or interruption.
- A learned adapter is used only after it passes the evaluation gate; admission and rollback
  invalidate task routes (`crates/dial-agent/src/learn/gate.rs`, `crates/dial-agent/src/learn/profiles.rs`).
- Weight candidates are likelihood-screened on validation targets before rollout evaluation;
  screening can reject, never admit. Exact rank-concatenated adapter mixtures and closed-form
  steering adapters use the same admission, integrity checks and warm-start path as trained LoRA.
- Reasoning is retained only on the final harvested assistant target; training and scoring use
  one template renderer and target mask. Verified live-task examples are advisory user-message
  context, excluded from hermetic evaluation and stripped of historical argument values.
- Training yields compute to foreground coding at a step boundary (`crates/dial-agent/src/learn/compute.rs`).
- Dial-owned MLX-LM decoding is constrained only inside tool-call spans of the supported
  parser formats (`json_tools`, `gemma4`, `function_gemma`, `glm47`, `mistral`; see
  `python/dial_tool_grammar.py`); it never alters free text or thinking, falls back to
  unconstrained sampling at a dead end, and does not replace Rust-side argument validation.
- Per-project state and secrets live outside the editable workspace (`crates/dial-llm/src/config/paths.rs`).

## Checks

```sh
cargo fmt --check
cargo build --locked --all-targets
cargo test --locked --workspace
```

Fast loops (the lib and the integration suite are separate test binaries):

```sh
make test-module MOD=llm::provider   # cargo test --locked --workspace --lib llm::provider
make test-integration                # cargo test --locked -p dial --test integration
```

Discoverable check tiers (portable checks never download models):

```sh
make test-rust              # Rust tests; TEST_PROFILE selects dev or ci
make test-python            # Python contracts; reports optional MLX skips
make test-bench-offline      # offline benchmark graders and reporting
make test-smoke DIAL=target/debug/dial  # existing artifact and scripted recovery fixtures
make test-mlx               # optional Apple-Silicon MLX checks; explicitly skips if unavailable
```

`test-smoke` does not build its binary. `PYTHON` selects an already-provisioned interpreter;
the targets do not install packages. CI uses the same Python, grader and smoke entry points.
Portable Python coverage is not evidence that hardware-dependent MLX paths passed.

Lib tests report timings with `RUSTC_BOOTSTRAP=1 target/debug/deps/<crate>-<hash> -Z unstable-options --report-time` (for example `dial_agent`).

## Runtime ownership

- `crates/dial-agent/src/agent/run/engine.rs` drives foreground and worker iterations. Role adapters own preparation, permissions and persistence; shared execution/recovery policy has one owner.
- `crates/dial-agent/src/agent/modes/lifecycle.rs` defines mode hooks for request preparation, dispatch, settlement and yielding. The parent implements goal/prewalk behavior through these hooks; workers keep their assignment policy in the worker adapter.
- `crates/dial-core/src/runtime/execution.rs` defines terminal status, effect certainty and committed paths. Tool payload and display text never authorize or reconstruct those facts. The shared tool boundary retains uncertainty through nested error handling, including a transaction whose rollback could not settle every path.
- `crates/dial-core/src/runtime/cancellation.rs` separates session termination, turn interruption, inference/read/background scopes and mutation commitment. Atomic commits settle before interruption is reported; uncertain effects never imply rollback.
- `crates/dial-core/src/runtime/stream_journal.rs` appends incremental runtime observations before publication. A body updates its observable state only after the journal accepts the change; write failure stops both callback publication and polling-visible changes. Session recovery overlays newer frames only on their active branch, marks open bodies interrupted, and never projects partial tool arguments as executable calls. Final snapshots live in the main journal. Session relocation, deletion and archiving handle both files.
- `crates/dial-llm/src/config/schema.rs` owns shared settings metadata. Defaults come from `Config::default`; model-dependent output bounds use the selected profile. UI and CLI consume the same declaration. Cross-field and provider constraints remain in configuration validation.
- `crates/dial-llm/src/config/selection.rs` owns model-selection setting changes shared by setup and interactive controllers. Preparation validates the explicit candidate before selection is published; presentation never reconstructs configuration precedence.
- Embedding setup uses the same model-selection writer as other role pickers. Switching to a
  local role clears stale remote endpoint/auth fields without clearing explicit embedding overrides.
- `crates/dial-llm/src/llm/download.rs` owns inference acquisition and quantization planning. The `crates/dial-agent/src/learn/download.rs` re-export preserves the earlier Rust import path, not learning ownership. Callers supply cancellation and progress; cached discovery does not fetch weights.
- `crates/dial-core/src/platform/atomic.rs` owns publication mechanics with distinct private-state, workspace-replacement and mode-preserving policies. Ownership/link checks and platform durability constraints are not interchangeable.
- Configuration owns its own types, defaults and persistence (`crates/dial-llm/src/config/settings_store.rs`, `crates/dial-llm/src/config/paths.rs`, and the per-domain modules under `crates/dial-llm/src/config/`). Feature modules consume configuration; configuration never imports agent, tools, storage or app code.
- CI records runtime-journal overhead and markdown/terminal encoding p95 without imposing a host-dependent speed threshold.
- RPC has one state projector for worker and cached queries. Typed notices expose their lane;
  ACP preserves source metadata in `_meta.dial` without requiring clients to change text handling.
- Session relocation treats the main journal and incremental stream journal as one cleanup unit:
  failure to reopen the destination removes both destination artifacts and preserves the source.
- Steering import, evaluation, automatic creation and promotion share per-vector mutation leases.
  Promotion holds both pending and active leases before checking state and uses the same admission
  gate as runtime loading; `--replace` never bypasses evaluation or a busy lease.

## Cross-surface review scope

The cross-system review follows ownership and shared contracts rather than requiring a rewrite
of each feature. The reviewed areas and resulting decisions are:

| Area | Design decision |
| --- | --- |
| Parent/child agent loop, recovery and tool dispatch | Retain shared lifecycle, execution-effect and cancellation owners; do not add a second orchestration pipeline. |
| Model configuration, setup and settings surfaces | Route embedding-role changes through the canonical selection writer; preserve explicit overrides and open-choice settings. |
| Benchmarks and worker isolation | Share source copying and exclusion policy outside benchmark ownership; keep private dependency materialization semantics separate. |
| Terminal, print CLI, RPC and ACP | Keep presentation adapters over shared agent events; preserve typed event metadata and use one RPC state projector. No terminal visual redesign was performed. |
| Session persistence and runtime streams | Clean up both destination journals when relocation fails, preserving the source and its live runtime. |
| Steering, import and automatic improvement | Share mutation leases and admission checks rather than duplicating weaker promotion gates. |
| Provider acquisition and optional learning | Retain existing acquisition ownership and compatibility exports; no model downloads, training runs or provider-policy changes were made for this review. |

Verification covers focused library tests plus architecture, isolation, configuration, benchmark
and RPC integration tests. It is not certification of every feature or platform. Native Windows
ProjFS, Linux overlay mounts, real provider inference, training and third-party client rendering
were not exercised. ACP metadata preservation does not establish client-side output replacement.
The earlier full-suite failures remain unresolved except for the specifically corrected schema
and MCP contract assertions; they have not all been established as pre-existing. Clippy's
correctness/suspicious gate passed with warnings, not a warning-free result. Unrelated formatting
differences were left intact.

**[INFERENCE]** Removing full-history cloning from RPC state queries and giving shared behavior
one owner should reduce avoidable work and future drift. No wall-clock, allocation or model-success
improvement was measured. Streaming execution, pruning defaults and broader protocol precedence
changes require separate evidence before changing their behavior.
