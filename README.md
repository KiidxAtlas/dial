# dial

A local-first coding agent harness for macOS, Linux and Windows (native or through WSL2). Run it in a project directory like any agent CLI. Learning is optional and separate: `dial improve` runs verified practice, harness proposals and local LoRA training behind an external evaluation gate. Ordinary coding sessions do not open a learning store, collect training trajectories, apply learned candidates or launch training/evaluation workers, unless you turn on **Teach this model** for a local model (`/settings` → Model → Learning). Local MLX inference and training require Apple Silicon; Intel Macs and Linux/WSL2 use hosted or OpenAI-compatible providers.

Standalone: no dependency on any external agent harness or hosted service. Rust control plane and tools; Python for local MLX inference/training (`mlx-lm`, plus `mlx-vlm` for vision).

Inspect the executable you are actually running with `dial --version` (compiled revision and
whether its compiled sources differed from that revision at build time) or `dial --build-info` (JSON including target, profile, build time and
actual executable path). This does not read runtime configuration or contact a model. Builds
without Git metadata report `unknown`; release/archive builders can set `DIAL_BUILD_REVISION`
and `SOURCE_DATE_EPOCH` explicitly. Rebuild/restart to pick up source changes.

This README is an index. Detailed documentation lives in [`docs/`](docs/) and the module layout,
dependency boundaries and invariants are in [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Install

Prebuilt releases target **macOS 13+ (Apple Silicon or Intel)**, **Linux x86-64 (Ubuntu 22.04 or
newer, also under WSL2)** and **native Windows x86-64**. No Rust toolchain is needed for a release.

macOS and Linux:

```sh
curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | bash
```

The installer adds `~/.local/bin` to your shell profile when it is not already on `PATH`
(`DIAL_NO_MODIFY_PATH=1` skips this). It checks the archive's SHA-256 against the release's
`SHA256SUMS`, and also verifies that file's Ed25519 signature when OpenSSL 3 is available
(`brew install openssl@3` on macOS; `DIAL_REQUIRE_SIGNATURE=1` makes the signature mandatory).
Windows (PowerShell 5.1 or 7; no OpenSSL or admin rights needed):

```powershell
irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 | iex
```

Or use the Linux commands inside WSL2. See [docs/windows.md](docs/windows.md) for pinned versions and next steps.

- **Linux/WSL2** needs `bubblewrap` and `ca-certificates`, working unprivileged user namespaces, and
  a normal (non-root) user. Under WSL2, **Windows executable interop must be disabled** in
  `/etc/wsl.conf`; Dial refuses sandboxed commands while it is enabled.
- **Local MLX inference and training** require macOS on Apple Silicon plus `uv`; hosted and
  OpenAI-compatible providers work everywhere without Python.
- **From source:** `make update` builds and installs the faster, incremental `local` profile
  from a checkout; `make update SLIM=1` omits optional integrations. Use
  `cargo install --path . --locked` when you specifically want shipping optimization.

Manual installation per platform, WSL2 interop settings, the Linux sandbox model, build-from-source
details, the development loop and release publishing are in
[docs/release-and-install.md](docs/release-and-install.md#platform-installation-details). Native
Windows behavior is in [docs/windows.md](docs/windows.md).

## Quick start

```sh
cd ~/code/my-project
dial                                        # terminal UI; first run opens model/provider setup
dial "fix the failing test in src/lib.rs"   # interactive, opening with a message
dial -p -y "summarize this repo"            # one-shot; prints the answer (-y or --approval auto for changes)
dial -c                                     # continue the latest session (-r <id-prefix> to pick one)
```

Useful first commands inside the UI: `/providers` (sign in or add an endpoint, including a Claude or
ChatGPT/Codex subscription), `/models` and `/roles` (choose models), `/settings` (configuration
hub), `Ctrl-P` (action palette), `/help` and `/hotkeys`. See
[docs/terminal-interface.md](docs/terminal-interface.md) for shortcuts and panels.

## Configuration essentials

Configuration is layered TOML: `~/.dial/config.toml`, then the private per-project file
`~/.dial/projects/<name>-<hash>/config.toml`, then command-line flags. Dial never reads
configuration from the project directory itself. Most settings are also editable in `/settings`,
which saves globally or per project (`Ctrl-S`).

```toml
model = "mlx-community/Qwen2.5-7B-Instruct-4bit"
approval = "write"        # ask | write | auto | readonly
[sandbox]
mode = "dev"              # dev | strict | off
[endpoint]                # optional: OpenAI-compatible server instead of local MLX
base_url = "http://127.0.0.1:1234/v1"
api_key_env = "LMSTUDIO_KEY"
[model_roles.small]       # helper roles fall back to the main model when unassigned
provider = "openai"
model = "gpt-4o-mini"
```

Most execution limits default to `0`, meaning unlimited. The full annotated example, layering rules
and unlimited defaults are in [docs/configuration.md](docs/configuration.md); model roles and
fallback chains are in [docs/model-routing.md](docs/model-routing.md).

## Documentation

**Getting started and everyday use**

- [Release and installation](docs/release-and-install.md): installers, manual platform setup, signing, building from source, publishing releases
- [Configuration](docs/configuration.md): layers, unlimited execution defaults, annotated example
- [Terminal interface](docs/terminal-interface.md): composer, slash commands, steering and queueing, shortcuts, `/settings`, model settings, themes, image previews
- [Composer and editor](docs/composer.md), [TUI rendering](docs/tui-rendering.md), [Voice](docs/voice.md), [Windows](docs/windows.md)

**Models and providers**

- [Provider and model catalog](docs/provider-catalog.md): discovery, local MLX/LM Studio/Ollama/llama.cpp, API keys, Claude and ChatGPT/Codex subscriptions, `/compute`, cost accounting
- [Model routing](docs/model-routing.md): model roles, picker role assignment, retries and fallback chains
- [LLM transport](docs/llm-transport.md): model profiles, protocols, prompt caching, request headers, retries, image input

**The agent and its tools**

- [Agent loop and context maintenance](docs/agent-loop.md): coding workflow, verification and completion, shared context economy, human questions and checkpoints, compaction policies, live phase policy
- [Tools, safety and context](docs/tools.md): tool roster and integrations, approvals and sandbox, context sources, [editing grammars](docs/tools.md#editing-grammars-and-safety), tool reliability
- [Execution and web tools](docs/execution-and-web.md): kernels, browser, web access, DAP
- [Shell tool](docs/shell.md), [Terminal shell](docs/terminal-shell.md), [SSH](docs/ssh.md)
- [Subagents](docs/subagents.md): delegation, typed roles, isolation, live tasks, worker panel

**Extending and automating**

- [Extensions and project context](docs/extensibility.md): skills import and toggles, custom tools, Rust SDK, [hooks, path permissions and schedules](docs/extensibility.md#lifecycle-hooks-path-permissions-isolated-workers-and-schedules), [git tool, budgets, custom agents and HTTP MCP](docs/extensibility.md#git-tools-budgets-custom-agents-and-http-mcp)
- [RPC and ACP](docs/rpc-and-acp.md): print mode JSON output, session CLI, persistent RPC, ACP
- [Background services](docs/background-services.md), [Encrypted collaboration](docs/collab.md), [Sharing, statistics and telemetry](docs/share-stats-telemetry.md)

**Memory, learning and measurement**

- [Memory and background learning](docs/memory-and-learning.md): memory backends, saved codebase understanding (`/understanding`), architecture synthesis, experience reuse
- [Optional learning and harness improvement](docs/improvement.md): `dial improve` cycles, LoRA and fast weight candidates, verified local-model workflows, repository exercises, activation steering, portable packs
- [Benchmarks and measurement](docs/benchmarks.md): `dial bench`/`dial benchmark` suites, efficiency measurement, regression checks, configuration comparisons, assistance ablation
- [Autoresearch and archive](docs/autoresearch-and-archive.md)

## Known limits

- Tool-calling quality depends on the model: 7B-class instruct models handle the tool set; sub-1B models often emit unreadable calls (which become training signal rather than being hidden).
- Local compute is serialized across projects by default. Disable **Compute locks** in Settings → Model → Local worker (`compute_locks = false`) to allow concurrent model work; this can exhaust RAM/GPU memory. Laya roles never require the compute lock. With locks enabled, a session that queues improvement releases compute when idle and its child agents have finished; a subsequent turn waits if the cycle holds compute. There is no claim of zero-downtime training.
  While waiting for compute, the elapsed timer advances and commands follow the same live/deferred/interrupting policy as during inference. Turn duration includes that wait; acquiring compute returns the status to Working rather than leaving it marked as waiting.
- The TUI is keyboard-driven; not every terminal supports modified Enter keys. `dial schedule` provides scheduling but no daemon installer. Voice (text-to-speech, push-to-talk dictation and protocol live sessions) is described in `docs/voice.md`. Browser and kernel require available OS containment; they fail closed rather than falling back to unrestricted execution.
- Live memory/skill edits are the user's working state and are not gated; only the admitted harness is.
- Operator tools are trusted programs and are not sandboxed during live work; they are excluded from automatic evaluation. Non-macOS: `bash` is unavailable unless `sandbox.mode = "off"` for live use, while generated evaluations still require the strict sandbox.

## Source layout

```
src/agent/    agent loop, system prompt, project context, todo, typed subagents, approvals
src/tools/    Workspace + built-ins (files, search, shell, hashline, sandbox) and pluggable tools
              (ext.rs registry: bg, web, lsp, ast, mcp)
src/llm/      backends, providers, catalog, auth, local MLX server, tool-call parsing
src/learn/    self-improvement: curriculum, harvest, mutate, probe, gate, worker, verify
src/storage/  sessions, memory, skills, checkpoints, SQLite store, paths, settings
src/config/   configuration types and validation
src/ui/       terminal renderer, composer, themes, overlays
```

Adding a tool: implement `tools::ext::Tool` in its own module, return it from a `tools()` constructor, and list it in `Registry::standard`.
