# Release and installation

## Install a release

The supported release installer is the asset `install.sh` on macOS and Linux:

```sh
curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | bash
```

Published targets are `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-msvc`, and `aarch64-pc-windows-msvc`. The Windows archives are `dial-<tag>-<target>.zip` containing `dial.exe`; install with the release's `install.ps1` in Windows PowerShell 5.1 or PowerShell 7 (one-line commands in [windows.md](windows.md)).

On Linux, target selection prefers the active libc reported by `ldd`; installing musl development
tools on a glibc host does not switch that host to the musl build. Loader presence is a fallback
only when `ldd` is unavailable.

The published installers (`install.sh` and `install.ps1`) embed the release's Ed25519 public key,
select the host target, and check the archive digest against `SHA256SUMS` before extraction.
Both verify the signed manifest. `install.sh` tries OpenSSL 3 (on `PATH`, Homebrew's `openssl@3`,
or `DIAL_OPENSSL`), Python with `cryptography` (`DIAL_PYTHON`), then OpenSSH `ssh-keygen`
(`DIAL_SSH_KEYGEN`). If none is available, it refuses installation unless
`DIAL_ALLOW_UNSIGNED_INSTALL=1` explicitly opts into checksum-only verification.
`install.sh` installs into `~/.local/bin` (override with `DIAL_INSTALL_DIR`), validates the binary
version before and after activation, and restores the previous executable if activation fails.
When the directory is not on `PATH`, it appends one `export PATH=…` line (`fish_add_path` for fish)
to the login shell's profile (`~/.zshrc`, `~/.bash_profile` on macOS bash, `~/.bashrc` on Linux bash,
otherwise `~/.profile`), once; `DIAL_NO_MODIFY_PATH=1` prints the line instead. For a pinned
version, download `install.sh` and pass a tag such as `v0.2.2` to it. Release archives include
`LICENSE`, `THIRD_PARTY_NOTICES`, and the Silver/Unscii font notices.

`dial update` (or `/update` inside the interactive terminal, followed by `/restart`) updates Dial the way it was installed. For installer copies it checks the GitHub latest release (or `dial update --version 0.1.1`, `/update 0.1.1`). If you already run that release it says so and stops; an explicit `--version` always installs, so it can also reinstall or downgrade. It verifies the same signed manifest and asset digest, stages the candidate on the install filesystem, and checks its `--version`. On Unix it holds a lock file (`.dial-update.lock`) in the install directory so concurrent updates cannot interleave, preserves a digest-verified copy of the current binary as the rollback point, renames the candidate over the active name (never removing that name first, so an interruption leaves a complete old or new executable), fsyncs the directory, and re-checks `--version`. If the post-install check fails, the rollback copy is renamed back; if that also fails, the error reports both failures and the rollback copy is kept at `<name>.dial.bak` for manual recovery. Windows uses a deferred PowerShell swap after the running process exits, with its own rollback.

Other installations are updated by their owners, which `dial update` runs for you:

- **Homebrew** runs `brew upgrade dial` (`--version` is refused; the formula pins the version).
- **Cargo** runs `cargo install --locked --git https://github.com/KiidxAtlas/dial dial`, because Dial is not published on crates.io; `--version` adds `--tag v<version>`.
- **Source builds** carry no release verification key. If the checkout the binary was built from still exists, `dial update` refuses when tracked files have uncommitted changes, runs `git pull --ff-only` there (so a diverged branch is never merged), and then runs `make update`, which rebuilds and reinstalls with its usual backups. If the build already matches the checkout's commit and was clean, it stops without rebuilding. `--version` is refused; check out a tag and run `make update` instead. This applies to a source build in `~/.cargo/bin` too, since `make update` refreshes that copy. Without the checkout, `dial update` refuses before downloading anything and points to the signed release installer.
- **Nix** is not changed; update the dial flake input and rebuild.

### Update notices

The interactive terminal checks the GitHub latest release in the background, at most once a day, and shows a muted line above the composer when a newer version exists, for example ``Dial 0.2.7 is available (you have 0.2.6) · run `/update` ``. It names `/update` for every installation that command can update (installer, Homebrew, Cargo and source builds with their checkout); Nix installs, and source builds whose checkout is gone, get the manual instruction instead. The last answer is cached in `dial-release-check` in Dial's user directory, so the notice shows at once on later launches, including offline ones. Failed checks are silent and wait for the next day. No check runs in strict sandbox mode or with `sandbox.allow_network = false`, and print, JSON, RPC and ACP modes never show the notice. Nothing is installed automatically.

## Release signing inputs

The workflow intentionally fails closed until repository signing inputs are configured. Set:

- `DIAL_RELEASE_SIGNING_KEY` as a protected GitHub Actions secret containing an OpenSSL-readable Ed25519 private key in PEM format. Never commit or print it.
- `DIAL_RELEASE_PUBLIC_KEY` as a GitHub Actions variable containing the matching raw 32-byte Ed25519 public key, base64 encoded. Release binaries embed this value at compile time.
- `DIAL_RELEASE_PUBLIC_KEY_PEM` as a GitHub Actions variable containing the matching public key in PEM format. The published installer embeds it; the publish job derives and compares the raw key to the signing key before signing.

The publish job signs the exact `SHA256SUMS` bytes with Ed25519 and publishes the base64-encoded detached signature as `SHA256SUMS.sig`, plus an OpenSSH-format `SHA256SUMS.sshsig` for `ssh-keygen -Y verify`. Missing or mismatched values fail the release; no example or generated signing key is trusted by production code.

For local use of the source-tree installers, provide the trusted public PEM before invoking them: `install.sh` reads `DIAL_RELEASE_PUBLIC_KEY`, and `scripts/install.ps1` reads `DIAL_RELEASE_PUBLIC_KEY_PEM`. Each refuses to proceed without it.

## Other distribution channels

The Homebrew formula is in `Formula/dial.rb`. Each release regenerates its checksums from `SHA256SUMS` with `scripts/update-formula.py` and opens a `release-packaging/<tag>` pull request for review; merge it after the release is published. The Nix flake is `flake.nix`; the Docker image is built from `Dockerfile` and published to `ghcr.io/kiidxatlas/dial` on releases. Pin versions or image digests for reproducible deployment. The container is intended for non-interactive use; mount the project directory and persistent Dial state explicitly.

To build the container locally, pass the source revision explicitly because `.dockerignore`
excludes Git metadata:

```sh
docker build --build-arg DIAL_BUILD_REVISION="$(git rev-parse HEAD)" -t dial .
```

The release workflow supplies the tagged commit and `DIAL_RELEASE_PUBLIC_KEY` as build arguments
so the container binary retains its source identity and trusted update key.

## Build from source

Install the pinned Rust toolchain from `rust-toolchain.toml` and run `cargo build --release --locked -p dial` for shipping optimization. `make build` and `make update` default to the optimized `local` profile (incremental compilation, no LTO); pass `PROFILE=release` for shipping-equivalent builds (thin LTO). `make update` rebuilds from the checkout, backs up existing binaries, and retains only the three newest backups at each managed location. `make check`, `make test-lib`, and `make test-module MOD=module::path` provide narrower development checks; `make test` retains the full Rust suite. CI builds and tests with the `ci` profile (unoptimized, no debug info) and caches Rust dependencies separately for test, lint, and release jobs. Release builds start as soon as a tag is pushed and run alongside CI; the publish job waits for CI to pass on the tagged commit before signing and publishing anything. Python tests run under pytest in CI; optional model/runtime dependencies are separate from the Rust executable.

Unix subprocess creation shares one pipe/fork guard for ordinary, detached and process-group
children. This prevents concurrent core spawns from retaining unrelated output-pipe writers on
macOS, where close-on-exec pipe creation is not atomic. Cancellation still kills the owned process
group and bounds output draining; output is marked incomplete if EOF cannot be observed.

## Platform installation details

Release targets are **macOS 13+ on Apple Silicon (arm64) or Intel (x86-64)**, **Linux x86-64 (Ubuntu
22.04 or newer)**, and **native Windows x86-64** (`dial-<tag>-x86_64-pc-windows-msvc.zip`, installed
with `scripts/install.ps1` in PowerShell). Linux x86-64 builds also run under WSL2 Ubuntu. No Rust compiler or source
checkout is needed for prebuilt releases. Download the archive for your platform
and the release's `SHA256SUMS` and `SHA256SUMS.sig` files from
[GitHub Releases](https://github.com/KiidxAtlas/dial/releases).

For the terminal installer:

```sh
curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | bash
dial
```

When GitHub CLI is installed and signed in, the installer downloads through it; otherwise it uses
plain HTTPS. Complete the platform prerequisites below before installing.

The installer selects the correct archive, verifies the Ed25519 signature on `SHA256SUMS` against the release public key embedded in the published `install.sh` (when OpenSSL 3 is available; see above), then verifies the archive's SHA-256 before installing. `install.ps1` does the same with the key embedded in it.
Run `bash install.sh v0.1.1` to select a specific release. On Windows, run the release's `install.ps1` in PowerShell for the native `dial.exe` (commands in [windows.md](windows.md)), or run the Linux commands in your WSL2 Ubuntu terminal; the PowerShell command in the WSL2 section below installs WSL2 first.

### macOS (Apple Silicon or Intel)

In the download directory (replace `v0.1.1` with the release you downloaded):

```sh
version=v0.1.1
# Apple Silicon: aarch64-apple-darwin; Intel: x86_64-apple-darwin
target=aarch64-apple-darwin
grep "  dial-$version-$target.tar.gz$" SHA256SUMS | shasum -a 256 -c -   # after verifying SHA256SUMS.sig as scripts/install.sh does
tar -xzf "dial-$version-$target.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 755 "dial-$version-$target/dial" "$HOME/.local/bin/dial"
export PATH="$HOME/.local/bin:$PATH"
dial --version
```

Stop if checksum verification fails. Add `export PATH="$HOME/.local/bin:$PATH"`
to your shell configuration (usually `~/.zshrc`) to keep Dial on your PATH.
To update, repeat with a newer release and restart any running Dial sessions.
The archives are not signed or notarized; if macOS blocks execution, review your
system's security prompt rather than disabling Gatekeeper globally.

Hosted providers need an account/API key configured through first-run setup, but
do not need Python. Local MLX inference is **macOS/Apple Silicon only** and
additionally requires `uv`, network access for runtime/model downloads, and sufficient memory for the selected model. The
Python worker is embedded in the binary; no repository checkout is needed.

### Windows through WSL2 / Linux x86-64

On Windows, install **WSL2 with Ubuntu 22.04 or newer**, using PowerShell as
administrator if it is not already installed:

```powershell
wsl --install -d Ubuntu-22.04
```

Restart if prompted and create your normal Ubuntu user. For confinement, disable
Windows executable interop in this Ubuntu distribution: edit `/etc/wsl.conf` with
`sudoedit`, preserving existing settings, and add or update:

```ini
[interop]
enabled=false
appendWindowsPath=false
```

Then run `wsl --shutdown` from PowerShell and reopen Ubuntu. This affects that
distribution's ability to launch Windows executables, not Windows itself. Dial
refuses sandboxed commands while Windows interop remains enabled, rather than
letting a Windows process escape Linux confinement.

In Ubuntu (or native Linux), install the sandbox and certificates:

```sh
sudo apt-get update
sudo apt-get install -y bubblewrap ca-certificates
```

Download the Linux archive and checksum into a Linux directory, then:

```sh
version=v0.1.1
grep "  dial-$version-x86_64-unknown-linux-gnu.tar.gz$" SHA256SUMS | sha256sum -c -   # after verifying SHA256SUMS.sig as scripts/install.sh does
tar -xzf "dial-$version-x86_64-unknown-linux-gnu.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 755 "dial-$version-x86_64-unknown-linux-gnu/dial" "$HOME/.local/bin/dial"
export PATH="$HOME/.local/bin:$PATH"
dial --version
```

Stop if checksum verification fails. Add the PATH export to `~/.bashrc`. Keep your
checkout and Dial's home/state on the Linux filesystem (for example `~/code`),
not the Windows-mounted drive, and run Dial as your normal user, never with sudo.
Choose a hosted provider in first-run setup or `/providers`; local MLX is unavailable on Intel Macs and Linux/WSL2.
An OpenAI-compatible server running elsewhere can also be configured as an endpoint.

Linux/WSL2 commands require working unprivileged user namespaces. Dial runs a real
filesystem and network enforcement probe; missing or blocked bubblewrap disables
command execution with an error, never an unsandboxed fallback. Linux Dev mode is
deliberately stricter than macOS Dev mode: only the system runtime, workspace and
explicit `sandbox.extra_write` directories are visible. Host home directories,
IPC sockets and Windows drives are not exposed wholesale. Secret/denied paths
overlapping any exposed tree are refused; keep secrets outside allowed trees.
Toolchains must be installed in system paths or inside the project/explicit allowed
directories. Do not allow your entire home directory just to expose a toolchain.

### Build from source and development builds

With Rust and a C toolchain installed (Xcode Command Line Tools on macOS;
`build-essential` on Ubuntu), clone this repository and run:

```sh
make update
cd ~/code/my-project
dial                                  # native terminal UI; first run opens model/provider setup
dial "fix the failing test in src/lib.rs"   # interactive, opening with a message
dial -p -y "summarize this repo"      # one-shot; prints the answer (use -y or --approval auto for tools that change things)
dial -c                               # continue the latest session   (-r <id-prefix> to pick one)
```

To rebuild after changing this checkout, run `make update` here (or `make -C /path/to/dial update`).
This runs `cargo build -p dial --bin dial --profile local --locked --timings`: an incremental build without the shipping profile's thin LTO. Dependencies stay optimized at level 2; Dial itself uses level 1 and more parallel codegen to reduce rebuild time. Published releases and `cargo install` still use the unchanged release profile. It copies the binary to `bin/dial` in this checkout (git-ignored), and installs it over the `dial` your PATH resolves to (or `~/.local/bin/dial` if none), also refreshing `~/.cargo/bin/dial` when present. Previous copies are kept as `dial.bak-<stamp>` (three most recent). Use `make update PROFILE=release` for shipping-equivalent optimization.
`cargo install --path . --locked` remains available when you specifically want a release build
installed into Cargo's bin directory instead. `command -v dial` shows which copy your terminal
will run, and `make help` lists the other build and check targets. Restart any running dial
session afterward. See `ARCHITECTURE.md` for the module layout.

For a faster development loop, use `make check` for compilation checks,
`make test-lib` for library unit tests, or `make test-module MOD=llm::provider`
for a single module (a module path is required). `make test-integration` runs the
process/API fixtures; `make test` still runs the full Rust suite. Tests use the
usual dev profile, so these commands share its build cache. `make build` uses the
optimized `local` profile; its first build creates a separate cache in
`target/local`, with subsequent edits benefiting from incremental compilation.

### Faster builds and optional integrations

```sh
make update                         # all capabilities, faster local compilation
make update SLIM=1                  # omit optional integrations
make update SLIM=1 FEATURES=aws      # slim, but keep the AWS credential chain / SigV4
make build SLIM=1 FEATURES=desktop   # slim, but keep native desktop control
make check SLIM=1                   # typecheck the same feature selection
```

Default builds retain voice, embedded shell, native desktop control and AWS support.
Slim builds omit these optional integrations, retaining the external shell, browser,
coding tools and non-AWS model providers. Bedrock bearer-token authentication remains
available; AWS credential-chain/SigV4 authentication requires `FEATURES=aws`. Native
desktop operations require `FEATURES=desktop`; the `voice` and `embedded-shell` features
can also be restored individually. Use a quoted, comma-separated list for several,
for example `FEATURES='aws,desktop'`. Missing optional capabilities report an explicit
error rather than silently succeeding. Select the same flags on subsequent updates.
Native-only desktop helpers are compiled only with a supported desktop backend (or
their unit tests), so slim builds do not produce unused-native-code warnings.

Builds automatically write Cargo's timing report to
`target/cargo-timings/cargo-timing.html`. Keep `target` between builds: the first build
of a profile or feature selection compiles its dependencies, and switching toolchains
or features can invalidate cached work. `CARGO_TARGET_DIR=/path/to/cache make update`
uses that persistent cache for both compilation and installation; its timing report
is under that directory instead. No clean step or toolchain update is part of `make update`.
For a compile-only, unoptimized development binary, use `make build PROFILE=dev`.

For local MLX you can also prepare the runtime and model explicitly:
`dial setup -m mlx-community/Qwen2.5-7B-Instruct-4bit` (requires `uv`).
Discovery itself does not download weights. Selecting an uncached local model and submitting a
prompt resolves its weights; a missing default MLX runtime gets an installation confirmation.

### Publishing a release (maintainers)

The release workflow runs on pushed `v*` tags and requires the tag to match
`Cargo.toml`'s version. `scripts/release.sh <version>` prepares the release from a clean `main`.
It first refuses unless `HEAD` matches `origin/main` and a `CI` run on that commit succeeded (it
asks `gh`), so a tag is never placed on code CI has not passed. It then sets
`[workspace.package] version`, refreshes `Cargo.lock` for workspace members only, moves the
`## [Unreleased]` changelog entries under a dated version heading (refusing an empty section), runs
`scripts/preflight.sh --quick` on the bumped tree (skip with `--no-check`), commits
`release: v<version>` and creates an annotated tag. It pushes only with `--push`:

```sh
scripts/release.sh 0.1.23          # commit + tag locally
git push origin main
git push origin v0.1.23
```

#### Checking a commit locally before pushing

`make preflight` runs CI's checks on a clean snapshot of `HEAD` (`REV=<commit>` for another), so
uncommitted edits in the checkout neither hide nor cause failures. The snapshot is a detached
worktree in `.tmp/preflight/wt` with a persistent target directory, so repeat runs are incremental,
and a tree that passed is recorded and not checked again. It covers, in CI's terms:

- the lint job: `cargo fmt --check`, clippy with `-D warnings` for the host **and** for
  `x86_64-unknown-linux-gnu` (so `cfg(target_os = "linux")` code is linted, as on CI's Ubuntu
  runner), and `cargo deny`;
- the musl job: `cargo check` for both musl release targets;
- a Windows compile check (`x86_64-pc-windows-gnu`, which type-checks the same `cfg(windows)` code
  as CI's MSVC build) and the slim build;
- the installer, build, Windows-contract-runner, packaging, driver and Python contracts (Python 3.12
  with CI's packages, in a throwaway `uv` environment);
- the workspace tests in the system temp directory (on macOS this sits behind the `/var` ->
  `/private/var` symlink, as on CI runners, so path-comparison bugs surface locally), then the
  installed binary's artifact smoke test and the real-terminal smoke scripts.

Cross targets compile their C dependencies with `zig` (`scripts/zig-cc`); install it
(`brew install zig`) and the rustup targets, or those steps are reported as not covered.
Tests run only on the host, so Linux-only tests (sandbox enforcement) and the native Windows suite
still run only in CI; they are compile-checked locally. `make preflight-quick` runs only the compile
and lint gates (about four minutes warm). A run stops at the first failing step and prints the end
of its log; `PREFLIGHT_KEEP_GOING=1` reports every failing step. Logs are in `.tmp/preflight/logs/`.

`make hooks` points `core.hooksPath` at `.githooks`, whose `pre-push` hook runs `make preflight`
on every commit pushed to a branch and refuses the push if it fails. `DIAL_PREFLIGHT=quick git
push` runs only the compile and lint gates; `DIAL_PREFLIGHT=skip git push` (or `--no-verify`)
bypasses it. Runs in one checkout are serialized.

CI skips commits that only touch Markdown under `docs/`, root-level Markdown or the PR template.
Changes to protocol files such as `docs/rpc-schema.json` still trigger CI. A release commit always
changes `Cargo.toml`, so CI runs for it; do not tag a documentation-only commit.

`.github/workflows/release.yml` does not re-run the test suite. Each macOS and Linux build job
first requires a successful `CI` run (formatting, clippy, the full Rust suite including the Linux
sandbox probes, and the terminal smoke test, on macOS arm64, macOS Intel and Ubuntu) for the exact
tagged commit, waiting while that run is in progress; with no successful run it fails. Push the
commit to `main` and let CI pass before tagging. The jobs then build Apple Silicon/Intel Mac and
Linux x86-64 and arm64 (glibc and musl) release binaries and smoke-test each extracted archive outside the checkout. Only
after **all** platform builds (macOS arm64 and Intel, Linux x86-64 and arm64 in glibc and musl, Windows x86-64 and arm64) pass,
does it create a GitHub release with the archives, bundled MIT license and README, a
combined `SHA256SUMS` with its `SHA256SUMS.sig` and `SHA256SUMS.sshsig` signatures, and generated notes. CI runs on both
macOS and Ubuntu without weakening host security. CI builds the `dial` library and its unit-test
build one after the other: each peaks near 5 GB in rustc, and together they would swap on the
7 GB macOS arm64 runner. The native Windows Rust contract (`scripts/windows-contract.txt`, which lists its skipped tests with reasons) runs in CI's `windows-rust` job, so the CI requirement above covers it; the release does not repeat it. The Windows release jobs (x86-64 and ARM64) build `dial.exe`, smoke-test the packaged archive (`--version`, `--help`), verify the binary format for the target, and run `scripts/test-install.ps1` under Windows PowerShell 5.1 and PowerShell 7, as does the CI `installer-windows` job. The publish job builds `install.ps1` with the release key embedded and publishes it with `SHA256SUMS`, `SHA256SUMS.sig`, and `SHA256SUMS.sshsig`.

CI caches compiled dependencies per job with `Swatinem/rust-cache`. Caches are saved only from
`main`, and also after a failing run, so one red test does not force the next run on that OS to
rebuild from scratch. The repository's 10 GB cache quota evicts the least recently used entries
first.

The workflow preserves repository visibility. Dial is MIT licensed; see `LICENSE` for
permission to use, modify, and redistribute it.
