# Release and installation

## Install a release

The supported release installer is the asset `install.sh` on macOS and Linux:

```sh
curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | bash
```

Published targets are `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `x86_64-pc-windows-msvc`. The Windows archive is `dial-<tag>-x86_64-pc-windows-msvc.zip` containing `dial.exe`; install it with the release's `install.ps1` in Windows PowerShell 5.1 or PowerShell 7 (one-line commands in [windows.md](windows.md)).

The published installers (`install.sh` and `install.ps1`) embed the release's Ed25519 public key, select the host target, verify the detached signature over `SHA256SUMS`, and check the archive digest before extraction. `install.sh` installs into `~/.local/bin` (override with `DIAL_INSTALL_DIR`), validates the binary version before and after activation, and restores the previous executable if activation fails. Add the directory to `PATH`. For a pinned version, download `install.sh` and pass a tag such as `v0.1.1` to it. Release archives include `LICENSE`, `THIRD_PARTY_NOTICES`, and the Silver/Unscii font notices.

`dial update` checks the GitHub latest release (or `dial update --version 0.1.1`), verifies the same signed manifest and asset digest, stages on the install filesystem, checks `--version`, and atomically activates the binary with rollback. Windows uses a deferred PowerShell swap after the running process exits. Homebrew, Cargo, and Nix-managed installations are left to their package managers; the command prints the matching update instruction.

## Release signing inputs

The workflow intentionally fails closed until repository signing inputs are configured. Set:

- `DIAL_RELEASE_SIGNING_KEY` as a protected GitHub Actions secret containing an OpenSSL-readable Ed25519 private key in PEM format. Never commit or print it.
- `DIAL_RELEASE_PUBLIC_KEY` as a GitHub Actions variable containing the matching raw 32-byte Ed25519 public key, base64 encoded. Release binaries embed this value at compile time.
- `DIAL_RELEASE_PUBLIC_KEY_PEM` as a GitHub Actions variable containing the matching public key in PEM format. The published installer embeds it; the publish job derives and compares the raw key to the signing key before signing.

The publish job signs the exact `SHA256SUMS` bytes with Ed25519 and publishes its base64-encoded detached signature as `SHA256SUMS.sig`. Missing or mismatched values fail the release; no example or generated signing key is trusted by production code.

For local use of the source-tree installers, provide the trusted public PEM before invoking them: `install.sh` reads `DIAL_RELEASE_PUBLIC_KEY`, and `scripts/install.ps1` reads `DIAL_RELEASE_PUBLIC_KEY_PEM`. Each refuses to proceed without it.

## Other distribution channels

The Homebrew formula is in `Formula/dial.rb`; the Nix flake is `flake.nix`; the Docker image is built from `Dockerfile` and published to `ghcr.io/kiidxatlas/dial` on releases. Pin versions or image digests for reproducible deployment. The container is intended for non-interactive use; mount the project directory and persistent Dial state explicitly.

## Build from source

Install the pinned Rust toolchain from `rust-toolchain.toml` and run `cargo build --release --locked` for shipping optimization. `make build` and `make update` default to the optimized `local` profile (incremental compilation, no fat LTO); pass `PROFILE=release` for shipping-equivalent builds. `make update` rebuilds from the checkout, backs up existing binaries, and retains only the three newest backups at each managed location. `make check`, `make test-lib`, and `make test-module MOD=module::path` provide narrower development checks; `make test` retains the full Rust suite. CI caches Rust dependencies separately for test, lint, and release jobs. Python tests run under pytest in CI; optional model/runtime dependencies are separate from the Rust executable.

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
export PATH="$HOME/.local/bin:$PATH"
dial
```

When GitHub CLI is installed and signed in, the installer downloads through it; otherwise it uses
plain HTTPS. Complete the platform prerequisites below before installing.

The installer selects the correct archive, verifies the Ed25519 signature on `SHA256SUMS` against the release public key embedded in the published `install.sh`, then verifies the archive's SHA-256 before installing. `install.ps1` does the same with the key embedded in it.
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
cargo install --path . --locked
cd ~/code/my-project
dial                                  # native terminal UI; first run opens model/provider setup
dial "fix the failing test in src/lib.rs"   # interactive, opening with a message
dial -p -y "summarize this repo"      # one-shot; prints the answer (use -y or --approval auto for tools that change things)
dial -c                               # continue the latest session   (-r <id-prefix> to pick one)
```

To rebuild after changing this checkout, run `make update` here (or `make -C /path/to/dial update`).
This runs `cargo build --profile local --locked`: an optimized, incremental build without the shipping profile's expensive fat LTO. It copies the binary to `bin/dial` in this checkout (git-ignored), and installs it over the `dial` your PATH resolves to (or `~/.local/bin/dial` if none), also refreshing `~/.cargo/bin/dial` when present. Previous copies are kept as `dial.bak-<stamp>` (three most recent). Use `make update PROFILE=release` for shipping-equivalent optimization; published releases and `cargo install` still use the unchanged release profile.
`cargo install --path .`
installs into Cargo's bin directory instead. `command -v dial` shows which copy your terminal
will run, and `make help` lists the other build and check targets. Restart any running dial
session afterward. See `ARCHITECTURE.md` for the module layout.

For a faster development loop, use `make check` for compilation checks,
`make test-lib` for library unit tests, or `make test-module MOD=llm::provider`
for a single module (a module path is required). `make test-integration` runs the
process/API fixtures; `make test` still runs the full Rust suite. Tests use the
usual dev profile, so these commands share its build cache. `make build` uses the
optimized `local` profile; its first build creates a separate cache in
`target/local`, with subsequent edits benefiting from incremental compilation.

For local MLX you can also prepare the runtime and model explicitly:
`dial setup -m mlx-community/Qwen2.5-7B-Instruct-4bit` (requires `uv`).
Discovery itself does not download weights. Selecting an uncached local model and submitting a
prompt resolves its weights; a missing default MLX runtime gets an installation confirmation.

### Publishing a release (maintainers)

The release workflow runs on pushed `v*` tags and requires the tag to match
`Cargo.toml`'s version. After updating the version and lockfile as needed, commit
the release changes and push that commit and its tag, for example:

```sh
git tag v0.1.1
git push origin main
git push origin v0.1.1
```

`.github/workflows/release.yml` checks formatting, runs the Rust tests (including
required Linux sandbox probes), builds Apple Silicon/Intel Mac and Linux x86-64 binaries,
and smoke-tests each extracted archive outside the checkout. Only after **all four**
platform builds (macOS arm64, macOS Intel, Linux x86-64 and Windows x86-64) pass does it create a GitHub release with the archives, bundled MIT
license and README, a combined `SHA256SUMS` with its `SHA256SUMS.sig` signature, and
generated notes. CI runs on both macOS and Ubuntu without weakening host security. The Windows release job and the CI `installer-windows` job run `scripts/test-install.ps1` under Windows PowerShell 5.1 and PowerShell 7; the release job also tests the packaged archive. The publish job builds `install.ps1` with the release key embedded and publishes it with `SHA256SUMS` and `SHA256SUMS.sig`.
The workflow preserves repository visibility. Dial is MIT licensed; see `LICENSE` for
permission to use, modify, and redistribute it.
