# Documentation

New here? Follow the [Quickstart](quickstart.md). References are grouped by what you want to do,
not by crate ownership. In the terminal, `/help <topic>` prints a focused documentation path.

## Start here

- [Quickstart](quickstart.md) — Install, pick a model, get a fix reviewed and undo it, in five minutes.
- [Release and installation](release-and-install.md) — Install Dial, build from source and check or publish releases.
- [How Dial compares](comparison.md) — Dial's defaults and capabilities next to another open-source terminal agent.

## Everyday

- [Everyday Dial](everyday.md) — Start, steer, review, undo and resume ordinary coding work.
- [Running Dial in Docker](docker.md) — Run Dial with Docker, Compose and Docker Model Runner.
- [Windows](windows.md) — Set up and use Dial on Windows and WSL.
- [Terminal interface](terminal-interface.md) — Choose a focused terminal reference; legacy section links remain available.
- [Composer and transcript](terminal-composer.md) — Write and paste drafts; read the launch screen, status and transcript.
- [Commands and help](terminal-commands.md) — Find actions with slash completion, the palette and topic help.
- [Turns, steering and review](terminal-turns.md) — Steer and queue work; review outcomes and handle interruptions safely.
- [Sessions and history](terminal-sessions.md) — Resume, search, branch, export and share conversation history.
- [Keyboard shortcuts](terminal-shortcuts.md) — Find, customize and troubleshoot keyboard shortcuts.
- [Models and roles](terminal-models.md) — Select models and roles, manage favorites and download weights.
- [Terminal settings](terminal-settings.md) — Search settings, choose storage scope and review setup presets.
- [Terminal integration](terminal-integration.md) — Use terminal signals, clickable links and the live side pane.
- [Composer and editor](composer.md) — Understand composer editing, paste, completion and external editor behavior.
- [Terminal shell](terminal-shell.md) — Run user shell commands and manage terminal handoffs.
- [Voice](voice.md) — Dictate prompts and configure speech transcription.
- [Session coordination](session-coordination.md) — Exchange messages and advisory path claims between live sessions.
- [Encrypted collaboration](collab.md) — Connect collaborative sessions and understand host/guest controls.
- [Agent-requested directory access](directory-access.md) — Add working directories and understand workspace access boundaries.
- [Image providers](image-generation.md) — Generate and edit images through explicitly approved image tools.
- [Evidence investigations](evidence-investigation.md) — Investigate sources and retain evidence for follow-up work.

## Configure

- [Configuration](configuration.md) — Understand configuration layers, defaults and annotated settings.
- [Optional dependencies and guided setup](setup.md) — Inspect optional capabilities and install missing dependencies explicitly.
- [Provider and model catalog](provider-catalog.md) — Configure local and hosted model providers and account access.
- [Model routing](model-routing.md) — Assign model roles, routing policies and fallback chains.
- [Model downloads](model-downloads.md) — Acquire local model weights and choose quantizations.
- [Model embeddings for semantic memory](model-embeddings.md) — Choose embedding backends and configure semantic retrieval.
- [Vision models: grounding and image embeddings](model-vision.md) — Configure image-input support and vision model behavior.
- [Explicit paid provider batches](provider-batches.md) — Configure and run provider batch requests.
- [Background services](background-services.md) — Configure background service processes and lifecycle controls.
- [Optional learning and harness improvement](improvement.md) — Configure improvement, evaluation and promotion workflows.
- [Memory and background learning](memory-and-learning.md) — Understand persistent memory, training and learned adapters.
- [Extensions and project context](extensibility.md) — Extend Dial with tools, hooks and integrations.
- [Experimental source-investigation extensions](source-extensions.md) — Install and manage source-based optional extensions.
- [Writing and evaluating skills](skills.md) — Discover, enable and author reusable agent skills.
- [Shell tool](shell.md) — Configure shell execution and its safety boundaries.
- [SSH integration](ssh.md) — Configure remote hosts, authentication and remote workspace access.
- [Sharing, statistics, and telemetry](share-stats-telemetry.md) — Control transcript sharing, local statistics and telemetry.

## Internals

- [Everyday UX acceptance](ux-journeys.md) — Offline journey coverage, measurement boundaries and the separately approved real-service test plan.
- [Agent loop and context maintenance](agent-loop.md) — Trace agent execution, context maintenance, verification and completion.
- [Tools, safety and context](tools.md) — Reference tool contracts, schemas, safety and context sources.
- [Execution and web tools](execution-and-web.md) — Understand execution kernels, browser tools and web access.
- [Opt-in asynchronous execution](async-execution.md) — Understand background work, scheduling and result delivery.
- [Opt-in candidate and read overlap](async-candidates-and-reads.md) — Understand asynchronous candidate exploration and read behavior.
- [Approved application monitoring](application-monitoring.md) — Inspect application monitoring and operational signals.
- [Autoresearch and archive](autoresearch-and-archive.md) — Understand automated research and archive workflows.
- [Local benchmark improvement batch](benchmark-measurements.md) — Interpret benchmark measurements and their limitations.
- [Benchmarks and measurement](benchmarks.md) — Run benchmark suites and inspect evaluation reports.
- [Surface inventory, November 2026](plans/surface-inventory-2026-11.md) — Review experimental flags and optional modes with the evidence available for each.
- [Client compatibility and conformance](client-contracts.md) — Understand client-facing contracts and protocol behavior.
- [Public API compatibility](compatibility.md) — Review retained Rust exports, deprecations and migration paths.
- [Experimental evidence foundation](evidence.md) — Understand evidence identity, retention and verification boundaries.
- [LLM transport behavior](llm-transport.md) — Inspect model protocols, request profiles, caching and retries.
- [Requirement-aware evidence](requirement-verification.md) — Understand requirements, checks and recorded acceptance evidence.
- [RPC and ACP](rpc-and-acp.md) — Integrate with JSON-RPC and Agent Client Protocol interfaces.
- [Rust focused-test selection](rust-focused-tests.md) — Select focused Rust verification for changed modules.
- [Opt-in session preparation](session-preparation.md) — Understand startup, session preparation and restoration.
- [Stream rules](stream-rules.md) — Understand streaming behavior and request/response rules.
- [Subagents](subagents.md) — Understand delegated work, worker state and lifecycle controls.
- [Experimental system investigations](system-investigation.md) — Investigate runtime behavior and cross-system boundaries.
- [Tool-effectiveness evaluation](tool-effectiveness.md) — Interpret tool usage, effectiveness and reliability signals.
- [TUI rendering](tui-rendering.md) — Understand terminal rendering, frame pacing and transcript updates.

## Known limits

- Tool-calling quality depends on the model and workload. Small local models can produce broken
  calls; if results are poor, try a larger model. Parameter count alone does not guarantee reliability.
- Local MLX inference and training require Apple Silicon. Intel Macs and Linux/WSL2 use hosted or
  OpenAI-compatible providers, or GGUF through llama.cpp.
- Local compute is serialized across projects by default; see **Compute locks** in
  [Provider and model catalog](provider-catalog.md). Dial makes no claim of zero-downtime training.
- Not every terminal supports modified Enter keys. `dial schedule` handles scheduling but does not
  install a daemon.
- Live memory and skill edits are your working state and are not gated; only the admitted harness is.
- Operator tools are trusted programs and are not sandboxed during live work; they are excluded from
  automatic evaluation. `bash` runs unconfined by default. With `sandbox.mode = "dev"` or
  `"strict"` on macOS and Linux/WSL2 it runs inside the OS sandbox (Linux needs bubblewrap and
  unprivileged user namespaces, and refuses to run commands without them rather than falling
  back). Windows supports `dev` through Low integrity, but cannot confine general reads or network
  access and refuses `strict`; see [Windows security](windows.md#security). Browser and `eval`
  containment requirements are platform-specific; see [execution tools](execution-and-web.md).
  Generated evaluations always require the strict sandbox.

See also [Architecture](../ARCHITECTURE.md) for crate ownership and invariants, and
[contributor notes](../AGENTS.md) for repository conventions and checks.
