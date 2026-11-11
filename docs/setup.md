# Optional dependencies and guided setup

Optional features (voice, language servers, the JavaScript and browser runtimes, embeddings) need
outside software. Dial sets it up on request, never at startup.

## First launch and model readiness

First launch offers **Hosted provider** and **Local model** explicitly. Hosted opens the existing
Providers panel for sign-in/API keys; Local opens Models for a running endpoint or downloaded model.
Esc backs out of a panel to the path choice; Esc or Cancel at the choice exits without selecting a
model. Browsing is not permission to install software, download weights, or send an inference request.
Provider/model panels may discover endpoint metadata using their existing catalog checks.

Before asking, first launch checks for local inference servers that are already listening at their
usual addresses (LM Studio, Ollama, MLX-Serve, llama.cpp, Docker Model Runner, Osaurus; the
`*_BASE_URL` variables and `OLLAMA_HOST` override them). It names the ones it finds and highlights
**Local model**, so Enter opens Models. This is a TCP connection check only: it does not list
models, send a request or select anything, and Esc still exits without choosing. When no server is
listening on Apple Silicon, it instead suggests the largest MLX coding model in the `dial models
recommend` list (7B and up) that fits physical memory, and highlights **Local model**. That is an
estimate by RAM tier, not a measurement, and nothing downloads until you confirm in **Get models**.

Interactive **`/doctor`** leads with core model readiness, then optional capabilities. It distinguishes
configuration, cached/unknown endpoint metadata, and session-only response evidence. Open Models to
change the selection or Providers to inspect the credential source and check metadata. A configured
model or successful catalog response is **not** proof that inference works.

**Verify selected model response** is available in Doctor, the local-model checklist and Providers
(it always tests the current chat selection, not merely the highlighted provider). It first shows a
default-Cancel confirmation: one tiny prompt, no tools or conversation history, up to 32 output tokens
and a 20-second diagnostic budget. Hosted requests use the network and may incur provider charges;
local requests may load weights/use compute. Only explicit confirmation sends the request. The normal
interrupt key cancels a running check; no request is queued during another turn. Failure, cancellation
or an empty response never count as verified. A nonempty response verifies text inference only, not
tool support or quality. Evidence is in-memory for this session and exact configuration; changing the
model, endpoint or configuration makes old evidence unknown. Provider errors are reduced to actionable
categories rather than exposing raw server bodies, endpoint credentials or authentication headers.
HTTP status hints match whole numeric codes, not digit substrings in local endpoint ports;
a refused connection on port `40123`, for example, is an endpoint failure, not an authentication failure.

Managed-local checks reuse Dial's existing compute owner. After confirmation, idle resident models
are released before the diagnostic acquires the existing compute lock exclusively. Active child
agents, another session/worker holding compute, or disabled compute locking cause a refusal rather
than a competing model load. Wait for the owner to finish, then explicitly retry from Doctor.
This path has isolated endpoint tests; it is not a claim that every installed local runtime was tested.

**Check coding/tool compatibility** is a separate opt-in action in Doctor, the local-model checklist
and Providers. Its default-Cancel review discloses up to two requests, 128 output tokens each and a
20-second overall budget, with the same hosted charges/local compute warning. It sends only synthetic
messages and an echo schema: no conversation history, project files or real tool execution. Outcomes
distinguish text-only replies, invalid invocations, structured versus text-parsed invocations, and
whether the model consumed a synthetic result and returned its receipt. An invocation without a
successful continuation is not a verified roundtrip. Evidence is session-only and bound to the exact
configuration, separate from the existing text-response test. Interrupt cancels; busy sessions do not
queue requests. Passing this small protocol check is not a coding-quality or universal tool guarantee.

## Where setup happens

- **Settings** (`/settings`): a row whose program is missing shows a muted `○ available` line with
  `Ctrl+L installs <name>` under it. Those lines are information, not errors. Press Ctrl+L once to
  see the exact plan (the commands and where they install), then Ctrl+L again to run it in the
  background. Save errors stay red.
- **`/setup`**: opens a searchable guide in the terminal. Choose a capability, **Connect a model
  provider**, or **Set up semantic memory embeddings**. Capability rows show installed/missing
  status; the embedding picker shows each model's download size, dimensions and description.
  Selecting a capability or model only reviews its full existing setup plan. The plan appears
  in the transcript and in a scrollable confirmation (Page Up / Page Down for long plans).
  **Cancel** is selected by default; choose **Install and save settings** explicitly to start
  the background job. Esc, Ctrl+C, or Cancel before installation changes nothing. Browsing
  never downloads software or models. Provider selection opens the existing Providers panel.
- **Text and direct commands**: `/setup list` lists installed and missing capabilities as text.
  `/doctor` opens core readiness followed by the same optional-capability diagnostics.
  `/setup <id>` still only previews; `/setup <id> yes` explicitly runs it.
  `/setup embeddings` still prints the model catalog; `/setup embeddings:<model>` previews,
  and `/setup embeddings:<model> yes` pulls and assigns that model. Custom Ollama model names
  remain supported through these direct commands. `/setup grounding` and
  `/setup image-embeddings` list the curated vision models; `/setup <id>:<repo>` previews and
  `/setup grounding yes` / `/setup image-embeddings yes` download the default MLX checkpoint
  and assign its role (see [model-vision.md](model-vision.md)). `/setup providers` opens
  Providers directly.
- **Shell**: `dial doctor` lists what is installed (✓ with its path) and counts what is not;
  `dial doctor --all` lists every missing capability with its `dial doctor --fix <id>` command. Both are followed by a Memory section (the selected backend, what it
  still needs, and each backend's readiness). Select a backend without the TUI with
  `dial config set memory.backend <off|provenance|semantic|local|sharpshooter|hindsight>`.
  `dial doctor --fix <id>` previews and ends with the exact `--yes` command that runs it.

## What a setup may do

Each recipe tries methods in order and stops at the first one whose tool is available:

1. npm into `~/.dial/tools/<id>` for private runtimes such as Bun;
2. `uv tool install` into `~/.dial/tools/<id>` for Piper and mlx-whisper;
3. Homebrew, with no sudo (installs into Homebrew's own prefix);
4. a direct download of the project's own release archive for Node.js, Bun and uv, when none of
   the above is available.

Downloads are explicit: they appear in the plan, show progress, can be cancelled, and are refused
unless the archive's SHA-256 matches the digest pinned in `crates/dial-tools/src/setup/deps/prebuilt.rs` (Node.js
22.23.3, Bun 1.4.3, uv 0.13.1; darwin and linux, x64 and arm64). Archives unpack into a staging
directory; entries with absolute paths, `..`, hard links, or symlinks that leave the directory are
rejected. Nothing runs with sudo.

Prerequisites for private runtimes are installed first (for example uv for mlx-whisper or Piper).

### Automatic language servers

With the `lsp` integration enabled, the first approved LSP request detects the language from
the file type and project markers, reuses an existing server, or installs a missing built-in
server with a known recipe. Startup and installation remain approval-gated; enabling the
integration does not install every server at launch. Custom server commands are never replaced.

Language servers use their package manager's normal locations, not the project or Dial storage:
`npm install --global` respects npm's configured prefix, `uv tool install` respects uv's tool
directories, `go install` respects `GOBIN`/`GOPATH`, rustup uses the selected Rust toolchain,
and Homebrew uses its own prefix. Dial does not override these destinations or use sudo.
An existing npm, uv, Go, rustup or Homebrew installation is required for the corresponding
method; missing managers and unsupported recipes produce actionable errors instead of
downloading a private language-server runtime. Bins outside `PATH` are resolved through the
package manager after approval and used directly. Existing project-local and older Dial-managed
servers remain usable; no existing installation is moved or removed.

Local model runtime (`dial setup -m`) and Hindsight are manual: Dial shows the hint, not an install.
Ollama uses Homebrew when present and otherwise points to https://ollama.com/download.

The guide does not offer installation for already-installed capabilities or for blocked/manual
recipes; it displays the existing status or prerequisite instructions instead. Embedding setup
also respects Ollama's installation prerequisites. A successful embedding setup assigns the
model to `model_roles.embedding` and enables semantic embedding when it was off; explicit
embedding URL/model overrides still take precedence. Setup jobs retain the existing progress,
cancellation and error reporting. Cancelling a job after it starts may leave software or model
data already downloaded; cancelling the review never starts a job.

## Keeping external runtimes and tools current

Interactive Dial checks installed external tools in the background at most once per day.
Checks are read-only: they do not install packages, change models, accept provider terms,
or update Dial or project lockfiles. Offline/strict-sandbox sessions skip these checks.
Failed attempts are also throttled so an offline machine does not retry on every UI tick;
the transcript shows failures rather than claiming the tool is current.

- `/setup updates` checks now, bypassing the daily throttle.
- `/setup update <id>` reviews the update commands and destination.
- `/setup update <id> yes` explicitly approves that tool's update as a background job.

Updates never run merely because a check found a newer version. Review the plan first,
especially for Homebrew: changing a global formula can affect software outside Dial.
Unmanaged/custom installations and unsupported update sources show manual guidance instead
of being replaced. Updates do not silently downgrade a newer runtime to a bundled fallback.
Private npm packages are checked against their published stable versions; Homebrew tools
are checked against formula metadata only after ownership of the active binary is verified.
Private release-archive runtimes report newer upstream releases for manual review rather
than downloading unreviewed archives. MLX stays on Dial's embedded compatibility pins,
not arbitrary latest PyPI versions. Browser helpers use their owner's tested manifest;
independent Playwright/Chromium upgrades are not supported. Other custom/uv/Go/rustup
installations report manual guidance, not a guarantee that they are current.
Restart Dial after updating an active runtime so the next process uses the new version.

## Local providers that are not running

A configured local provider (Ollama, LM Studio, llama.cpp or any loopback endpoint) that refuses
the connection shows `○ not running` in muted text, not as an error. Auth failures, HTTP errors and
malformed replies stay red, because they point at a configured server that is misbehaving.
