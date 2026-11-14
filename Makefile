CARGO ?= cargo
# Use PROFILE=release for shipping-equivalent optimization (thin LTO).
PROFILE ?= local
# Keep builds and installation on the same persistent cache, including custom locations.
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR
BUILD_DIR = $(CARGO_TARGET_DIR)/$(if $(filter dev,$(PROFILE)),debug,$(PROFILE))
# Slim omits optional integrations; FEATURES can add individual capabilities back.
SLIM ?= 0
FEATURES ?=
CARGO_FEATURE_FLAGS = $(if $(filter 1,$(SLIM)),--no-default-features) $(if $(strip $(FEATURES)),--features "$(FEATURES)")
# TIMINGS=1 writes Cargo's HTML timing report (cargo-timings/) for `make build` and `make update`.
TIMINGS ?= 0
TIMINGS_FLAG = $(if $(filter 1,$(TIMINGS)),--timings)
INSTALL_DIR ?= $(shell path="$$(command -v dial 2>/dev/null)"; if [ -n "$$path" ]; then dirname "$$path"; else printf '%s' "$(HOME)/.local/bin"; fi)
# Timestamped `dial.bak-*` copies kept per installed location by `make update` (newest first).
KEEP_BACKUPS ?= 3
# Checks never install dependencies or download models. Python tests report optional skips.
PYTHON ?= python3
NODE ?= node
TEST_PROFILE ?= dev
# Nextest's reporting profile is independent of Cargo's optimization profile.
NEXTEST_PROFILE ?= default
DIAL ?= $(BUILD_DIR)/dial

# Tests that run the embedded shell need a freshly built `dial` host (the test binary cannot act as
# one). HOST_RUN builds it with the same profile and features as the test command, smoke-checks it,
# and exports DIAL_EMBEDDED_SHELL_HOST for that command only; see scripts/embedded-host.py.
# Callers append `-- <cargo command>` after their feature flags.
HOST_RUN = $(PYTHON) scripts/embedded-host.py --cargo "$(CARGO)"


.DEFAULT_GOAL := help
.PHONY: help update install build check test test-lib test-integration test-module test-nextest lint test-rust test-drivers test-python test-bench-offline bench-headline test-smoke test-mlx preflight preflight-quick hooks
.PHONY: test-install test-build test-windows-contract test-embedded-host test-packaging test-release-policy preflight-worktree
.PHONY: release-check lint-deps clean test-clean
.PHONY: test-tool-audit audit-tools

help:
	@printf '%s\n' \
		'make update   Rebuild and replace the installed dial from this checkout (keeps KEEP_BACKUPS=3 backups)' \
		'make install  Same as update' \
		'make build    Build locally (SLIM=1 omits optional integrations; PROFILE=release for shipping; TIMINGS=1 writes a Cargo timing report)' \
		'make clean    Remove build output (target/, idle session build dirs); skips dirs in use (DRY_RUN=1 lists only)' \
		'make check    Check Rust compilation' \
		'make test     Run the Rust tests' \
		'make test-lib Run every workspace crate'"'"'s library unit tests (no CLI/integration targets)' \
		'make test-integration  Run the single integration test binary (tests/integration)' \
		'make test-module MOD=llm::provider  Run lib tests under one module path (CRATE=dial-llm builds only that crate)' \
		'make lint     Run clippy with the CI lint gate' \
		'make lint-deps  Detect unused workspace dependencies (requires cargo-machete; never fixes manifests)' \
		'make test-rust TEST_PROFILE=dev  Run portable Rust tests (CI uses ci)' \
		'make test-nextest  Run the Rust tests under cargo-nextest (no retries, hung-test timeouts; .config/nextest.toml)' \
		'make test-drivers  Run browser, JavaScript eval and Python kernel transport contracts (Node 22+, Python 3)' \
		'make test-python  Run Python contracts; report unavailable optional MLX checks' \
		'make test-bench-offline  Run benchmark grader/reporting tests, no model or binary needed' \
		'make test-tool-audit  Validate all tool-family ownership and test audit evidence/closure gates (offline)' \
		'make audit-tools AUDIT_TIER=rust AUDIT_OUT=benchmarks/audit-run  Execute reviewed tool checks and retain evidence' \
		'make bench-headline DIAL=target/local/dial [BASELINE=old.json]  Run the headline local-tasks score (needs a loaded model in HEADLINE_PROFILES)' \
		'make test-packaging  Check distribution metadata and packaging contracts' \
		'make test-release-policy  Check publication authorization and version ordering' \
		'make release-check  No-build source, packaging, shell and tooling checks (RELEASE_TAG=v0.3.0 optional)' \
		'make test-build  Test local build/install commands without replacing your installed binary' \
		'make test-install  Test the Unix release installer (Python cryptography required)' \
		'make test-windows-contract  Test the focused Windows contract runner' \
		'make test-embedded-host  Test embedded shell host preparation (mocked Cargo; no build)' \
		'make test-smoke DIAL=target/local/dial  Check an already-built binary and scripted recovery' \
		'make test-mlx  Run optional local MLX checks; explicitly skip unsupported hosts, never download' \
		'make preflight  Run every CI check locally on a clean snapshot of HEAD (REV=<commit> for another)' \
		'make preflight-quick  Cheap readiness and fmt, then clippy (host + Linux), musl, Windows, slim compilation' \
		'make preflight-worktree  Run preflight on current staged/unstaged files without committing' \
		'make hooks    Run make preflight automatically before every git push (pre-push hook)'

update:
	$(PYTHON) scripts/install-dev.py check --keep "$(KEEP_BACKUPS)"
	$(CARGO) build -p dial --bin dial --profile $(PROFILE) --locked $(TIMINGS_FLAG) $(CARGO_FEATURE_FLAGS)
	mkdir -p bin "$(INSTALL_DIR)"
	$(PYTHON) scripts/install-dev.py install --keep "$(KEEP_BACKUPS)" --build "$(BUILD_DIR)/dial" --bin-target bin/dial --install-dir "$(INSTALL_DIR)" --cargo-bin "$(HOME)/.cargo/bin/dial"
	@active="$$(command -v dial 2>/dev/null)"; \
	if [ -z "$$active" ] || ! cmp -s "$$active" "$(BUILD_DIR)/dial"; then \
		printf 'ERROR: PATH selects %s, which differs from the installed worktree build.\n' "$$active" >&2; \
		exit 1; \
	fi
	@$(PYTHON) scripts/install-dev.py prune --keep "$(KEEP_BACKUPS)" --bin-target bin/dial --install-dir "$(INSTALL_DIR)" --cargo-bin "$(HOME)/.cargo/bin/dial"
	@printf 'Updated %s and %s\n' '$(CURDIR)/bin/dial' '$(INSTALL_DIR)/dial'
	@printf '%s\n' 'If your shell cached another dial path, run: hash -r (bash) or rehash (zsh).'


install: update

build:
	$(CARGO) build -p dial --bin dial --profile $(PROFILE) --locked $(TIMINGS_FLAG) $(CARGO_FEATURE_FLAGS)

# Remove build output (target/, idle per-session .tmp/cargo-target dirs, preflight's cache).
# Directories used by a running Cargo build or live Dial session are skipped. DRY_RUN=1 lists only.
clean:
	$(PYTHON) scripts/clean.py --target-dir "$(CARGO_TARGET_DIR)" $(if $(filter 1,$(DRY_RUN)),--dry-run)

test-clean:
	$(PYTHON) scripts/test-clean.py

check:
	$(CARGO) check --locked --workspace $(CARGO_FEATURE_FLAGS)

test: test-rust

test-rust:
	$(PYTHON) scripts/test-prerequisites.py
	$(HOST_RUN) --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS) -- $(CARGO) test --locked --workspace --no-fail-fast --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS)

test-lib:
	$(HOST_RUN) $(CARGO_FEATURE_FLAGS) -- $(CARGO) test --locked --workspace --lib $(CARGO_FEATURE_FLAGS)

test-integration:
	$(HOST_RUN) $(CARGO_FEATURE_FLAGS) -- $(CARGO) test --locked -p dial --test integration $(CARGO_FEATURE_FLAGS)

# Fast loop for one lib module: make test-module MOD=llm::provider (searches every workspace crate).
# CRATE=dial-llm builds only that crate's test binary instead of every crate's.
test-module:
	@test -n "$(MOD)" || { printf '%s\n' 'Usage: make test-module MOD=llm::provider [CRATE=dial-llm]' >&2; exit 2; }
	$(HOST_RUN) $(CARGO_FEATURE_FLAGS) -- $(CARGO) test --locked $(if $(strip $(CRATE)),-p $(CRATE),--workspace) --lib $(CARGO_FEATURE_FLAGS) $(MOD)

test-nextest:
	$(PYTHON) scripts/test-prerequisites.py
	@command -v cargo-nextest >/dev/null 2>&1 || { printf '%s\n' 'cargo-nextest is not installed: https://nexte.st (or use make test-rust)' >&2; exit 2; }
	$(HOST_RUN) --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS) -- $(CARGO) nextest run --locked --workspace --no-fail-fast --cargo-profile $(TEST_PROFILE) --profile $(NEXTEST_PROFILE) $(CARGO_FEATURE_FLAGS)
	$(HOST_RUN) --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS) -- $(CARGO) test --locked --workspace --doc --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS)

lint:
	$(CARGO) clippy --locked --workspace --all-targets $(CARGO_FEATURE_FLAGS) -- -D warnings

# Scope the audit to workspace manifests, not standalone benchmark/architecture fixtures.
# An absolute root manifest avoids cargo-machete 0.9.2's empty-parent path error.
lint-deps:
	$(CARGO) machete --skip-target-dir "$(CURDIR)/Cargo.toml" "$(CURDIR)/crates"

test-drivers:
	DIAL_TEST_PYTHON="$(PYTHON)" $(NODE) --test crates/dial-tools/src/tools/net/browser/driver.test.cjs crates/dial-tools/src/tools/exec/eval/driver.test.cjs assets/collab-web/viewer.test.cjs

test-python:
	$(PYTHON) scripts/test-prerequisites.py python
	$(PYTHON) scripts/test-prerequisites-unit.py
	HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 $(PYTHON) -m pytest -rs python/test_*.py
	$(PYTHON) -m unittest discover -s sdk/python/tests

test-bench-offline:
	$(PYTHON) -m unittest discover -s examples/benchmarks -p 'test_*.py'

test-tool-audit:
	$(PYTHON) examples/benchmarks/tool_audit.py validate
	$(PYTHON) -m unittest discover -s examples/benchmarks -p 'test_tool_audit.py'

AUDIT_TIER ?= offline
AUDIT_OUT ?=
audit-tools:
	@test -n "$(AUDIT_OUT)" || { printf '%s\n' 'Set a new AUDIT_OUT under benchmarks/; existing evidence is never overwritten.' >&2; exit 2; }
	$(PYTHON) examples/benchmarks/tool_audit.py run --tier "$(AUDIT_TIER)" --out "$(AUDIT_OUT)"

# Headline task score: the fixed local-tasks suite, run against an already-loaded model. Reports
# go to a durable path under the target dir, never $TMPDIR, so an interrupted run keeps its
# progress journal. BASELINE compares against an earlier headline report (correctness first).
HEADLINE_PROFILES ?= examples/benchmarks/local-tasks-profiles.toml
HEADLINE_REPORT := $(CARGO_TARGET_DIR)/benchmarks/headline-$(shell date +%Y%m%d-%H%M%S).json
BASELINE ?=
bench-headline:
	@status=0; \
	"$(DIAL)" benchmark examples/benchmarks/local-tasks.toml --profiles "$(HEADLINE_PROFILES)" --report "$(HEADLINE_REPORT)" || status=$$?; \
	if [ -n "$(BASELINE)" ] && [ -f "$(HEADLINE_REPORT)" ]; then \
		$(PYTHON) examples/benchmarks/compare.py "$(BASELINE)" "$(HEADLINE_REPORT)" || status=1; \
	fi; \
	exit $$status

test-install:
	bash scripts/test-install.sh

test-build:
	$(PYTHON) scripts/test-build.py

test-windows-contract:
	$(PYTHON) scripts/test-windows-contract.py

test-embedded-host:
	$(PYTHON) scripts/test-embedded-host.py

test-packaging:
	$(PYTHON) scripts/test-packaging.py
	$(PYTHON) scripts/check-packaging.py

test-release-policy:
	$(PYTHON) scripts/test-prerequisites.py release-policy
	$(PYTHON) scripts/test-release-policy.py
	$(PYTHON) scripts/test-benchmark-release.py

# No compilation, dependency installation, network, staging or publication. Aggregate cheap
# failures so callers see all prerequisites before deciding to run an expensive tier.
RELEASE_TAG ?=
release-check:
	@status=0; \
	$(PYTHON) scripts/release-policy.py check $(if $(RELEASE_TAG),"$(RELEASE_TAG)") || status=1; \
	$(PYTHON) scripts/check-packaging.py || status=1; \
	$(PYTHON) scripts/release-policy.py tools || status=1; \
	for script in scripts/release.sh scripts/release-candidate-run.sh scripts/preflight.sh; do bash -n "$$script" || status=1; done; \
	exit $$status

# Deliberately no build prerequisite: test exactly the artifact selected by DIAL.
test-smoke:
	$(PYTHON) scripts/check-artifact.py --dial "$(DIAL)"
	$(PYTHON) examples/benchmarks/smoke.py --dial "$(DIAL)" --suite recovery --machine-repetitions 2

test-mlx:
	HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 $(PYTHON) scripts/test-mlx.py

# CI's checks on a clean snapshot of a commit (default HEAD), cross-compiling the Linux, musl and
# Windows targets with zig; see scripts/preflight.sh. Results are cached per tree.
REV ?= HEAD
preflight:
	scripts/preflight.sh "$(REV)"

preflight-quick:
	scripts/preflight.sh --quick "$(REV)"

preflight-worktree:
	scripts/preflight.sh --in-place

hooks:
	git config core.hooksPath .githooks
	@printf '%s\n' 'pre-push now runs scripts/preflight.sh; use DIAL_PREFLIGHT=quick git push for compile/lint gates or DIAL_PREFLIGHT=skip git push to bypass once.'
