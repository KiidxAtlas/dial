# Architecture

Dial is a Rust control plane (agent loop, tools, UI, storage, learning loop) with a small Python
worker used only for MLX inference and LoRA training.

## Modules (`src/`)

| Module | Responsibility |
|---|---|
| `agent` | Agent loop, prompts, approvals, guards, subagents, todos, investigation/verification gates, harvesting of training data |
| `llm` | Provider clients (OpenAI-compatible, Anthropic, ChatGPT, local MLX server), streaming, tool-call repair (`text_tool_calls`, `json_repair`) |
| `tools` | Model-callable tools: files, hashline edits, search, shell + sandbox, LSP, AST, DAP, MCP, browser, documents, git, kernel |
| `storage` | Sessions, checkpoints, memory, project understanding, skills, settings (outside the workspace) |
| `config` | Configuration types, domain setting declarations, validated snapshots and loading |
| `learn` | Gated self-improvement: curriculum, verification, probes, LoRA training jobs, admission/rollback |
| `ui` | Terminal interface: composer, transcript, overlays, themes |
| `runtime` | Shared result bodies, typed execution facts, cancellation scopes, and streaming journal; snapshots for observers and the session journal |
| `app`, `repl`, `rpc`, `setup`, `main` | Entry points and wiring |
| `benchmark`, `benchmark_cli` | Benchmark definitions, isolation and reporting |
| `steering`, `adaptive`, `schedule`, `reliability` | Runtime steering, adaptive controls, scheduling and reliability helpers |

## Dependency boundaries

Enforced by `tests/integration/architecture.rs` (production code only; `#[cfg(test)]` modules are exempt):

- `ui` does not depend on `learn`, `tools` or `config`.
- `llm` does not depend on `ui`, `learn` or `tools`.
- `tools` does not depend on `ui`, `learn` or `agent`; `config` does not depend on `ui` or `learn`.
- `runtime` does not depend on `ui`, `learn`, `agent`, `tools` or `llm`. Shared argument/schema/template/structural engines and process execution have neutral owners. Journal reclamation uses private storage publication after the session fsync barrier.
- `storage`, `agent` and `learn` do not depend on `ui`.

Changing a rule means editing that test and stating why here.

## Invariants

- The session owns the active branch and execution tree. Conversation records are immutable;
  in-flight assistant, reasoning, tool and worker nodes have shared mutable result bodies.
  `agent/state_ui.rs` reduces execution callbacks into these bodies before notifying views.
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
  checkpoint observer: schema validation (`agent/mod.rs` `call_form_error`) precedes approval
  (`agent/mod.rs` `dispatch_inner` → `agent/approval.rs`), and confinement and checkpointing
  run in `tools/mod.rs` and `tools/sandbox.rs`.
- Shell confinement is platform-owned: macOS retains `sandbox-exec`; Linux/WSL2 uses
  bubblewrap namespaces (`tools/sandbox/linux.rs`). Linux exposes only the system
  runtime, workspace and explicit extra-write directories, and refuses denied/allowed
  overlaps and enabled WSL Windows interop. Both backends fail closed after real
  enforcement probes; approvals never grant an unsandboxed fallback. MLX remains
  macOS/Apple Silicon only; Linux/WSL2 uses hosted or OpenAI-compatible providers.
- Every completed live edit turn, including text-only completion, surfaces a bounded checkpoint diff; shell and approval-requiring extension calls are tracked as uncheckpointed and require staged/unstaged Git review (`agent/mod.rs`, `storage/checkpoint.rs`).
- Failed focused checks remain named repair obligations and are re-run alongside checks selected from new changed files; project-aware mappings use Python, Rust, Go and JavaScript test metadata (`agent/verification.rs`, `agent/mod.rs`).
- A learned adapter is used only after it passes the evaluation gate; admission and rollback
  invalidate task routes (`learn/gate.rs`, `learn/profiles.rs`).
- Weight candidates are likelihood-screened on validation targets before rollout evaluation;
  screening can reject, never admit. Exact rank-concatenated adapter mixtures and closed-form
  steering adapters use the same admission, integrity checks and warm-start path as trained LoRA.
- Reasoning is retained only on the final harvested assistant target; training and scoring use
  one template renderer and target mask. Verified live-task examples are advisory user-message
  context, excluded from hermetic evaluation and stripped of historical argument values.
- Training yields compute to foreground coding at a step boundary (`learn/compute.rs`).
- Dial-owned MLX-LM decoding is constrained only inside tool-call spans of the supported
  parser formats (`json_tools`, `gemma4`, `function_gemma`, `glm47`, `mistral`; see
  `python/dial_tool_grammar.py`); it never alters free text or thinking, falls back to
  unconstrained sampling at a dead end, and does not replace Rust-side argument validation.
- Per-project state and secrets live outside the editable workspace (`storage/paths.rs`).

## Checks

```sh
cargo fmt --check
cargo build --locked --all-targets
cargo test --locked
```

Fast loops (the lib and the integration suite are separate test binaries):

```sh
make test-module MOD=llm::provider   # cargo test --locked --lib llm::provider
make test-integration                # cargo test --locked --test integration
```

Lib tests report timings with `RUSTC_BOOTSTRAP=1 target/debug/deps/dial-<hash> -Z unstable-options --report-time`.

## Runtime ownership

- `agent/engine.rs` drives foreground and worker iterations. Role adapters own preparation, permissions and persistence; shared execution/recovery policy has one owner.
- `agent/modes/lifecycle.rs` defines mode hooks for request preparation, dispatch, settlement and yielding. The parent implements goal/prewalk behavior through these hooks; workers keep their assignment policy in the worker adapter.
- `runtime/execution.rs` defines terminal status, effect certainty and committed paths. Tool payload and display text never authorize or reconstruct those facts. The shared tool boundary retains uncertainty through nested error handling, including a transaction whose rollback could not settle every path.
- `runtime/cancellation.rs` separates session termination, turn interruption, inference/read/background scopes and mutation commitment. Atomic commits settle before interruption is reported; uncertain effects never imply rollback.
- `runtime/stream_journal.rs` appends incremental runtime observations before publication. A body updates its observable state only after the journal accepts the change; write failure stops both callback publication and polling-visible changes. Session recovery overlays newer frames only on their active branch, marks open bodies interrupted, and never projects partial tool arguments as executable calls. Final snapshots live in the main journal. Session relocation, deletion and archiving handle both files.
- `config/schema.rs` owns shared settings metadata. Defaults come from `Config::default`; model-dependent output bounds use the selected profile. UI and CLI consume the same declaration. Cross-field and provider constraints remain in configuration validation.
- CI records runtime-journal overhead and markdown/terminal encoding p95 without imposing a host-dependent speed threshold.
