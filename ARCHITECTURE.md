# Architecture

Dial is a Rust control plane (agent loop, tools, UI, storage, learning loop) with a small Python
worker used only for MLX inference and LoRA training.

## Modules (`src/`)

| Module | Responsibility |
|---|---|
| `agent` | Agent loop, prompts, approvals, guards, subagents, todos, investigation/verification gates, harvesting of training data |
| `llm` | Provider clients (OpenAI-compatible, Anthropic, ChatGPT, local MLX server), streaming, tool-call repair (`hermes`, `json_repair`) |
| `tools` | Model-callable tools: files, hashline edits, search, shell + sandbox, LSP, AST, DAP, MCP, browser, documents, git, kernel |
| `storage` | Sessions, checkpoints, memory, project understanding, skills, settings (outside the workspace) |
| `config` | Configuration types and loading |
| `learn` | Gated self-improvement: curriculum, verification, probes, LoRA training jobs, admission/rollback |
| `ui` | Terminal interface: composer, transcript, overlays, themes |
| `runtime` | Shared result bodies, lifecycle phases and revisions; typed snapshots for observers and the session journal |
| `app`, `repl`, `rpc`, `setup`, `main` | Entry points and wiring |
| `benchmark`, `benchmark_cli` | Benchmark definitions, isolation and reporting |
| `steering`, `adaptive`, `schedule`, `reliability` | Runtime steering, adaptive controls, scheduling and reliability helpers |

## Dependency boundaries

Enforced by `tests/architecture.rs` (production code only; `#[cfg(test)]` modules are exempt):

- `ui` does not depend on `learn`, `tools` or `config`.
- `llm` does not depend on `ui`, `learn` or `tools`.
- `tools` and `config` do not depend on `ui` or `learn`.
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
  checkpoint observer (`tools/mod.rs`, `agent/approval.rs`, `tools/sandbox.rs`).
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
- Training yields compute to foreground coding at a step boundary (`learn/compute.rs`).
- Per-project state and secrets live outside the editable workspace (`storage/paths.rs`).

## Checks

```sh
cargo fmt --check
cargo build --locked --all-targets
cargo test --locked
```
