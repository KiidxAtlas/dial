#!/usr/bin/env bash
# Run CI's checks locally on an exact commit before it reaches GitHub.
#
#   scripts/preflight.sh               # check HEAD (a clean snapshot, not the working tree)
#   scripts/preflight.sh <rev>         # check another commit
#   scripts/preflight.sh --in-place    # check the working tree as it is (release.sh uses this)
#   scripts/preflight.sh --quick ...   # compile/lint gates only: fmt, clippy, cross-target checks
#   scripts/preflight.sh --force ...   # re-run even if this tree already passed
#
# The snapshot is a detached worktree in .tmp/preflight/wt with a persistent target directory, so
# repeated runs are incremental and other sessions' uncommitted edits never leak into the result.
# A tree that passed is recorded in .tmp/preflight/passed/ and is not checked again.
#
# Mirrors .github/workflows/ci.yml: fmt; clippy -D warnings for Linux (CI's lint job); cargo-deny;
# musl checks; a Windows compile check; installer/build/packaging/driver/Python contracts; the
# workspace tests in the system temp directory; the slim build; installed-binary smoke and
# real-terminal suites. Cross targets compile with zig (scripts/zig-cc). Tests run only on this
# host: Linux- and Windows-only test failures are compile-checked here, not executed.
#
# Everything runs inside main(), so bash has parsed the whole file before the first step: editing
# this script (for example by checking out another commit) cannot corrupt a run in progress.
set -euo pipefail

usage() { sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

failed=()
skipped=()

step() { # step <name> <command...>
  local name="$1"
  shift
  local log="$state/logs/$name.log" start=$SECONDS
  printf '  %-28s ' "$name"
  if "$@" >"$log" 2>&1; then
    printf 'ok   %4ss\n' $((SECONDS - start))
  else
    printf 'FAIL %4ss  (log: %s)\n' $((SECONDS - start)) "$log"
    tail -n 30 "$log" | sed 's/^/      /'
    failed+=("$name")
    [[ "${PREFLIGHT_KEEP_GOING:-0}" == 1 ]] || finish
  fi
}

skip() { # skip <name> <reason>
  printf '  %-28s skip  %s\n' "$1" "$2"
  skipped+=("$1: $2")
}

finish() {
  if ((${#skipped[@]})); then
    printf 'preflight: not covered here:\n'
    printf '  - %s\n' "${skipped[@]}"
  fi
  if ((${#failed[@]})); then
    echo "preflight: FAILED (${failed[*]}) for $label"
    exit 1
  fi
  echo "preflight: passed for $label"
  [[ -z "$key" ]] || date '+%Y-%m-%d %H:%M:%S' >"$state/passed/$key"
  exit 0
}

cross() { # cross <rust target> <step name> <cargo subcommand> <args...>
  local target="$1" name="$2" sub="$3"
  shift 3
  if [[ "$target" == "$host" ]]; then
    step "$name" cargo "$sub" "$@"
  elif [[ "$have_zig" == 0 ]]; then
    skip "$name" "install zig to cross-check $target"
  elif ! grep -qx "$target" <<<"$installed_targets"; then
    skip "$name" "rustup target add $target"
  else
    # --target right after the subcommand: anything after `--` goes to rustc, not cargo.
    step "$name" cargo "$sub" --target "$target" "$@"
  fi
}

acquire_lock() { # one run at a time per checkout: the snapshot and target directory are shared
  local lock="$state/lock" owner
  until mkdir "$lock" 2>/dev/null; do
    owner="$(cat "$lock/pid" 2>/dev/null || true)"
    if [[ -n "$owner" ]] && ! kill -0 "$owner" 2>/dev/null; then
      rm -rf "$lock"
      continue
    fi
    echo "preflight: waiting for another run (pid ${owner:-?})" >&2
    sleep 10
  done
  echo $$ >"$lock/pid"
  trap "rm -rf '$lock'" EXIT
}

main() {
  local quick=0 force=0 in_place=0 rev=HEAD arg
  for arg in "$@"; do
    case "$arg" in
      --quick) quick=1 ;;
      --force) force=1 ;;
      --in-place) in_place=1 ;;
      -*) usage ;;
      *) rev="$arg" ;;
    esac
  done

  root="$(git rev-parse --show-toplevel)"
  state="$root/.tmp/preflight"
  mkdir -p "$state/passed" "$state/logs"
  export CARGO_TARGET_DIR="${PREFLIGHT_TARGET_DIR:-$state/target}"

  # rust-toolchain.toml pins the compiler; a Homebrew cargo earlier on PATH would ignore it.
  local toolchain_bin
  toolchain_bin="$(cd "$root" && dirname "$(rustup which cargo)")"
  export PATH="$toolchain_bin:$PATH"

  local dir commit tree done
  key=""
  if [[ "$in_place" == 1 ]]; then
    dir="$root"
    label="working tree"
  else
    commit="$(git rev-parse --verify "$rev^{commit}")"
    tree="$(git rev-parse "$commit^{tree}")"
    label="${commit:0:12}"
    # A full pass also satisfies a quick request; a quick pass never satisfies a full one.
    key="$tree"
    [[ "$quick" == 0 ]] || key="$tree-quick"
    if [[ "$force" == 0 ]]; then
      for done in "$state/passed/$tree" "$state/passed/$key"; do
        if [[ -e "$done" ]]; then
          echo "preflight: $label already passed ($(basename "$done"), $(cat "$done"))"
          exit 0
        fi
      done
    fi
    dir="$state/wt"
  fi

  acquire_lock
  if [[ "$in_place" == 0 ]]; then
    if git -C "$dir" rev-parse --git-dir >/dev/null 2>&1; then
      git -C "$dir" checkout -q --detach --force "$commit"
      git -C "$dir" clean -q -fdx
    else
      git worktree prune
      rm -rf "$dir"
      git worktree add -q --detach "$dir" "$commit"
    fi
  fi
  cd "$dir"
  host="$(rustc -vV | sed -n 's/^host: //p')"
  echo "preflight: checking $label in $dir (rustc $(rustc --version | cut -d' ' -f2), $host)"

  # Cross-target C dependencies (ring, zstd, ...) compile with zig instead of per-target toolchains.
  have_zig=0
  if command -v zig >/dev/null 2>&1; then
    have_zig=1
    local zcc="$root/scripts/zig-cc" t
    export CC_x86_64_unknown_linux_gnu="$zcc x86_64-linux-gnu.2.35"
    export CC_x86_64_unknown_linux_musl="$zcc x86_64-linux-musl"
    export CC_aarch64_unknown_linux_musl="$zcc aarch64-linux-musl"
    export CC_x86_64_pc_windows_gnu="$zcc x86_64-windows-gnu"
    for t in x86_64_unknown_linux_gnu x86_64_unknown_linux_musl aarch64_unknown_linux_musl x86_64_pc_windows_gnu; do
      export "AR_$t=zig ar"
    done
  fi
  installed_targets="$(rustup target list --installed)"

  step fmt cargo fmt --check
  # CI's lint job runs on Linux, where cfg(target_os = "linux") code is linted too.
  cross x86_64-unknown-linux-gnu clippy-linux clippy --locked --workspace --all-targets -- -D warnings
  step clippy-host cargo clippy --locked --workspace --all-targets -- -D warnings
  cross x86_64-unknown-linux-musl musl-x86_64 check --locked --profile ci -p dial
  cross aarch64-unknown-linux-musl musl-aarch64 check --locked --profile ci -p dial
  # CI builds Windows with MSVC; the GNU target type-checks the same cfg(windows) code.
  cross x86_64-pc-windows-gnu windows-check check --locked --profile ci --workspace --all-targets
  step slim-check cargo check --locked --profile ci --no-default-features -p dial --bin dial
  if command -v cargo-deny >/dev/null 2>&1; then
    step cargo-deny cargo deny check advisories licenses bans sources
  else
    skip cargo-deny "cargo install cargo-deny"
  fi
  [[ "$quick" == 0 ]] || finish

  # CI runs every Python step on 3.12 with these packages installed; use the same in a throwaway
  # uv environment rather than whatever python3 this host has.
  local py=()
  if command -v uv >/dev/null 2>&1; then
    py=(uv run --quiet --no-project --python 3.12 --with pytest --with fastapi==0.141.1
      --with pydantic --with uvicorn --with cryptography)
  else
    skip python-env "install uv to run Python steps on CI's 3.12 with its packages; using python3"
  fi
  pyrun() { ${py[@]+"${py[@]}"} "$@"; }

  step test-drivers pyrun make test-drivers
  step test-install pyrun bash scripts/test-install.sh
  step test-build pyrun python3 scripts/test-build.py
  step test-windows-contract pyrun python3 scripts/test-windows-contract.py
  step packaging pyrun sh -c 'python3 scripts/test-packaging.py && python3 scripts/check-packaging.py'
  step test-python pyrun make test-python PYTHON=python3
  step test-bench-offline pyrun make test-bench-offline PYTHON=python3

  # macOS runners resolve the temp directory through /var -> /private/var; use it as CI does.
  local tmp
  tmp="$(getconf DARWIN_USER_TEMP_DIR 2>/dev/null || printf '%s' /tmp)"
  step test-rust env TMPDIR="$tmp" make test-rust TEST_PROFILE=ci
  local installed="$CARGO_TARGET_DIR/installed" s
  step install cargo install --locked --path . --profile ci --root "$installed" --force
  local dial="$installed/bin/dial"
  step test-smoke pyrun make test-smoke DIAL="$dial"
  for s in interactive_smoke interactive_surfaces session_contention_smoke side_lifecycle_smoke; do
    step "$s" pyrun python3 "examples/benchmarks/$s.py" --dial "$dial" --captures "$CARGO_TARGET_DIR/pty-captures/$s"
  done
  if [[ "$host" != *linux* ]]; then
    skip linux-tests "Linux-only tests (sandbox enforcement, bubblewrap) run in CI's ubuntu job"
  fi
  skip windows-tests "the native Windows contract suite and install.ps1 run in CI's windows jobs"
  finish
}

main "$@"
