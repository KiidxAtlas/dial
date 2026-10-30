# Architecture

Dial is a Rust control plane (agent loop, tools, UI, storage, learning loop) with a small Python
worker for MLX inference and LoRA training, plus optional operator-installed Python extensions.

## Workspace layout

The repository is one Cargo workspace. The root package `dial` holds only the binary, a facade
library (`src/lib.rs`) and the integration tests (`tests/`). The binary separates process startup
(`src/main.rs`), argument declarations (`src/args.rs`), configuration overlays (`src/overlay.rs`),
launch routing (`src/dispatch.rs`), chat launch (`src/chat.rs`) and command handlers (`src/cmd/`).
Every other member lives under `crates/`. The facade re-exports each crate's modules under their historical
paths (`pub use dial_agent::{agent, …}`), so `dial::<module>` keeps resolving for the binary, tests
and external callers.

Crates are listed lowest layer first. A crate may depend only on earlier, lower-numbered layers
in this table (see Dependency boundaries).

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
| 9 | `dial-app` | `app` (application wiring, with private slash-command handlers in `app/commands/`), `cli` (subcommands, flags, plain-text tables), `benchmark`, `repl`, `rpc`, `acp`, `schedule`, `changelog`, `collab` |

The root `build.rs` only declares rerun inputs; per-crate build scripts own their own outputs.

### File ownership conventions

- A module uses either `name.rs` with children in `name/`, or `name/mod.rs`, never both.
  Keep working conventions within a subsystem; neither spelling warrants a repository-wide rename.
- Physical paths follow logical module ownership. Provider-native compaction lives in
  `dial-llm/src/llm/provider/compaction.rs`; settings declarations consumed by the schema live in
  `dial-llm/src/config/schema/`. `config/settings/` holds the settings module's own children.
- `dial-core/src/platform/` exposes portable primitives by concern (`fs`, `process`, `io`,
  `time`, `sys`). Native implementations live beneath `unix/` and `windows/` behind the same
  portable facade. Edit transaction backends similarly live in
  `dial-tools/src/tools/edit/edit_transaction/{unix,windows}.rs`.
- Slash-command handlers belong to the interactive application in `dial-app/src/app/commands/`.
  Binary-only subcommands remain in root `src/cmd/`; reusable CLI helpers remain in `dial-app/src/cli/`.
- Use ordinary `mod` declarations. Reserve `#[path]` for deliberate platform selection where
  the on-disk platform grouping is clearer than duplicating the API hierarchy; do not use it to
  conceal unfinished moves. Tests stay with the logical owner and move with it.
- Keep runtime assets in `assets/`, language-specific helpers in `python/` and `runtimes/`,
  and palettes in `crates/dial-ui/themes/`. Source moves must preserve compile-time includes and
  packaging discovery. Build output and private local state are not source modules.
- Embedded tool drivers live beside their Rust owners: browser JavaScript under
  `dial-tools/src/tools/net/browser/`, eval JavaScript under `tools/exec/eval/`, and the Python
  kernel driver under `tools/exec/kernel/`. `make test-drivers` exercises their transport contracts
  locally and in CI without installing Playwright or downloading models.
- Generated caches and runtime state are ignored and excluded from Docker contexts. Do not remove
  active build directories, installed toolchains, private state or unfinished plans as source cleanup.

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
| `knowledge` | codebase index, provenance, immutable evidence operations, history and autoresearch |
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
| `prompts` | shared built-in instruction sources, grouped by purpose; ownership index in `prompts/README.md` |
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

Start at [the prompt and core-behavior index](crates/dial-agent/src/agent/prompts/README.md)
when changing instructions. Shared wording lives in `agent/prompts`; assembly, runtime
selection, safety enforcement and budgets remain in their domain owners. Lower-layer
tool contracts and self-contained domain prompts are linked there, not duplicated.

## Dependency boundaries

Enforced by `tests/integration/architecture.rs`.

- **Crate layers.** `CRATE_LAYERS` lists the workspace crates lowest first (`dial-build-info`,
  `dial-core`, `dial-llm`, `dial-storage`, `dial-tools`, `dial-computer`, `dial-agent`, `dial-ui`,
  `dial-app`). `crates_depend_only_on_lower_layers` requires every workspace-crate dependency of a
  crate, including dev-dependencies, to appear earlier in that list. An unlisted crate is a
  violation, so a new crate is placed in the order deliberately.
  `crate_manifests_are_checked_against_the_layer_order` tests that check against fixture manifests.
  `workspace_crates_and_layer_registry_agree` additionally rejects missing, duplicate or unregistered
  crates and crates not selected by `workspace.members`.
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

`modules_have_one_conventional_definition` rejects ambiguous `foo.rs` plus `foo/mod.rs`
definitions across the root and all member source trees. Fixture tests cover both accepted Rust
layouts and violations in nested crate directories.

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
make test-nextest                    # cargo-nextest, no retries, hung-test timeouts (.config/nextest.toml)
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
- `crates/dial-core/src/runtime/evidence.rs` owns bounded versioned evidence, source snapshots, coverage and explicit uncertain clock metadata. `dial-storage::storage::artifact` retains authenticated manifests/representations under the existing lifecycle; pack, relocation and GC expand bounded manifest dependencies. `dial-tools::tools::knowledge::evidence` owns authorized capture/freshness and retained inspection/comparison/refinement. Media readers and approved monitor matches retain citations. These payloads never change execution authority, approvals, wake eligibility or verification credit.
- `crates/dial-core/src/runtime/cancellation.rs` separates session termination, turn interruption, inference/read/background scopes and mutation commitment. Atomic commits settle before interruption is reported; uncertain effects never imply rollback.
- `crates/dial-core/src/runtime/stream_journal.rs` appends incremental runtime observations before publication. A body updates its observable state only after the journal accepts the change; write failure stops both callback publication and polling-visible changes. Session recovery overlays newer frames only on their active branch, marks open bodies interrupted, and never projects partial tool arguments as executable calls. Final snapshots live in the main journal. Session relocation, deletion and archiving handle both files.
- `crates/dial-agent/src/agent/run/dispatch.rs` publishes shared-call completion and timing by call ID as each worker finishes; role adapters retain original-order provider conversation settlement.
- `crates/dial-ui/src/ui/input/pump.rs` owns terminal reads. `crates/dial-ui/src/ui/term/output.rs` owns bounded FIFO output and control-sequence ordering. Frame admission is nonblocking; terminal diff/history state commits only on acceptance, and handoffs drain before ownership changes.
- `crates/dial-app/src/app/shell/frame_pacing.rs` owns adaptive repaint cadence and bounded stall diagnostics. `crates/dial-ui/src/ui/transcript.rs` owns budgeted retirement, released render caches, replay and grapheme-safe reveal; presentation never dispatches partial tool arguments.
- `crates/dial-app/src/app/side.rs` owns tools-free side requests over immutable turn context. `crates/dial-storage/src/storage/side_thread.rs` owns private thread history and live-prefix checkpoints; recovering a pending answer marks it interrupted, never reissues inference. Explicit promotion forks saved ancestry into a full sibling without changing the main branch.
- `crates/dial-agent/src/agent/workers/live_tasks/pool.rs` owns in-memory bounded pool queues and retained logical slots. Each item keeps its own launch context; incompatible configuration invalidates reuse. Worker settings in `live_tasks/control.rs` are revisioned per worker and applied only at request boundaries.
- `crates/dial-app/src/app/restore.rs` restores the parent conversation before eligible interrupted workers through the ordinary validated resume path. Startup acquires the live control handle before a foreground turn; worker projection correlates job IDs with their current runtime-node incarnation.
- `crates/dial-llm/src/llm/identity.rs` owns scoped declarative identity selectors. Loop detection, Devin payload selection and Cloud Code Claude labeling retain their distinct prior matching semantics; a family fact for one axis is not authority for another.
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
- Session relocation, archive and deletion include the main journal, incremental stream journal and optional side-thread file. Failed destination cleanup removes only artifacts that operation created; source retirement follows successful destination publication and reopen.
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
| Terminal, print CLI, RPC and ACP | Keep adapters over shared events; add independent terminal I/O, bounded rendering, draft-safe dialogs and native transcript navigation without moving execution authority into presentation. |
| Session persistence and runtime streams | Preserve writer leases; contention creates an independent sibling. Move/archive/delete associated side history and roll back failed destination publication without damaging the source. |
| Side conversations and worker controls | Freeze side context without borrowing the running agent; persist streamed prefixes. Keep worker editors and model revisions local, and restore only safely acknowledged starts. |
| Extension panels | Validate bounded declarative layouts, report typed outcomes, and keep human input separate from tool permission. |
| Steering, import and automatic improvement | Share mutation leases and admission checks rather than duplicating weaker promotion gates. |
| Provider acquisition and optional learning | Retain acquisition ownership; centralize existing identity policy without changing its scope or running downloads/training. |
| Background execution | Long `eval` cells and named shells hand off through the existing bash adopter into the `bg` table; handed-off cells refuse host callbacks and busy names/runtimes refuse new work. |
| Media and remote reads | `read` keeps one dispatch path: hooks and approval run before vision questions; URL reads reuse the guarded web fetch client; archive formats share one admission walk with entry, size and decode limits. |
| Plugin protocol 2 | `dial-tools` validates capabilities and queues effects; the app drains them at safe boundaries behind user approval; the UI renders sanitized slots. Observed events are asynchronous and never block the turn. |
| Python extensions | The bundled stdlib SDK supports legacy EOF and managed framed request/session-worker transport. Explicit install/update prepares isolated dependencies; loading never installs packages. Neutral bounded contributions live in `dial-core::runtime::extensions`; `dial-tools` owns registration, worker leases and settings, `dial-agent::run::extension_api` admits host tool/task/model/context requests, and the app/UI own rendering and input. Plugin tools retain approvals, versioned contracts and registration ownership. Python is trusted user code, not sandboxed workspace content. |
| Portable sessions | Packing is versioned and hash-verified; unpack never overwrites and refuses traversal, links and newer schemas. A global transfer ledger makes a moved-away source fork. |
| Provider resilience | Cache-field refusals narrow a per-endpoint latch; cache warming runs on its own client and folds replay cost into session usage; OAuth accounts share one store and rotate like the API-key pool. |
| Consultation and host integration | `seance` is a read-only worker over stored transcripts; RPC host tools and URI schemes are ordinary tools subject to approval and reset on session switch. |

Verification includes the workspace build and Rust suite, Clippy with `-D warnings`, and real-PTY
scripted-provider probes for streaming controls, worker restoration, panels, navigation, writer
contention and side-session lifecycle.
These checks are not certification of every platform or feature: native Windows ProjFS, Linux
overlay mounts, real provider inference, training and third-party client rendering were not exercised.
ACP metadata preservation does not establish client-side output replacement.

No matched wall-clock, allocation, provider-cache-hit or real-model-success improvement is claimed.
Bounded queues, render budgets and shared ownership establish concrete invariants; comparative
performance and model-quality claims still require matched measurements.
