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
INSTALL_DIR ?= $(shell path="$$(command -v dial 2>/dev/null)"; if [ -n "$$path" ]; then dirname "$$path"; else printf '%s' "$(HOME)/.local/bin"; fi)
# Timestamped `dial.bak-*` copies kept per installed location by `make update` (newest first).
KEEP_BACKUPS ?= 3
# Checks never install dependencies or download models. Python tests report optional skips.
PYTHON ?= python3
NODE ?= node
TEST_PROFILE ?= dev
DIAL ?= $(BUILD_DIR)/dial


.DEFAULT_GOAL := help
.PHONY: help update install build check test test-lib test-integration test-module test-nextest lint test-rust test-drivers test-python test-bench-offline test-smoke test-mlx preflight preflight-quick hooks
.PHONY: test-install test-build test-windows-contract test-packaging test-release-policy preflight-worktree
.PHONY: release-check

help:
	@printf '%s\n' \
		'make update   Rebuild and replace the installed dial from this checkout (keeps KEEP_BACKUPS=3 backups)' \
		'make install  Same as update' \
		'make build    Build locally with timings (SLIM=1 omits optional integrations; PROFILE=release for shipping)' \
		'make check    Check Rust compilation' \
		'make test     Run the Rust tests' \
		'make test-lib Run every workspace crate'"'"'s library unit tests (no CLI/integration targets)' \
		'make test-integration  Run the single integration test binary (tests/integration)' \
		'make test-module MOD=llm::provider  Run lib tests under one module path' \
		'make lint     Run clippy with the CI lint gate' \
		'make test-rust TEST_PROFILE=dev  Run portable Rust tests (CI uses ci)' \
		'make test-nextest  Run the Rust tests under cargo-nextest (no retries, hung-test timeouts; .config/nextest.toml)' \
		'make test-drivers  Run browser, JavaScript eval and Python kernel transport contracts (Node 22+, Python 3)' \
		'make test-python  Run Python contracts; report unavailable optional MLX checks' \
		'make test-bench-offline  Run benchmark grader/reporting tests, no model or binary needed' \
		'make test-packaging  Check distribution metadata and packaging contracts' \
		'make test-release-policy  Check publication authorization and version ordering' \
		'make release-check  No-build source, packaging, shell and tooling checks (RELEASE_TAG=v0.3.0 optional)' \
		'make test-build  Test local build/install commands without replacing your installed binary' \
		'make test-install  Test the Unix release installer (Python cryptography required)' \
		'make test-windows-contract  Test the focused Windows contract runner' \
		'make test-smoke DIAL=target/local/dial  Check an already-built binary and scripted recovery' \
		'make test-mlx  Run optional local MLX checks; explicitly skip unsupported hosts, never download' \
		'make preflight  Run every CI check locally on a clean snapshot of HEAD (REV=<commit> for another)' \
		'make preflight-quick  Cheap readiness and fmt, then clippy (host + Linux), musl, Windows, slim compilation' \
		'make preflight-worktree  Run preflight on current staged/unstaged files without committing' \
		'make hooks    Run make preflight automatically before every git push (pre-push hook)'

update:
	$(PYTHON) scripts/install-dev.py check --keep "$(KEEP_BACKUPS)"
	$(CARGO) build -p dial --bin dial --profile $(PROFILE) --locked --timings $(CARGO_FEATURE_FLAGS)
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
	$(CARGO) build -p dial --bin dial --profile $(PROFILE) --locked --timings $(CARGO_FEATURE_FLAGS)

check:
	$(CARGO) check --locked --workspace $(CARGO_FEATURE_FLAGS)

test: test-rust

test-rust:
	$(PYTHON) scripts/test-prerequisites.py
	$(CARGO) test --locked --workspace --no-fail-fast --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS)

test-lib:
	$(CARGO) test --locked --workspace --lib $(CARGO_FEATURE_FLAGS)

test-integration:
	$(CARGO) test --locked -p dial --test integration $(CARGO_FEATURE_FLAGS)

# Fast loop for one lib module: make test-module MOD=llm::provider (searches every workspace crate)
test-module:
	@test -n "$(MOD)" || { printf '%s\n' 'Usage: make test-module MOD=llm::provider' >&2; exit 2; }
	$(CARGO) test --locked --workspace --lib $(CARGO_FEATURE_FLAGS) $(MOD)

test-nextest:
	$(PYTHON) scripts/test-prerequisites.py
	@command -v cargo-nextest >/dev/null 2>&1 || { printf '%s\n' 'cargo-nextest is not installed: https://nexte.st (or use make test-rust)' >&2; exit 2; }
	$(CARGO) nextest run --locked --workspace --no-fail-fast --cargo-profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS)
	$(CARGO) test --locked --workspace --doc --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS)

lint:
	$(CARGO) clippy --locked --workspace --all-targets $(CARGO_FEATURE_FLAGS) -- -D warnings

test-drivers:
	DIAL_TEST_PYTHON="$(PYTHON)" $(NODE) --test crates/dial-tools/src/tools/net/browser/driver.test.cjs crates/dial-tools/src/tools/exec/eval/driver.test.cjs assets/collab-web/viewer.test.cjs

test-python:
	$(PYTHON) scripts/test-prerequisites.py python
	HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 $(PYTHON) -m pytest -rs python/test_*.py

test-bench-offline:
	$(PYTHON) -m unittest discover -s examples/benchmarks -p 'test_*.py'

test-install:
	bash scripts/test-install.sh

test-build:
	$(PYTHON) scripts/test-build.py

test-windows-contract:
	$(PYTHON) scripts/test-windows-contract.py

test-packaging:
	$(PYTHON) scripts/test-packaging.py
	$(PYTHON) scripts/check-packaging.py

test-release-policy:
	$(PYTHON) scripts/test-prerequisites.py release-policy
	$(PYTHON) scripts/test-release-policy.py

# No compilation, dependency installation, network, staging or publication. Aggregate cheap
# failures so callers see all prerequisites before deciding to run an expensive tier.
RELEASE_TAG ?=
release-check:
	@status=0; \
	$(PYTHON) scripts/release-policy.py check $(if $(RELEASE_TAG),"$(RELEASE_TAG)") || status=1; \
	$(PYTHON) scripts/check-packaging.py || status=1; \
	$(PYTHON) scripts/release-policy.py tools || status=1; \
	for script in scripts/release.sh scripts/preflight.sh; do bash -n "$$script" || status=1; done; \
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
	@printf '%s\n' 'pre-push now runs scripts/preflight.sh; DIAL_PREFLIGHT=quick or skip changes that per push.'
