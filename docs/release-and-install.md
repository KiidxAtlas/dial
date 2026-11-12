# Release and installation

## Release-readiness checklist

- Review and commit the intended source changes before running `make preflight`: it validates
  a clean snapshot of the selected commit, not uncommitted work. Do not discard unrelated work
  to obtain a clean status. Review version-specific release notes before tagging.
- Keep the source scaffold separate from local state. `src/`, `crates/`, `python/`, `runtimes/`
  and `assets/` contain implementation and bundled resources; `tests/` and `examples/` contain
  regression coverage and runnable examples; `scripts/`, `.github/` and `Formula/` own release
  and check tooling. The root manifests, lockfile, licenses and platform packaging are required.
- Benchmark reports and `scratch/` experiments are disposable local output, not release inputs.
  Git and Docker exclusions keep them out of new source changes and container build contexts.
  A local cleanup must preserve installed environments, build caches, private Dial state,
  unfinished plans and unrelated work unless their removal is separately approved.
- Confirm required main/tag protections, signing-environment reviewers and secret scoping in
  GitHub. Local workflow files cannot establish repository settings or successful publication.
- Run applicable native-platform and optional integration tiers explicitly. Host tests and
  cross-compilation do not verify Linux confinement, Windows runtime behavior, native desktop
  capture/input, live providers, remote SSH or collaboration interoperability.
- Verify the published archive signatures and container digest signature/provenance/SBOM.
  Hosted signing/publication and bit-for-bit reproducibility require independent acceptance;
  standalone archives do not yet have equivalent provenance/SBOM attestations. CI Python tools
  use the transitive, hashed `scripts/requirements-ci.lock`. The Nix input is pinned to an immutable
  revision, but still needs a genuine resolver-generated `flake.lock` and native Nix acceptance.

### Known platform limits

Portable Unix process-group signalling remains best-effort against PID reuse. General storage
retention policy and native accessibility/terminal compatibility require separate owner checks;
historical report pass counts do not establish current release readiness.

### Dependency reproducibility

`scripts/requirements-ci.txt` owns direct portable test dependencies. CI installs the generated
`scripts/requirements-ci.lock` with `pip --require-hashes`; local preflight consumes the same lock.
Regenerate intentionally with:

```sh
uv pip compile scripts/requirements-ci.txt --python-version 3.12 --generate-hashes --universal --output-file scripts/requirements-ci.lock
```

Review the lock diff and run the Python, installer and benchmark contracts after updates.
This lock covers CI tooling, not the optional MLX worker (`python/pyproject.toml`), Laya or
operator-installed extensions. Their separate environments must not be called fully reproducible.

`flake.nix` pins nixpkgs to an immutable commit rather than a moving branch. On a Nix-enabled host,
run `nix flake lock`, review and stage `flake.lock`, then run `nix flake check` and `nix build`.
Do not fabricate a NAR hash or treat source inspection as native Nix acceptance.

### Hosted settings acceptance

A repository owner must configure and verify `main` and release-tag protections, repository
rulesets, and reviewer/deployment-branch restrictions for the `release-signing` environment
before publication. Missing required protections are release blockers; workflow files alone
cannot establish the current hosted settings. See [Release signing inputs](#release-signing-inputs).

## Install a release

The supported release installer is the asset `install.sh` on macOS and Linux. `dial.rootatlas.dev/install`
sends terminal clients (curl, wget) straight to the latest release's asset, so the short command and the
GitHub URL run the same script:

```sh
curl -fsSL https://dial.rootatlas.dev/install | sh
# same script: curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | sh
```

`install.sh` is POSIX `sh` (dash, busybox ash, bash and zsh all run it); bash is not required.

Published targets are `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-msvc`, and `aarch64-pc-windows-msvc`. The Windows archives are `dial-<tag>-<target>.zip` containing `dial.exe`; install with the release's `install.ps1` in Windows PowerShell 5.1 or PowerShell 7 (one-line commands in [windows.md](windows.md)).

On Linux, target selection prefers the active libc reported by `ldd`; installing musl development
tools on a glibc host does not switch that host to the musl build. Loader presence is a fallback
only when `ldd` is unavailable.

The published installers (`install.sh` and `install.ps1`) embed the release's Ed25519 public key,
select the host target, and check the archive digest against `SHA256SUMS` before extraction.
Both verify the signed manifest. `install.sh` tries OpenSSH `ssh-keygen` 8.1+ first
(`DIAL_SSH_KEYGEN`; it ships with macOS and most Linux distributions, so usually nothing extra is
needed), then OpenSSL 3 (on `PATH`, Homebrew's `openssl@3`, or `DIAL_OPENSSL`), then Python with
`cryptography` (`DIAL_PYTHON`). OpenSSL and Python also cover older releases published without
`SHA256SUMS.sshsig`. If none can verify, it refuses installation unless
`DIAL_ALLOW_UNSIGNED_INSTALL=1` explicitly opts into checksum-only verification.
`install.sh` installs into `~/.local/bin` (override with `DIAL_INSTALL_DIR`), validates the binary
version before and after activation, and restores the previous executable if activation fails.
When the directory is not on `PATH`, it appends one `export PATH=…` line (`fish_add_path` for fish)
to the login shell's profile (`~/.zshrc`, `~/.bash_profile` on macOS bash, `~/.bashrc` on Linux bash,
otherwise `~/.profile`), once; `DIAL_NO_MODIFY_PATH=1` prints the line instead. For a pinned
version, pass a tag such as `v0.2.2`: `curl -fsSL https://dial.rootatlas.dev/install | sh -s -- v0.2.2`. Release archives include
`LICENSE`, `THIRD_PARTY_NOTICES`, and the Silver/Unscii font notices.

`dial update` (or `/update` inside the interactive terminal, followed by `/restart`) updates Dial the way it was installed. For installer copies it checks the GitHub latest release (or `dial update --version 0.1.1`, `/update 0.1.1`). If you already run that release it says so and stops; an explicit `--version` always installs, so it can also reinstall or downgrade. It verifies the same signed manifest and asset digest, stages the candidate on the install filesystem, and checks its `--version`. On Unix it holds a lock file (`.dial-update.lock`) in the install directory so concurrent updates cannot interleave, preserves a digest-verified copy of the current binary as the rollback point, renames the candidate over the active name (never removing that name first, so an interruption leaves a complete old or new executable), fsyncs the directory, and re-checks `--version`. If the post-install check fails, the rollback copy is renamed back; if that also fails, the error reports both failures and the rollback copy is kept at `<name>.dial.bak` for manual recovery. Windows uses a deferred PowerShell swap after the running process exits, with its own rollback.

Other installations are updated by their owners, which `dial update` runs for you:

- **Homebrew** runs `brew upgrade dial` (`--version` is refused; the formula pins the version).
- **Cargo** runs `cargo install --locked --git https://github.com/KiidxAtlas/dial dial`, because Dial is not published on crates.io; `--version` adds `--tag v<version>`.
- **Source builds** carry no release verification key. If the checkout the binary was built from still exists, `dial update` refuses when tracked files have uncommitted changes, runs `git pull --ff-only` there (so a diverged branch is never merged), and then runs `make update`, which rebuilds and reinstalls with its usual backups. If the build already matches the checkout's commit and was clean, it stops without rebuilding. `--version` is refused; check out a tag and run `make update` instead. This applies to a source build in `~/.cargo/bin` too, since `make update` refreshes that copy. Without the checkout, `dial update` refuses before downloading anything and points to the signed release installer.
- **Nix** is not changed; update the dial flake input and rebuild.

### Update notices

The interactive terminal checks the GitHub latest release in the background, at most once a day, and shows a muted line above the composer when a newer version exists, for example ``Dial 0.2.7 is available (you have 0.2.6) · run `/update` ``. The line stays pinned for five minutes per release; after that the release is remembered as seen and the notice remains only in `/inbox`. It names `/update` for every installation that command can update (installer, Homebrew, Cargo and source builds with their checkout); Nix installs, and source builds whose checkout is gone, get the manual instruction instead. The last answer is cached in `dial-release-check` in Dial's user directory, so the notice shows at once on later launches, including offline ones. Failed checks are silent and wait for the next day. No check runs in strict sandbox mode or with `sandbox.allow_network = false`, and print, JSON, RPC and ACP modes never show the notice. Nothing is installed automatically.

### Which build am I running?

`dial --version` shows the compiled revision and whether the sources differed from it at build
time. `dial --build-info` prints JSON with the target, profile, build time and executable path.
Neither reads configuration or contacts a model. Builds without Git metadata report `unknown`;
release and archive builders can set `DIAL_BUILD_REVISION` and `SOURCE_DATE_EPOCH` explicitly.

## Release signing inputs

The workflow intentionally fails closed until repository signing inputs are configured. Set:

- `DIAL_RELEASE_SIGNING_KEY` as a secret scoped to the **`release-signing` environment**, containing an OpenSSL-readable Ed25519 private key in PEM format. Never commit or print it.
- `DIAL_RELEASE_PUBLIC_KEY` as a GitHub Actions variable containing the matching raw 32-byte Ed25519 public key, base64 encoded. Release binaries embed this value at compile time.
- `DIAL_RELEASE_PUBLIC_KEY_PEM` as a GitHub Actions variable containing the matching public key in PEM format. The published installer embeds it; the signing job derives and compares the raw key to the signing key before signing.

Configure required reviewers and restrict deployment tags on `release-signing` in GitHub's
repository settings. The workflow's `environment:` declaration does **not** establish these
protections by itself. Protect `main` and release tags, require reviewed pull requests and CI,
and keep signing secrets unavailable to other jobs. Repository-owner configuration is required.

The isolated read-only signing job signs the exact `SHA256SUMS` bytes with Ed25519 and
publishes the base64-encoded detached signature as `SHA256SUMS.sig`, plus an OpenSSH-format
`SHA256SUMS.sshsig`. Its temporary private-key file is owner-only and shredded on exit, and
again by an `if: always()` step that runs even when the signing step was killed before its exit
trap. A separate least-privilege publishing job, which never receives the key, checks every
artifact against `SHA256SUMS` and the signature against `DIAL_RELEASE_PUBLIC_KEY_PEM` before it
publishes them. Missing or mismatched values fail the release; no example or generated signing
key is trusted by production code.

Tags must be ancestors of `main`, with a completed successful **push-to-main** CI run for
the exact tagged SHA; pull-request CI does not authorize publication. Preparation runs concurrently;
`release-publish.yml` serializes publication, package metadata and container updates together.
An older stable tag may publish versioned artifacts but cannot move GitHub, Homebrew or
container `latest` backwards. Container builds request BuildKit provenance and an SBOM,
then sign and verify the immutable image digest with pinned Cosign using GitHub OIDC.
The installer action is commit-pinned and verifies its pinned Cosign binary's checksum.
These workflow steps have not established a successful hosted signing run locally and do not
provide provenance/SBOM for the standalone release archives.

To verify a published container, resolve its digest and use the exact release tag identity
(replace `vX.Y.Z` and `sha256:…`):

```sh
cosign verify --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity 'https://github.com/KiidxAtlas/dial/.github/workflows/release-publish.yml@refs/tags/vX.Y.Z' \
  'ghcr.io/kiidxatlas/dial@sha256:…'
```

This identity names the reusable publishing workflow (`job_workflow_ref` in the OIDC token),
not its caller. Releases from before the publication-workflow split used `release.yml` instead;
select the exact workflow in the tagged source rather than accepting an identity wildcard.

An attestation or signature is not proof that the image is safe. Verify the expected repository,
workflow, tag and digest; do not accept an arbitrary GitHub-issued certificate identity.

For local use of the source-tree installers, provide the trusted public PEM before invoking them: `install.sh` reads `DIAL_RELEASE_PUBLIC_KEY`, and `scripts/install.ps1` reads `DIAL_RELEASE_PUBLIC_KEY_PEM`. Each refuses to proceed without it.

## Other distribution channels

The Homebrew formula is in `Formula/dial.rb`. Install it from this repository's tap:

```sh
brew tap kiidxatlas/dial https://github.com/KiidxAtlas/dial
brew install dial
```

After a release is published, the release workflow verifies the signed `SHA256SUMS`, regenerates `Formula/dial.rb` and the AUR `dial-bin` metadata from it with `scripts/update-formula.py`, and opens and merges a `release-packaging/<tag>` pull request, so Homebrew users get the release without a manual step. Submission-ready definitions for the AUR (`dial-bin`) and homebrew-core are in [`packaging/`](../packaging/README.md). The Nix flake is `flake.nix`; the Docker image is built from `Dockerfile` and published to `ghcr.io/kiidxatlas/dial` on releases. Pin versions or image digests for reproducible deployment. The container runs interactively or one-shot; mount the project directory and persistent Dial state explicitly. [`docs/docker.md`](docker.md) has the `docker run` and Compose recipes, the sandbox options Bubblewrap needs, and Docker Model Runner setup.

To build the container locally, pass the source revision explicitly because `.dockerignore`
excludes Git metadata:

```sh
docker build --build-arg DIAL_BUILD_REVISION="$(git rev-parse HEAD)" \
  --build-arg SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)" -t dial .
```

The release workflow does not compile Dial again for the image. It verifies the signed manifest
and Linux x86-64 GNU archive, stages only its binary and notices with `scripts/stage-container.py`,
and assembles `Dockerfile.release` for `linux/amd64`. The binary retains the candidate's exact
source identity and embedded update key. The source-building `Dockerfile` remains available for
local development and CI. Published images are smoke-tested, then signed by immutable digest.

### Release speed and measurement

Main CI builds all eight unsigned archives in parallel with its tests while the version is still
untagged. A tag promotes those exact-commit archives after CI succeeds; absent, expired or
public-key-mismatched candidates trigger a full rebuild. The tag gate reports which path it chose.
The optional rebuild may be skipped without skipping publication after successful signing.
Only publication and downstream `latest` updates share the release queue, so a slow candidate
build no longer holds up another tag's validation or preparation. Signing protections, archive
checks, native tests and version ordering remain required. Portable Unix installer tests run in
main CI rather than once for each of the six Unix candidate targets; each archive still gets its
own format/runtime smoke checks, and Windows retains its archive-backed installer checks.

CI explicitly selects Nextest's `ci` reporting profile, independently of Cargo's `ci` build profile,
and uploads `test-timings-<os>` artifacts containing `junit.xml` even when tests fail. Use the
per-test durations to investigate the critical path before removing coverage. The inspected
macOS CI run `42935891367` spent 646 seconds in Nextest; its settings tiny-terminal grid test
took 86 seconds. Those historical measurements are not a promised speedup or current acceptance.
Locally, `make test-nextest TEST_PROFILE=ci NEXTEST_PROFILE=ci` produces the same JUnit report
under `target/nextest/ci/` (or the selected `CARGO_TARGET_DIR`).

Profiling the unchanged 961-size settings grid found repeated linear scans of the original
settings declarations. Original locations are now indexed once, and the normal settings view
does not compute learning-only membership. Three local runs on the same Apple Silicon host
reduced the grid's median from 7.65 to 2.21 seconds (3.46×); all dimensions and modes remain
covered. Other activity on this shared host was not independently ruled out; these are indicative
local test timings, not an isolated benchmark or a measured reduction in hosted CI duration.

Optional repository variables `DIAL_MACOS_RUNNER` and `DIAL_WINDOWS_RUNNER` select faster
GitHub-hosted runner labels for push builds/tests/lints on their corresponding platform.
Leave them unset to retain the existing runners. The replacement must provide the same OS and
architecture: Apple Silicon macOS for `macos-15`, x86-64 Windows for `windows-2025`.
Windows ARM and Linux keep their existing runners; pull requests always use the default labels.
Provisioning/billing and access to larger runners require repository-owner configuration; these
variables do not provision runners. Larger runners are billed even for public repositories;
confirm account eligibility and a budget before selecting one. Do not point them at persistent
self-hosted machines
without a separately reviewed isolation and cleanup policy. Compare queue time separately from
job execution time, and do not assume that consolidating parallel targets on one cross-builder
will reduce wall-clock time. Intel macOS already cross-compiles on Apple Silicon with Rosetta
smoke coverage; cross-compilation does not replace native Linux/Windows runtime tests.

The manual **Release optimization benchmark** workflow compares cold, full-featured thin-LTO
and no-LTO builds on native macOS ARM and Linux x86-64 runners. It never signs or publishes.
It records build seconds, binary bytes, binary SHA-256, compiler/source identity, seven warmed
`--help` startup samples and their median, plus Cargo HTML timings, then runs artifact/recovery
smoke tests. It deliberately restores no build cache. Repeat the workflow before drawing timing
conclusions; startup measurements are not representative of all agent workloads. Shipping stays
on thin LTO until representative parsing/image/runtime benchmarks and size results justify a change.

A single native Apple Silicon/macOS cold-build comparison with Rust 1.99.0 and a dirty snapshot
of `99ad369` measured 132.4 seconds with thin LTO and 136.4 seconds without it. Both binaries
passed artifact and recovery smoke checks; their sizes were 147.7 MB and 145.3 MB respectively.
Another session reported overlapping benchmark processes during both builds, so these are
shared-host observations, not an isolated LTO comparison. The approximately 3% difference
does not establish an improvement from disabling LTO. The shipping profile is unchanged.

For a local measurement on a native host (set the matching target triple):

```sh
python3 scripts/benchmark-release.py --target aarch64-apple-darwin --lto thin --output target/benchmark-thin.json
python3 scripts/benchmark-release.py --target aarch64-apple-darwin --lto off --output target/benchmark-off.json
```

Each variant has a separate target directory. Use a new `--target-dir` for each cold measurement;
reusing it measures a cached build. Do not clear someone else's caches or benchmark during other
builds. Local preflight already reuses a successful unchanged tree; its result does not replace
the exact-commit hosted main-CI publication gate.

## Build from source

Install the pinned Rust toolchain from `rust-toolchain.toml` and run `cargo build --release --locked -p dial` for shipping optimization. `make build` and `make update` default to the optimized `local` profile (incremental compilation, no LTO); pass `PROFILE=release` for shipping-equivalent builds (thin LTO). `make update` rebuilds from the checkout, backs up existing binaries, and retains only the three newest backups at each managed location. `make check`, `make test-lib`, and `make test-module MOD=module::path` provide narrower development checks; `make test` retains the full Rust suite. CI builds and tests with the `ci` profile (unoptimized, no debug info) and caches Rust dependencies separately for test, lint, and release jobs. Main CI builds unsigned candidates alongside tests; the tag workflow requires a completed successful push-to-main CI run for the exact tagged commit before fallback builds, signing or publication. Python tests run under pytest in CI; optional model/runtime dependencies are separate from the Rust executable.

`make test-python` and `make test-release-policy` check the selected `PYTHON` interpreter before
starting their suites. Missing or broken `pytest`/`cryptography` imports fail with the interpreter
path and setup command, rather than a partial run or an optional-dependency skip. To use the
pinned portable test environment without modifying system Python:

```sh
uv run --no-project --python 3.12 --with-requirements scripts/requirements-ci.lock \
  make test-python test-release-policy
```

An existing prepared environment can be selected with `make PYTHON=/path/to/python ...`.
The test targets never install packages automatically; add `--offline` to `uv run` when its
interpreter and pinned dependencies are already cached. Optional MLX/model checks remain
separate and report their unavailable prerequisites.

Unix subprocess creation shares one pipe/fork guard for ordinary, detached and process-group
children. This prevents concurrent core spawns from retaining unrelated output-pipe writers on
macOS, where close-on-exec pipe creation is not atomic. Cancellation still kills the owned process
group and bounds output draining; output is marked incomplete if EOF cannot be observed.

## Platform installation details

Release targets are **macOS 13+ on Apple Silicon (arm64) or Intel (x86-64)**, **Linux x86-64
and arm64 (glibc or musl; Ubuntu 22.04 or newer)**, and **native Windows x86-64 and arm64**
(`dial-<tag>-<target>.zip`, installed with the release's `install.ps1` in PowerShell). Linux builds
also run under WSL2 on the matching architecture. No Rust compiler or source checkout is needed
for prebuilt releases. Download the archive for your platform
and the release's `SHA256SUMS` and `SHA256SUMS.sig` files from
[GitHub Releases](https://github.com/KiidxAtlas/dial/releases).

For the terminal installer:

```sh
curl -fsSL https://dial.rootatlas.dev/install | sh
dial
```

When GitHub CLI is installed and signed in, the installer downloads through it; otherwise it uses
plain HTTPS. Complete the platform prerequisites below before installing.

The installer verifies the Ed25519 signature on `SHA256SUMS` against the embedded release public key, using OpenSSH `ssh-keygen`, OpenSSL 3, or Python `cryptography`, then verifies the archive's SHA-256. It refuses to proceed without verification unless `DIAL_ALLOW_UNSIGNED_INSTALL=1` explicitly requests checksum-only installation. `install.ps1` verifies the signed manifest with its embedded key too.
Run `sh install.sh v0.1.1` (or `… | sh -s -- v0.1.1`) to select a specific release. On Windows, run the release's `install.ps1` in PowerShell for the native `dial.exe` (commands in [windows.md](windows.md)), or run the Linux commands in your WSL2 Ubuntu terminal; the PowerShell command in the WSL2 section below installs WSL2 first.

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
The macOS executable is not Apple Developer ID code-signed or notarized; the signed Ed25519
manifest authenticates archive digests separately. If macOS blocks execution, review your
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
This runs `cargo build -p dial --bin dial --profile local --locked`: an incremental build without the shipping profile's thin LTO. Dependencies stay optimized at level 2; Dial itself uses level 1 and more parallel codegen to reduce rebuild time. Published releases and `cargo install` still use the unchanged release profile. It copies the binary to `bin/dial` in this checkout (git-ignored), and installs it over the `dial` your PATH resolves to (or `~/.local/bin/dial` if none), also refreshing `~/.cargo/bin/dial` when present. Previous copies are kept as `dial.bak-<stamp>` (three most recent). Use `make update PROFILE=release` for shipping-equivalent optimization.
`cargo install --path . --locked` remains available when you specifically want a release build
installed into Cargo's bin directory instead. `command -v dial` shows which copy your terminal
will run, and `make help` lists the other build and check targets. Restart any running dial
session afterward. See `ARCHITECTURE.md` for the module layout.

For a faster development loop, use `make check` for compilation checks,
`make test-lib` for library unit tests, or `make test-module MOD=llm::provider`
for a single module (a module path is required; add `CRATE=dial-llm` to build only that
crate's test binary instead of every crate's). `make test-integration` runs the
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

Default builds retain voice, embedded shell, embedded JavaScript, the heavy `read` decoders
(`data-formats`: Parquet/Arrow, RAR, executables and object files), native desktop control and
AWS support. Slim builds omit these optional integrations, retaining the external shell, browser,
coding tools, the other document readers and non-AWS model providers. Without `data-formats`
(parquet and arrow alone are roughly 120 MB of build output) a slim build compiles noticeably
less and keeps a smaller cache. Bedrock bearer-token authentication remains
available; AWS credential-chain/SigV4 authentication requires `FEATURES=aws`. Native
desktop operations require `FEATURES=desktop`; the `voice`, `embedded-shell`, `embedded-js` and
`data-formats` features can also be restored individually. Use a quoted, comma-separated list for several,
for example `FEATURES='aws,desktop'`. Missing optional capabilities report an explicit
error rather than silently succeeding. Select the same flags on subsequent updates.
Native-only desktop helpers are compiled only with a supported desktop backend (or
their unit tests), so slim builds do not produce unused-native-code warnings.

`make build TIMINGS=1` (or `make update TIMINGS=1`) also writes Cargo's timing report to
`target/cargo-timings/cargo-timing.html`; it is off by default so routine builds do not
accumulate reports. Keep `target` between builds: the first build
of a profile or feature selection compiles its dependencies, and switching toolchains
or features can invalidate cached work. `CARGO_TARGET_DIR=/path/to/cache make update`
uses that persistent cache for both compilation and installation; its timing report
is under that directory instead. No clean step or toolchain update is part of `make update`.

Reducing build storage and rebuild time:

- `make clean` removes build output from this checkout: `target/` (or `CARGO_TARGET_DIR`), the
  per-session `.tmp/cargo-target/<session>` directories and preflight's `.tmp/preflight/target`.
  A directory a running Cargo build or a live Dial session still uses is skipped and reported, so
  it is safe to run while other sessions are open; run it again once they finish. It never touches
  sources, git worktrees, installed binaries or `~/.cargo/registry`. `make clean DRY_RUN=1` lists
  what would be removed and its size. The next build after a clean recompiles every dependency.
- Each build profile keeps its own full dependency cache. `make test` uses `dev` and `make build`
  uses `local`, so a typical checkout holds two. `make test-rust TEST_PROFILE=ci` reuses CI's
  smaller profile (no debug info, no incremental cache) at the cost of panic line numbers and
  incremental test rebuilds.
- Commands Dial runs build into a per-session `.tmp/cargo-target/<session>` directory (see
  [shell](shell.md#per-session-cargo-build-output)), so concurrent sessions each hold a copy of
  compiled dependencies. If you rarely run sessions in parallel, start Dial with an explicit
  `CARGO_TARGET_DIR` to share one cache; alternatively install a compiler cache such as
  [sccache](https://github.com/mozilla/sccache) and set `RUSTC_WRAPPER=sccache` so separate
  target directories and worktrees reuse compiled dependencies.
- Cargo's download cache (`~/.cargo/registry`) grows with every dependency version ever
  fetched; a tool such as `cargo cache --autoclean` (from the `cargo-cache` crate) trims it.

For a compile-only, unoptimized development binary, use `make build PROFILE=dev`.

For local MLX you can also prepare the runtime and model explicitly:
`dial setup -m mlx-community/Qwen2.5-7B-Instruct-4bit` (requires `uv`).
Discovery itself does not download weights. Selecting an uncached local model and submitting a
prompt resolves its weights; a missing default MLX runtime gets an installation confirmation.

### Publishing a release (maintainers)

Release preparation, verification and publication are separate:

```sh
scripts/release.sh 0.3.0          # metadata only; no build, stage, commit or tag
make release-check              # aggregate cheap source/tooling errors; no compilation
# Review, then commit and push main separately, only when authorized.
# Run the applicable expensive checks and wait for successful push-to-main CI.
scripts/release.sh 0.3.0 --tag    # explicit annotated tag of the already-tested commit
# Push only when publication is authorized:
git push origin v0.3.0
```

Preparation accepts a dirty checkout, sets the inherited workspace version, rolls nonempty
Unreleased notes into the dated version section, and refreshes only workspace lock entries
with offline Cargo resolution. It also repairs the Unreleased comparison and missing release
reference links, preserving historical commit links. Duplicate or malformed references are refused
before source edits. Repeating preparation, even at the current version, repairs link drift
without rolling the notes twice. `make test-packaging` and `make release-check` detect reference drift.
Preparation never compiles or silently downloads dependencies. An offline resolution failure leaves
the metadata edits available for inspection and retry; it does not certify a prepared tree.

`make release-check RELEASE_TAG=v0.3.0` checks stable tag/version agreement, inherited member
versions, local lock entries, exact nonempty release notes, packaging consistency, both release
shell scripts and required local commands/Python imports. It reports independent blockers in
one pass. It does not validate Cargo's complete dependency graph or Rust types and is not a
replacement for compilation, Clippy, native tests, artifact smoke checks or hosted acceptance.
Use Python 3.12 with the pinned check dependencies, for example:

```sh
uv run --no-project --python 3.12 --with-requirements scripts/requirements-ci.lock make release-check
```

`--tag` requires clean `main`, matching `origin/main`, passing no-build checks and completed
successful CI for that exact source: either the **push-to-main CI run for the commit** or the CI
run of the **merged pull request whose tested and built tree is identical** (see below), checked
with `gh`. It does not bump, commit, compile or push main. `--tag --push` additionally pushes the
tag; bare `--push` and the old `--no-check` bypass are refused. The workflow independently repeats
admission.

CI's shared no-build readiness/format job gates every compiling matrix and the container build.
While the workspace version has no tag, CI also calls `release-build.yml` to compile unsigned
candidates for all eight release targets **in parallel with the test/lint jobs**, on main pushes
and on pull requests from this repository (version-bump PRs in practice). A pull request records
candidates only when the merge with main that CI tests has the same tree as the PR head that is
built; a PR that is behind main gets none. Main skips the rebuild when a merged PR's CI already
recorded candidates for the same tree. Candidate jobs validate public keys, package and smoke-test
every archive, and record the built commit, its source tree, the version and the embedded public
key. They receive no private signing key or protected environment access. All candidate jobs must
succeed for that CI run to pass; this uses more concurrent runner capacity on release-bound
pushes, so queue time still depends on the account's concurrency allowance.

The tag workflow first looks for a pull request from this repository, merged into main with a
merge commit (its head is an ancestor of the tag), whose head has the tagged commit's tree and whose
newest successful CI run recorded candidates for that tree and tag. When found, it reuses that run's
tests and archives and does not wait for main CI;
this is what makes "merge the release PR, then tag" fast. Otherwise it requires successful
push-to-main CI for the exact SHA, as before, and promotes that run's candidates. Fork pull
requests, other trees, incomplete or expired sets and identities with a changed public key cannot
be promoted; those cases rebuild every platform with the same shared workflow. Released binaries
built on a pull request report its head commit in `dial --version`; that commit is in main's
history and has the tagged tree. Authentication/API errors fail visibly rather than silently
bypassing admission. The protected `release-signing` input check and final signing approval
remain mandatory when configured. The final job rechecks the candidate tree, tag and public key
before signing; build jobs never receive the private key. Workflow source alone cannot establish
hosted repository protections.

Release in one path: either push the prepared commit to main and tag it after main CI passes, or
open a release PR, merge it once its checks pass, and tag the merge commit right away. Doing both
repeats CI on the same tree.

Candidate Rust caches are saved from main, including after failure, so later main commits can
restore them; tag-scoped fallback builds restore main caches without creating isolated tag caches.
PR container builds restore the main layer cache but do not export competing PR cache entries.
The release container no longer needs a Rust compilation cache. Cache availability and speedups
are not guaranteed; Docker cache mounts are not themselves persisted by the layer-cache exporter.

Release candidate jobs also cache pip downloads and locally built wheels, keyed by Python and
`scripts/requirements-ci.lock`, so the Intel macOS packaging job can reuse its source-built
`cryptography` wheel. Installation still requires the locked hashes. Only main enables this
cache; PRs and tag fallback builds do not, avoiding extra branch-scoped cache entries. A cold or
evicted cache still builds normally.

The `x86_64-apple-darwin` candidate is cross-compiled on an Apple Silicon `macos-15` runner
instead of the Intel runner, whose release build took about 24 minutes and set the candidate
critical path. Its packaged binary is format-checked and smoke-tested under Rosetta 2, which the
job installs if missing.

#### Runner capacity and downstream failures

Intel macOS is still built, smoke-tested and published, but CI no longer runs the test suite on
Intel hardware: that job took 30 to 60 minutes and held every release behind it. Intel-specific
regressions that the Rosetta smoke test does not exercise can therefore reach a release.
Provisioning a paid runner or increasing the repository cache quota requires an owner decision.
Do not send untrusted PRs to a persistent privileged self-hosted runner.

The publish job writes the binary release URL to its summary before downstream jobs start. If
package metadata or container publication fails, rerun only the failed jobs with
`gh run rerun <release-run-id> --failed`; do not retag, rebuild or republish successful binaries.
Those jobs retain visible failures. The release workflow does not refresh dial.rootatlas.dev;
the site picks up a new release when its cached copy of GitHub's latest release expires.

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
- the workspace tests under `cargo-nextest`, as CI runs them (`brew install cargo-nextest`;
  without it `cargo test` runs the same tests one binary at a time, several minutes slower), in the
  system temp directory (on macOS this sits behind the `/var` -> `/private/var` symlink, as on CI
  runners, so path-comparison bugs surface locally), then the installed binary's artifact smoke
  test and the real-terminal smoke scripts.

Cross targets compile their C dependencies with `zig` (`scripts/zig-cc`); install it
(`brew install zig`) and the rustup targets, or those steps are reported as not covered.
Tests run only on the host, so Linux-only tests (sandbox enforcement) and the native Windows suite
still run only in CI; they are compile-checked locally. `make preflight-quick` first runs cheap
readiness and formatting, then the compile/lint gates. Full preflight also runs portable contracts
before expensive workspace compilation. Cheap failures are collected together and stop the run
before compile/lint work, even with `PREFLIGHT_KEEP_GOING=1`; that flag otherwise keeps collecting
failures in the expensive phase. Logs are in `.tmp/preflight/logs/`.

`make hooks` points `core.hooksPath` at `.githooks`, whose `pre-push` hook runs `make preflight`
on every commit pushed to a branch and refuses the push if it fails. `DIAL_PREFLIGHT=quick git
push` runs only the compile and lint gates; `DIAL_PREFLIGHT=skip git push` (or `--no-verify`)
bypasses it. Runs in one checkout are serialized.

CI runs on main pushes and pull requests. Release candidates are built only on main while the
workspace version is untagged; documentation-only updates after a tag do not rebuild candidates.

`.github/workflows/release.yml` does not re-run the test suite. It requires a successful `CI` run
(formatting, clippy, the full Rust suite including Linux sandbox probes, and terminal smoke tests)
for the exact tagged commit, waiting while that run is in progress; without one it fails. Push
the release-prepared commit to main and let CI and its parallel candidate builds pass before
tagging. The tag workflow reuses those archives, or rebuilds through the identical shared workflow
if candidates are unavailable. Every extracted archive is smoke-tested outside the checkout.
after **all** platform builds (macOS arm64 and Intel, Linux x86-64 and arm64 in glibc and musl, Windows x86-64 and arm64) pass,
does it create a GitHub release with the archives, bundled MIT license and README, a
combined `SHA256SUMS` with its `SHA256SUMS.sig` and `SHA256SUMS.sshsig` signatures, and curated notes
from the tag's exact version section in `CHANGELOG.md` (missing, duplicate or empty sections refuse publication).
The container job pins Cosign installer v4.1.2 and Cosign v3.0.6; its OIDC signing and verification still
require a successful hosted run. CI runs the full portable test suite once, on Ubuntu, and a
focused macOS contract (`scripts/macos-contract.txt`) on macOS: the modules with macOS-specific
code (Seatbelt sandbox, APFS clones, power assertions, launchd schedules, pasteboard, keychain,
desktop capture, browser discovery, self-update) plus sandboxed isolation, shell-session and
terminal-lifecycle integration tests. Repeating the portable suite on macOS took about 11 of the
job's 20 minutes in CI run `42935891367`. Locally, its 28 filters ran 3,196 tests in about 7
minutes, including compilation, on Apple Silicon. Add a filter with any new macOS-specific code;
`make preflight` on a macOS host still runs the full suite before pushing. Ubuntu builds the
`dial` library and its unit-test build one after the other: each peaks near 5 GB in rustc. The native Windows Rust contract (`scripts/windows-contract.txt`, which lists its skipped tests with reasons) runs in CI's `windows-rust` job, so the CI requirement above covers it; the release does not repeat it. The Windows release jobs (x86-64 and ARM64) build `dial.exe`, smoke-test the packaged archive (`--version`, `--help`), verify the binary format for the target, and run `scripts/test-install.ps1` under Windows PowerShell 5.1 and PowerShell 7, as does the CI `installer-windows` job. The publish job builds `install.ps1` with the release key embedded and publishes it with `SHA256SUMS`, `SHA256SUMS.sig`, and `SHA256SUMS.sshsig`.

The Windows contract selects each library's owning package rather than compiling every
workspace test harness for each filter. Its migrated-record privacy assertion checks native
owner-only permissions (Unix mode bits or Windows ACLs), not synthetic Windows POSIX bits.
The Unix CI smoke tier stages the executable already produced by the workspace build into
`target/installed/bin`, then runs the same artifact and recovery checks; it does not invoke
`cargo install` and compile another copy. Real-terminal surface fixtures use the visible
`/sessions this` palette route and Ctrl+C to interrupt a running turn; Escape only dismisses UI.

CI caches compiled dependencies per job with `Swatinem/rust-cache`. Caches are saved only from
`main`, and also after a failing run, so one red test does not force the next run on that OS to
rebuild from scratch. The repository's default 10 GB cache quota evicts the least recently used
entries first, so the Linux feature jobs share one cache per runner image, saved only by the
largest combination on that image. Ubuntu 22.04 Clippy restores that same dependency cache,
without becoming a competing writer; macOS/Windows lint caches keep their separate keys.
Per-combination caches previously crowded out the macOS test caches. Sharing compatible
check artifacts reduces duplication but does not guarantee cache residency; inspect cache hits
and repository usage before increasing the quota or deleting useful build artifacts.

Push and pull-request CI compile three Linux feature selections: minimal (no optional
features), the default feature set, and all features. `.github/workflows/linux-features.yml`
owns the shared compile job and runs the exhaustive 32-combination powerset weekly (Monday
04:23 UTC). Maintainers can also dispatch **Linux feature coverage** manually, with exhaustive
coverage enabled by default; run that tier before a release when a change touches feature
gating. Per-push coverage does not prove that each optional feature compiles alone or with
every other; a gating mistake can surface only in the weekly run. PipeWire selections use
Ubuntu 24.04; other selections use Ubuntu 22.04.

CI runs format, build and the full test suite on `macos-15` and Ubuntu. The installed-binary
smoke tests, real-terminal lifecycle scripts, slim build check and runtime overhead recording
run on Ubuntu only; `make preflight` runs the smoke and real-terminal suites on a developer's
macOS host. Release candidates cross-compile the `x86_64-apple-darwin` archive on `macos-15` and
smoke-test it under Rosetta 2.

The musl compile checks run on pull requests and on pushes that do not build release
candidates; a main push that builds candidates compiles both musl targets there instead. The
source container image (`.github/workflows/docker.yml`) builds only when the Dockerfiles,
compose files, `Cargo.lock`, the toolchain pin or that workflow change. Source changes that break
only the container build are caught by the release workflow, which builds and smoke-tests the
published image.

The workflow preserves repository visibility. Dial is MIT licensed; see `LICENSE` for
permission to use, modify, and redistribute it.
