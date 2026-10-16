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
TEST_PROFILE ?= dev
DIAL ?= $(BUILD_DIR)/dial


.DEFAULT_GOAL := help
.PHONY: help update install build check test test-lib test-integration test-module lint test-rust test-python test-bench-offline test-smoke test-mlx

help:
	@printf '%s\n' \
		'make update   Rebuild and replace the installed dial from this checkout (keeps KEEP_BACKUPS=3 backups)' \
		'make install  Same as update' \
		'make build    Build locally with timings (SLIM=1 omits optional integrations; PROFILE=release for shipping)' \
		'make check    Check Rust compilation' \
		'make test     Run the Rust tests' \
		'make test-lib Run library unit tests without building the CLI/integration targets' \
		'make test-integration  Run the single integration test binary (tests/integration)' \
		'make test-module MOD=llm::provider  Run lib tests under one module path' \
		'make lint     Run clippy with the CI lint gate' \
		'make test-rust TEST_PROFILE=dev  Run portable Rust tests (CI uses ci)' \
		'make test-python  Run Python contracts; report unavailable optional MLX checks' \
		'make test-bench-offline  Run benchmark grader/reporting tests, no model or binary needed' \
		'make test-smoke DIAL=target/local/dial  Check an already-built binary and scripted recovery' \
		'make test-mlx  Run optional local MLX checks; explicitly skip unsupported hosts, never download'

update:
	$(PYTHON) scripts/install-dev.py check --keep "$(KEEP_BACKUPS)"
	$(CARGO) build --bin dial --profile $(PROFILE) --locked --timings $(CARGO_FEATURE_FLAGS)
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
	$(CARGO) build --bin dial --profile $(PROFILE) --locked --timings $(CARGO_FEATURE_FLAGS)

check:
	$(CARGO) check --locked $(CARGO_FEATURE_FLAGS)

test: test-rust

test-rust:
	$(CARGO) test --locked --profile $(TEST_PROFILE) $(CARGO_FEATURE_FLAGS)

test-lib:
	$(CARGO) test --locked --lib $(CARGO_FEATURE_FLAGS)

test-integration:
	$(CARGO) test --locked --test integration $(CARGO_FEATURE_FLAGS)

# Fast loop for one lib module: make test-module MOD=llm::provider
test-module:
	@test -n "$(MOD)" || { printf '%s\n' 'Usage: make test-module MOD=llm::provider' >&2; exit 2; }
	$(CARGO) test --locked --lib $(CARGO_FEATURE_FLAGS) $(MOD)

lint:
	$(CARGO) clippy --locked --all-targets $(CARGO_FEATURE_FLAGS) -- -D clippy::correctness -D clippy::suspicious

test-python:
	HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 $(PYTHON) -m pytest -rs python/test_*.py

test-bench-offline:
	$(PYTHON) -m unittest discover -s examples/benchmarks -p 'test_*.py'

# Deliberately no build prerequisite: test exactly the artifact selected by DIAL.
test-smoke:
	$(PYTHON) scripts/check-artifact.py --dial "$(DIAL)"
	$(PYTHON) examples/benchmarks/smoke.py --dial "$(DIAL)" --suite recovery --machine-repetitions 2

test-mlx:
	HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 $(PYTHON) scripts/test-mlx.py
