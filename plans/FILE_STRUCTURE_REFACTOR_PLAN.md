# File structure refactor: single crate → Cargo workspace

Status: APPROVED, executing. Baseline: `main` at `d382c68`.

## 0. Approved decisions (supersede §4/§8 where they differ)

- Crates live in `crates/dial-<name>`; crate count is folded down; `themes/` moves into `dial-ui`;
  Phase 5 (grouping inside tools/agent, splitting `app.rs`) is in scope.
- `config` and `llm` share one crate: `llm::catalog` is wired into provider modules (chatgpt
  discovery, oauth, anthropic auth), so a `config→llm` cut would be a redesign, not a move.
- Folded crate map, in dependency order (a crate may depend only on crates above it):

| Crate | Former top-level modules |
|---|---|
| `dial-core` | `platform`, `runtime`, `output`, `build_info`, `workspace_copy` |
| `dial-llm` | `config` (incl. the `settings_schema` alias, removed), `llm` |
| `dial-storage` | `storage` |
| `dial-tools` | `tools`, `embedded_shell`, `browser_relay`, `setup`, `voice`, `security` |
| `dial-computer` | `computer` |
| `dial-agent` | `agent`, `learn`, `judgment`, `adaptive`, `steering`, `reliability`, `telemetry`, `stats`, `extensions`, `commit`, `pack` |
| `dial-ui` | `ui`, `git_panel`, `themes/` |
| `dial-app` | `app`, `commands`, `cli`, `benchmark`, `repl`, `rpc`, `acp`, `schedule`, `changelog`, `collab` |
| `dial` (root) | `main.rs`, binary-only commands, facade `lib.rs` |

- Each crate mounts its former top-level modules at its own root and imports lower crates' modules
  at the root (`use dial_core::{platform, runtime};`), so existing `crate::platform::…` paths keep
  resolving and extraction stays a move rather than a rewrite.
- Remaining production back-edges under this map (measured at `d382c68`): `agent→app` 5,
  `config→agent` 35, `config→app` 2, `config→storage` 22, `config→tools` 23, `core→storage` 2,
  `core→tools` 1, `llm→agent` 15, `llm→storage` 12, `storage→agent` 5, `storage→tools` 15,
  `tools→agent` 2, `tools→computer` 1, `ui→app` 1; plus test-only back-edges (a test file that
  exercises a higher layer moves to that layer or stops depending on it).
- Build metadata: `build_info` bakes `DIAL_BUILD_EPOCH`/`DIRTY` into consts, and its build script
  watches all sources. Hosting it in `dial-core` would rebuild every crate after any edit. It
  becomes a tiny `dial-build-info` crate (build.rs = today's revision logic) that only the root
  package and `dial-app` depend on. The one lower-layer user, `browser_relay/server.rs`, gets the
  version string from its caller. The Apple FM helper build goes to `dial-llm/build.rs`; the
  desktop cfgs and capture helper go to `dial-computer/build.rs`, the only crates that read them.
- Orphan-rule exposure measured: one inherent impl crosses a crate boundary
  (`impl ToolStatus` in `tools/outcome.rs`, type in `runtime/execution.rs`).

## 1. Diagnosis

One crate, 1,352 `.rs` files, ~485k lines (~370k production). Measured problems:

| # | Problem | Evidence |
|---|---|---|
| 1 | Crate root is a dumping ground | 57 entries in `src/` (22 loose files, 35 dirs); `src/app.rs` is 10,885 lines |
| 2 | Flat god-modules | `agent/mod.rs` mounts 88 siblings, `llm/mod.rs` 59, `tools/mod.rs` 58; no sub-domain grouping |
| 3 | Dependency cycles everywhere | `config⇄llm`, `config⇄agent`, `config⇄tools`, `config⇄storage`, `storage⇄tools`, `storage⇄agent`, `storage⇄llm`, `tools⇄computer`, `tools⇄embedded_shell`, `llm⇄steering`, `llm⇄reliability`, `agent⇄judgment`, `rpc⇄acp`, `runtime→storage`, `tools→agent`, `llm→agent`, `agent→cli`, `ui→cli` (full edge list in §3) |
| 4 | 30 `#[path]` mounts papering over layout | `main.rs` reaches into `cli/{benchmark,improve,update}`; `lib.rs` double-mounts `config/schema.rs` as `settings_schema`; `config/schema.rs` reaches into `settings/*` |
| 5 | Build surfaces assume one crate | `build.rs` emits env vars consumed by three unrelated modules (`build_info`, `llm/apple`, `computer/desktop`); `[profile.local.package.dial]`; CI/`Makefile`/`windows-contract.py` use bare `--lib` |
| 6 | Non-Rust runtime code in four homes | `src/**` (`.cjs`, `.py`, fonts, JSON, prompt `.md`), `python/`, `runtimes/{apple,browser}`, `assets/`, `themes/` |
| 7 | Crate-root test files | `src/cli_contract_tests.rs`, `src/tls_provider_tests.rs` |
| 8 | Tracked junk | `browser/orphans/97081-2713.json` (runtime state); `package-lock.json` with no `package.json`; untracked `node_modules/` not ignored |
| 9 | `docs/` mixes reference docs with one-off audit notes | `five-audit-fixes.md`, `harness-core-review.md`, `architecture-implementation-audits.md` (1,590 lines), `harness-quality.md`, `improvement.md` |
| 10 | Compile cost | Every edit rebuilds 485k lines; CI notes rustc peaks near 5 GB for the lib alone |

Module boundaries today are enforced only by a source-scanning test (`tests/integration/architecture.rs`). A workspace makes Cargo enforce them.

## 2. Constraints that shape the plan

- Public consumers of `dial::<module>` paths: `tests/integration/*` (12 files), `examples/runtime_baseline.rs`. Kept working via a facade (§4).
- `cargo install --locked --path .`, `Dockerfile`, `Formula/dial.rb`, `flake.nix`, `release.yml`, `scripts/check-packaging.py` all read the **root** `Cargo.toml` `[package]`. The root package therefore stays `dial` (bin + facade lib); the workspace is added to it, not replacing it.
- `env!("DIAL_BUILD_*")`, `env!("DIAL_APPLE_FM_HELPER")`, `env!("DIAL_CAPTURE_DARWIN_HELPER")` must be emitted by the `build.rs` of the crate that reads them → `build.rs` splits in three.
- `#[cfg(test)]` helpers are invisible across crates. `llm::fake` is used by tests in `agent`, `judgment`, `reliability`, `setup`, `storage`; `tools::testkit` only inside `tools`. `llm::fake` becomes a `dial-llm` `testing` feature (not `cfg(test)`), enabled by dependents' `[dev-dependencies]`.
- Features map onto crates: `aws` → `dial-llm/aws`; `voice` → optional `dial-voice`; `embedded-shell` → optional `dial-embedded-shell`; `desktop`/`wayland-pipewire` → `dial-computer` features. The root `dial` package re-exposes them with the same names so `make SLIM=1`, `--no-default-features` and CI stay valid.
- Only macOS compiles here; Windows/Linux-only files move with their modules and are verified by CI.
- `python/` and `runtimes/browser` are standalone language projects (uv/bun lockfiles, their own tests, `build.rs` inputs). They stay at the root; embedded paths are updated.

## 3. Cycle cuts (done first, inside the single crate)

Each row is a small, compiler-checked move. Counts are production references.

| Back-edge (refs) | What is referenced | Cut |
|---|---|---|
| `tools→agent` (1) | `agent::approval::parallel_read` | move to `tools` |
| `llm→agent` (2) | `agent::compaction::{estimate, NativeCompactionFailure}` | move to `llm::compaction` |
| `storage→agent` (4) | `agent::repair_evidence::{FileChange, RepairRecord}`, `agent::compaction::complete_boundary` | record types → `runtime` (shared facts); `complete_boundary` → `storage::session` |
| `learn→agent` (1) | `agent::repair_evidence` | same move as above |
| `config→agent` (25) | `*Config` structs + defaults from `agent::{compaction,questions,exploration,solve,agent_defs,isolation,laya}`, `laya::{PROVIDER,JUDGE_MODEL}`, `subagents::is_reserved` | all settings types and reserved-role names → `config` |
| `config→tools` (20) | `*Config`/`*Settings` from `tools::{lsp,dap,mcp,git,browser,kernel,eval,ssh,bash_interceptor}`, `EditSettings`, `default_tools`, `catalog::default_integrations` | settings types + default name lists → `config` |
| `config→storage` (22) | `storage::memory_backend::config::*`, `storage::share::Config`, `storage::settings_store::{load_layer,Scope}` | `memory_backend/config.rs`, `share` config and **`settings_store.rs`** → `config` (it is the layered-settings loader, not storage) |
| `config→llm` (105) | `llm::model_config::{ApiProtocol,ModelSelection,MAX_CONTEXT_WINDOW}`, `llm::catalog::provider_endpoint`, provider constants (`chatgpt::BASE_URL`, `apple::PROVIDER`, `anthropic::BASE_URL`), `host_of`, `anthropic_oauth::signed_in` | `llm/model_config.rs`, `llm/catalog/` (+ `models.json`, `providers.json`, `policy.json`), provider identity constants → `config::models`; `signed_in` becomes a credential-presence check in `config` |
| `config→{voice,collab,stats,telemetry,adaptive,steering,reliability}` (2 each) | `<mod>::Config` + `::default` | settings types → `config` |
| `storage→tools` (14) | `tools::artifact::{ArtifactStore,ArtifactConfig}`, `ToolOutput`, `tool_schema`, `Workspace`, `background::procinfo::ProcId` | `artifact.rs` → `storage`; `ToolOutput`/`ToolStatus`/`tool_schema`/`outcome::Effects` → `runtime` (also unblocks `stats`, `telemetry`, `computer`); `ProcId` → `runtime::process`; `Workspace` use in `storage/experience.rs` replaced by a path |
| `storage→llm` (13) | `images::{is_image,verified_bytes,MAX_IMAGE_BYTES}`, `Interrupted`, `tool_message`, `model_config::ROLES`, `json_repair`, semantic-memory provider calls | `images.rs`, `json_repair`, `Interrupted` → `runtime`; `ROLES` → `config::models`; `tool_message` + semantic backend keep `storage` **above** `llm` |
| `llm→storage` (12) | `process_record::{Record,SpawnSpec}`, `settings_store`, `session::Session` (1, `provider.rs`) | `process_record.rs`, `paths.rs`, `atomic.rs`, `redact.rs` → `runtime`; `settings_store` → `config`; the `Session` use is replaced by the value it reads |
| `runtime→storage` (2) | `atomic::PRIVATE_LIMIT`, `settings_store::write_private` | come along with `atomic.rs` |
| `llm→steering` (8) / `steering→llm` (1) / `learn→steering` (3) | `Specification`, `specification`, `local_unsupported_reason`, `runtime`, `automatic` | split `steering.rs`: spec/runtime → `llm::steering`; the loop (uses `learn`, `agent`) → `agent::steering` |
| `llm→reliability` (5) / `tools→reliability` (1) / `reliability→llm` (1) | `normalize_reply`, `strict_parameters`, `constrain_openai`, `omit_optional_nulls` | split `reliability.rs`: reply/schema helpers → `llm`; the loop (uses `learn`, `agent`) → `agent::reliability` |
| `steering→repl`, `reliability→repl` (1 each) | `repl::install_sigint` | → `platform` |
| `agent→cli` (3) | `cli::model_flags::{parse,apply,install}_thinking` | → `config` |
| `ui→cli` (1) | `cli::local_models` | → `llm::download` |
| `ui→settings_schema`, `config→settings_schema` | double mount of `config/schema.rs` | delete the alias; use `config::schema` |
| `tools→{computer,security}` (1 each) via `tools/ext.rs` | extension registry lists `computer::tools`, `security::tools` | trait/types stay in `tools::ext`; the **registry** moves to `agent` |
| `tools→browser_relay` (1), `cli→browser_relay` (2) | `client::resolve` | `browser_relay/` becomes `tools::browser::relay` |
| `embedded_shell→tools` (1) | `tools::sandbox::is_secret_env` | → `platform::env` |
| `tools→setup` (3) | `setup::deps::*` (LSP server installs) | no cut: `setup` sits below `tools` |
| `commit→agent` (1), `judgment→agent`, `reliability→agent` | `agent::make_backend` | `agent/backend.rs` (Config → provider Backend) → `llm::backend` |
| `agent⇄judgment` (5/6) | mutual | `judgment` becomes a module of the agent crate |
| `rpc⇄acp` (2/1) | mutual | one crate |
| `benchmark→ui` (3) | `theme::Theme`, `markdown::render`, `term::rows_to_ansi` | no cut: `benchmark` sits in `dial-cli`, above `ui` |

After these cuts the production graph is a DAG; `architecture.rs` gains a rule per cut as it lands so the split cannot regress.

## 4. Target layout

```
Cargo.toml                 [package] dial (bin + facade lib) + [workspace] + [workspace.dependencies]
src/main.rs                CLI entry (today's main.rs)
src/lib.rs                 facade: `pub use dial_agent as agent;` … (keeps dial::<module> paths)
src/bin_cmds/              today's cli/benchmark/, cli/improve.rs, cli/update.rs (no more #[path])
crates/
  dial-platform/           platform/, output/, build_info.rs; build.rs: revision/dirty/epoch
  dial-runtime/            runtime/, paths, atomic, redact, process_record, images, json_repair,
                           Interrupted, ToolOutput/ToolStatus/tool_schema/Effects, procinfo,
                           repair_evidence records, workspace_copy
  dial-config/             config/, settings_store, memory_backend/config, share config,
                           models/ (model_config, catalog + json, provider identity, policy.json),
                           every *Config/*Settings struct, thinking flag parsers
  dial-llm/                llm/ (providers, transports, stream, repair, server, oauth, usage_report,
                           compaction, backend, steering spec, reply normalisation), apple/ (Swift
                           helper + build.rs), features: aws, testing
  dial-storage/            storage/ (session, checkpoint, memory*, skills, codebase, history, export,
                           media, archive, dump, experience, provenance, retention, share, store,
                           session_pins, image_refs, foreign, anonymize), artifact store
  dial-setup/              setup/ (deps installer, embeddings, memory, python worker provisioning)
  dial-voice/      [opt]   voice/
  dial-embedded-shell/[opt] embedded_shell/
  dial-collab/             collab/
  dial-tools/              tools/ (grouped: edit/, shell/, search/, lsp/, dap/, mcp/, browser/ incl.
                           relay, git/, gh/, web/, documents/, kernel/, plugin/, …), runtimes/browser
                           embed paths updated
  dial-security/           security/
  dial-computer/           computer/; build.rs: desktop cfgs + macOS capture helper; features desktop,
                           wayland-pipewire
  dial-learn/              learn/
  dial-agent/              agent/ (grouped: turn/, context/, compaction/, subagents/, verification/,
                           modes/, prompt/, training/, …), judgment/, adaptive, telemetry, stats/,
                           steering loop, reliability loop, extension registry, extensions.rs
  dial-commit/             commit/, git_panel/
  dial-ui/                 ui/, themes/
  dial-rpc/                rpc.rs, acp.rs
  dial-cli/                cli/, benchmark/, repl.rs, pack.rs
  dial-app/                app.rs (split by concern), commands/, schedule.rs, changelog.rs
python/                    unchanged (standalone uv project)
runtimes/browser/          unchanged (standalone bun project, embedded by dial-tools)
assets/                    unchanged (browser extension, collab web, share viewer)
docs/                      reference docs; audits/ for one-off audit notes
tests/integration/         unchanged location; uses the dial:: facade
examples/                  unchanged
```

Dependency order (each crate depends only on earlier rows):
`platform → runtime → config → llm → storage → setup, voice, embedded-shell, collab → tools → security, computer, learn → agent → commit → ui → rpc, cli → app → dial`.

Facade rule: `src/lib.rs` re-exports every crate under its old module name, so `dial::llm::…` in tests/examples and `crate::llm::…` in not-yet-extracted code keep compiling during the migration. This is also what makes each extraction a mechanical step rather than a crate-wide rewrite.

Grouping inside `dial-tools` and `dial-agent` (problem #2) is a separate, later phase: it is a pure in-crate rename with no boundary impact and should not be mixed with the split.

## 5. Build and packaging surfaces to update

| Surface | Change |
|---|---|
| `Cargo.toml` (root) | add `[workspace] members = ["crates/*"]`, `[workspace.package]` (version, edition, license), `[workspace.dependencies]`; root deps shrink to the facade crates; features forward to crate features; `[profile.local.package.dial]` becomes one entry per `dial-*` member (Cargo has no glob for members) |
| `build.rs` | split: `dial-platform/build.rs` (revision/dirty/epoch; `INPUTS` paths become repo-relative from the crate dir), `dial-llm/build.rs` (Apple FM helper), `dial-computer/build.rs` (desktop cfgs, capture helper) |
| `Cargo.lock` | regenerated once; `--locked` everywhere after |
| `Makefile` | `test-lib` → `--workspace --lib`; `test-module MOD=llm::provider` → `cargo test -p dial-llm provider`; `check`/`build` unchanged |
| `.github/workflows/ci.yml` | `--lib` steps → `--workspace --lib`; memory note revisited (crates build one at a time anyway); clippy/deny unchanged |
| `scripts/windows-contract.py` / `.txt` | run `--workspace --lib`; filters lose the leading `<module>::` segment |
| `Dockerfile`, `release.yml` | font licence paths → `crates/dial-agent/src/snapcompact/fonts/` |
| `include_str!` sites | `themes/*` (31, `ui/theme.rs`), `runtimes/browser/*` (16, `tools/browser`), `python/*` (2, `setup.rs`): relative depth changes |
| `tests/integration/architecture.rs` | drop the cross-module boundary rules (Cargo enforces them); keep `tests_live_in_their_own_files` and scan `crates/*/src` |
| `ARCHITECTURE.md`, `docs/*.md` | module table → crate table; dangling links (`docs/architecture.md`, `guide.md`, `llms.md`, `readme.md`, `x.md` are referenced but absent) fixed; audit notes moved to `docs/audits/` |
| `.gitignore` | add `/node_modules/`, `/browser/` |
| `deny.toml`, `flake.nix`, `Formula/dial.rb` | no change (read root `Cargo.toml`) |

## 6. Migration order

Every phase ends with the tree green and one commit; nothing moves until the previous phase is verified.

| Phase | Work | Verification |
|---|---|---|
| 0 Hygiene | delete `browser/orphans/*`, `package-lock.json`; ignore `node_modules/`, `/browser/`; `docs/audits/`; fix dangling doc links; remove `settings_schema` alias; move the two crate-root test files under their modules; replace `main.rs` `#[path]` mounts with `src/bin_cmds/` | `cargo check --all-targets`, `cargo test --test integration` |
| 1 Cycle cuts | the §3 table, one commit per row group (config types; runtime shared types; llm/storage; steering/reliability splits; registries); add an `architecture.rs` rule per cut | `cargo check --all-targets`, `cargo test --lib <touched modules>`, `cargo test --test integration architecture` |
| 2 Workspace shell | root manifest becomes workspace + package; `[workspace.dependencies]`; no file moves | full `make test-rust`, `python3 scripts/test-build.py`, `scripts/check-packaging.py` |
| 3 Extraction, bottom-up | per crate: `git mv`, new `Cargo.toml`, `pub use dial_x as x;` facade line, widen `pub(crate)` only where the compiler reports it, `build.rs` split when `platform`/`llm`/`computer` move | after each crate: `cargo check --workspace --all-targets`, `cargo test -p <crate>`, `cargo test --test integration`; slim check (`--no-default-features --bin dial`) when `voice`/`embedded-shell`/`computer` move |
| 4 Surfaces | Makefile, CI, windows-contract, Dockerfile/release paths, ARCHITECTURE.md, docs | `cargo fmt --check`, `cargo clippy --workspace --all-targets` with the CI gate, `make test-smoke DIAL=target/debug/dial`, `cargo build --release --locked` (build.rs release assertions), `cargo deny check` |
| 5 In-crate grouping | sub-domain directories inside `dial-tools` and `dial-agent`; split `app.rs` by concern | `cargo test -p dial-tools -p dial-agent -p dial-app`, integration suite |

Rollback unit is one commit; phases 0–2 leave the tree a single crate, so abandoning after any of them still yields a strictly better layout.

### Execution log

| Commit | Phase | Verified with |
|---|---|---|
| `d382c68` | pre-work: inline tests → sibling files | check --all-targets, fmt |
| `d98543c` | 0 hygiene (orphans, lockfile, ignores, `docs/audits/`) | doc-only; links grepped |
| `afecded` | 1a configuration layer: config owns types/defaults/settings_store/paths; atomic → platform; `settings_schema` alias removed; ui allowlist rule | fmt, check --all-targets, slim bin check, `--lib config` (163), integration 306 (skip below) |
| `019a887` | 2 workspace shell: `[workspace]`, `[workspace.package]`, `[workspace.dependencies]`; packaging scripts read the workspace version | `cargo tree` identical for macOS/Linux/Windows × default/none/all features; lock unchanged; packaging tests |
| `9b08c3e` | 1b remaining back-edges: llm::{steering,reliability,wire estimate}, platform::{procinfo,process_record}, runtime::{tool_output,schema,clock,arguments}, storage::{repair_evidence,transcript,artifact}, tools::{codebase,history,provenance}; `BoundSession` trait; `Registry::standard(config, native)` takes the computer group; `ToolStatus::from_output` beside its type | fmt, check --all-targets, slim bin check, `fold.py` empty, per-module `--lib` subsets, integration 306 (skip) |
| `f6c42062` | 3 extraction: `dial-build-info` (build_info; build script takes the revision/dirty/epoch half of the root build.rs) | integration 308 (skip small_window); architecture guard scans every `crates/*/src` root and checks layers |
| `176435d2` | 3 extraction: `dial-core` (platform, runtime, output, workspace_copy; hex, install_tls_provider, flush_stdout_on_exit) | compiler-driven visibility widening; test-support feature for cross-crate test helpers |
| `abc6aadf` | 3 extraction: `dial-llm` (config, llm, url); Apple Foundation Models build step | integration 308 incl. two new architecture tests (skip small_window) |
| `689ede8d` | 3 extraction: `dial-storage` (storage) | lib dial-storage 245 passed; integration 308 (skip small_window) |
| `3cb30456` | 3 extraction: `dial-tools` (tools, embedded_shell, browser_relay, setup, voice, security); python/runtimes/assets paths gain `../../` | lib dial-tools 1110 passed; integration 308 (skip small_window) |
| `b5a10111` | 3 extraction: `dial-computer` (computer); build script takes the desktop cfg and macOS capture helper | dial-computer lib tests; integration 308 (skip small_window) |
| `73ccef74` | 3 extraction: `dial-agent` (agent, learn, judgment, adaptive, steering, reliability, telemetry, stats, extensions, commit, pack) | lib dial-agent 1002 passed; integration 308 (skip small_window) |
| `4793592a` | 3 extraction: `dial-ui` (ui, git_panel; `themes/` moves to `crates/dial-ui/themes`) | test-support helpers behind feature; visibility widened by compiler report |
| `f9f4d080` | 3 extraction: `dial-app` (app, commands, cli, benchmark, repl, rpc, acp, schedule, changelog, collab); root keeps bin and facade | facade re-exports keep `dial::<module>` paths |
| `c6d6f4d2` | 3 binary-only command modules (benchmark, improve, update) move from `src/cli` into `src/cmd` | not recorded in the commit |
| `10edeceb` | 3 tighten crate boundaries: narrow visibility the extraction widened | not recorded in the commit |
| `7b67cd33` | test hygiene: plan-file tests keep state inside a temporary dir | not recorded in the commit |
| `3affaecf` | 4 surfaces: build, CI and docs updated for the workspace layout | not recorded in the commit |
| `1c68fcf2` | 5 in-crate grouping: tools into sub-domain modules (exec, edit, fs, code, net, vcs, integrations, knowledge, surface) | not recorded in the commit |
| `d3fc82c1` | 5 in-crate grouping: agent into sub-domain modules (run, model, window, compact, verify, safety, workers, workflow, memory, ui, base; `modes/` stays at root) | not recorded in the commit |

### Completion pass: physical ownership and binary separation

The crate extraction above is retained. This pass finishes specific in-crate ownership mismatches
without creating more crates, changing the layer order, or rewriting runtime behavior.

| Old location | Destination / change | Reason and preserved contract |
|---|---|---|
| Root `src/main.rs` declarations and handlers | `src/{args,overlay,dispatch,chat}.rs`, command implementations in `src/cmd/`, parser contracts in `src/args/tests.rs` | Entry point owns startup, not every command; CLI flags and exit behavior stay unchanged |
| `dial-core/src/platform/windows_*.rs` and their test directories | `platform/windows/{console,fs,io,process,pty}.rs` with matching test children | Physical paths now match existing private logical modules |
| Inline portable and native platform modules | `platform/{fs,process,io,time,sys}.rs`, `platform/unix/{fs,process,io,sys}.rs` and `platform/windows/{time,sys}.rs` | Separate concerns symmetrically without changing portable APIs |
| `dial-llm/src/llm/compaction.rs` and tests | `llm/provider/compaction.rs` and `provider/compaction/tests.rs` | Compaction already belongs logically to the provider |
| `dial-llm/src/config/settings/` schema declaration files | `config/schema/{compaction,experimental,improvement,memory,model,tools,voice}.rs` | Stop mixing schema children with the settings module's own tests |
| `dial-app/src/commands/` | `dial-app/src/app/commands/` | Preserve private `app::commands` ownership and visibility rather than promoting an internal API |
| `dial-tools/src/tools/edit/edit_transaction_{unix,windows}.rs` | `edit_transaction/{unix,windows}.rs` | Keep native implementations beneath the transaction owner |
| `dial-agent/src/learn/processes_windows.rs` | `learn/processes/native.rs` | Preserve the existing cfg-gated native module path |
| Redundant `#[path]` attributes in catalog, schema, snapcompact and tests | Ordinary module declarations | Let Rust resolve conventional paths directly |

Automation: extend the integration architecture guards to reject ambiguous module definitions and
require the workspace's discovered crates and layer registry to agree in both directions. Existing
dependency-boundary and separate-test-source checks remain enabled.

Cleanup completion: embedded browser/eval/kernel drivers and their protocol tests now live beneath
their Rust owners (`tools/net/browser/`, `tools/exec/{eval,kernel}/`); compile-time includes follow
the moves. `make test-drivers` runs the existing browser, eval and Python kernel contracts in CI.
Unused root Tokio and workspace semver declarations are removed; libc, rusqlite and parking_lot
are test-only dependencies of the root package. The redundant update-test path mount is removed.
Generated browser/npm leftovers, Python caches and nine obsolete target experiment directories
were deleted with explicit approval. Active builds, `.tmp`, local toolchains, private state,
worktrees and unfinished plans are intentionally preserved.

Verification is performed after all parallel slices are integrated: workspace all-target compilation,
slim binary compilation, architecture and CLI contracts, affected crate tests, formatting and packaging
metadata checks. Native Windows-only checks require the installed cross-toolchain; runtime behavior
on another OS is not established by host tests. Actual results are reported separately, not inferred
from this plan. Historical audit citations remain historical rather than being rewritten as new evidence.

Test aborts ("fatal runtime error: failed to initiate panic, error 5") are an environment
defect, not a code regression: the tracked `.cargo/config.toml` passes
`-C link-arg=-Wl,-no_compact_unwind` on macOS, and with Homebrew rustc 1.99 that breaks
unwinding: a one-line `catch_unwind` test aborts with the flag and passes without it. Every
`should_panic`/containment test therefore killed the test binary and the full `--lib` run. Tests run
with `RUSTFLAGS="-Cdebuginfo=0"` (which replaces the target rustflags) and a dedicated
`CARGO_TARGET_DIR`. The config flag itself is left for the owner to decide.

With that override, `9b08c3e` gives full `--lib` 4367 passed / 0 failed / 16 ignored and integration
306 passed / 1 failed. The failure is the real assertion
`agent::small_window_completes_first_turn_with_on_demand_tools` (no "lean tool set" notice for an
8192 window; it looks like the compaction-failure path in `agent/turn.rs` preempts the lean-tool
switch). It fails identically at `019a887` and on the pre-refactor baseline, so it is out of scope
for this refactor, and phase checks `--skip` it.

### Phase 3 hazards measured before extraction

- **Assets:** 35 `include_str!/include_bytes!` sites leave `src/`: `python/*` (agent/laya, llm/server,
  setup), `assets/browser-extension/*` (browser_relay), `runtimes/browser/*` (tools/browser),
  `CHANGELOG.md` (changelog), `examples/benchmarks/efficiency.toml` (benchmark tests),
  `themes/*` (31, ui/theme). Each gains `../../` when moved under `crates/<c>/src`; `themes/`
  moves to `crates/dial-ui/themes/`.
- **`CARGO_MANIFEST_DIR`:** four test-only uses (benchmark, learn/worker, llm/provider/real_server, llm/server)
  assume the repo root → `join("../..")`.
- **`CARGO_PKG_VERSION`:** read in 9 modules across crates → every member uses `version.workspace = true`.
- **`cfg(test)` helpers:** ~100 `pub` items across modules; those used by another crate's tests become
  `#[cfg(any(test, feature = "test-support"))]`, enabled through the dependent's `[dev-dependencies]`.
- **Crate-root helpers:** `hex`, `install_tls_provider`, `flush_stdout_on_exit` move to the `dial-core` root
  and are re-exported by each crate root that needs `crate::hex`.
- **Import model:** each crate root has `pub(crate) use dial_lower::{module, …};` per lower module it
  uses, so `crate::platform::…` paths keep resolving without touching call sites; the facade
  `src/lib.rs` swaps `pub mod x;` for `pub use dial_c::x;`.

## 7. Risks

- **`pub(crate)` widening** (1,029 such items): only those crossing a new crate boundary change; the compiler enumerates them. Items made `pub` get a doc line stating the consumer.
- **Test helper visibility**: `llm::fake` must become a `testing` feature; any other `cfg(test)` helper used across a boundary surfaces as a compile error in Phase 3 and gets the same treatment.
- **Platform-specific code**: Windows/Linux paths compile only in CI; each Phase 3 commit should pass CI before the next extraction.
- **Build-time env vars**: a crate reading an `env!` its own `build.rs` does not emit fails to compile, so misplacement cannot ship silently.
- **Lock/profile drift**: `--locked` rejects a stale lock; the local profile override list must name every member or local builds of that crate silently get `opt-level = 2`.
- **Incremental rebuild wins depend on the DAG staying thin**: `agent` and `tools` remain the largest crates; Phase 5 grouping does not change that, a further split of `tools` (e.g. browser/lsp/dap/mcp as crates) is a possible follow-up, not part of this plan.

## 8. Open decisions

1. Crate naming: `dial-<module>` with `crates/` directory (proposed) vs. bare module names.
2. `collab`, `voice`, `embedded-shell`, `security`, `rpc`, `commit` as separate small crates (proposed; cleaner features and ownership) vs. folding into neighbours to reduce the crate count from 20.
3. `themes/` into `dial-ui` (proposed) vs. keeping all embedded assets at the root next to `runtimes/` and `python/`.
4. Whether Phase 5 (sub-domain grouping, `app.rs` split) is in scope for this effort or a follow-up.
