# Optional dependencies and guided setup

Optional features (voice, language servers, the JavaScript and browser runtimes, embeddings) need
outside software. Dial sets it up on request, never at startup.

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
  `/doctor` is an alias for the same guided `/setup` flow. `/setup <id>` still only previews;
  `/setup <id> yes` explicitly runs it.
  `/setup embeddings` still prints the model catalog; `/setup embeddings:<model>` previews,
  and `/setup embeddings:<model> yes` pulls and assigns that model. Custom Ollama model names
  remain supported through these direct commands. `/setup grounding` and
  `/setup image-embeddings` list the curated vision models; `/setup <id>:<repo>` previews and
  `/setup grounding yes` / `/setup image-embeddings yes` download the default MLX checkpoint
  and assign its role (see [model-vision.md](model-vision.md)). `/setup providers` opens
  Providers directly.
- **Shell**: `dial doctor` reports the same list (✓ installed with its path, · not installed with the
  `dial doctor --fix <id>` command), followed by a Memory section (the selected backend, what it
  still needs, and each backend's readiness). Select a backend without the TUI with
  `dial config set memory.backend <off|provenance|semantic|local|sharpshooter|hindsight>`.
  `dial doctor --fix <id>` previews and ends with the exact `--yes` command that runs it.

## What a setup may do

Each recipe tries methods in order and stops at the first one whose tool is available:

1. npm into `~/.dial/tools/<id>` (for example Bun and the language servers; they use the npm that
   comes with a Dial-installed Node.js);
2. `uv tool install` into `~/.dial/tools/<id>` (Piper, mlx-whisper);
3. Homebrew, with no sudo (installs into Homebrew's own prefix);
4. a direct download of the project's own release archive for Node.js, Bun and uv, when none of
   the above is available.

Downloads are explicit: they appear in the plan, show progress, can be cancelled, and are refused
unless the archive's SHA-256 matches the digest pinned in `crates/dial-tools/src/setup/deps/prebuilt.rs` (Node.js
22.23.3, Bun 1.4.3, uv 0.13.1; darwin and linux, x64 and arm64). Archives unpack into a staging
directory; entries with absolute paths, `..`, hard links, or symlinks that leave the directory are
rejected. Nothing runs with sudo.

Prerequisites are installed first: a language server that needs npm sets up Node.js, and
mlx-whisper or Piper sets up uv.

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
