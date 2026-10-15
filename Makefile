CARGO ?= cargo
# Use PROFILE=release for shipping-equivalent optimization (thin LTO).
PROFILE ?= local
BUILD_DIR = target/$(PROFILE)
INSTALL_DIR ?= $(shell path="$$(command -v dial 2>/dev/null)"; if [ -n "$$path" ]; then dirname "$$path"; else printf '%s' "$(HOME)/.local/bin"; fi)
# Timestamped `dial.bak-*` copies kept per installed location by `make update` (newest first).
KEEP_BACKUPS ?= 3


.DEFAULT_GOAL := help
.PHONY: help update install build check test test-lib test-integration test-module lint

help:
	@printf '%s\n' \
		'make update   Rebuild and replace the installed dial from this checkout (keeps KEEP_BACKUPS=3 backups)' \
		'make install  Same as update' \
		'make build    Build an optimized local binary (PROFILE=release for shipping optimization)' \
		'make check    Check Rust compilation' \
		'make test     Run the Rust tests' \
		'make test-lib Run library unit tests without building the CLI/integration targets' \
		'make test-integration  Run the single integration test binary (tests/integration)' \
		'make test-module MOD=llm::provider  Run lib tests under one module path' \
		'make lint     Run clippy with the CI lint gate'

update:
	$(CARGO) build --profile $(PROFILE) --locked
	mkdir -p bin "$(INSTALL_DIR)"
	@stamp="$$(date +%Y%m%d-%H%M%S)"; \
	for path in bin/dial "$(INSTALL_DIR)/dial"; do \
		if [ -f "$$path" ]; then \
			cp -p "$$path" "$$path.bak-$$stamp"; \
			printf 'Preserved previous binary: %s.bak-%s\n' "$$path" "$$stamp"; \
		fi; \
	done
	@if [ "$(INSTALL_DIR)" != "$(HOME)/.cargo/bin" ] && [ -f "$(HOME)/.cargo/bin/dial" ]; then \
		stamp="$$(date +%Y%m%d-%H%M%S)"; \
		cp -p "$(HOME)/.cargo/bin/dial" "$(HOME)/.cargo/bin/dial.bak-$$stamp"; \
		printf 'Preserved previous binary: %s.bak-%s\n' "$(HOME)/.cargo/bin/dial" "$$stamp"; \
	fi
	@for path in bin/dial "$(INSTALL_DIR)/dial" "$(HOME)/.cargo/bin/dial"; do \
		for old in "$$path".bak-*; do [ -f "$$old" ] && printf '%s\n' "$$old"; done \
			| sort -r | awk 'NR > $(KEEP_BACKUPS)' | while IFS= read -r old; do \
				rm -f -- "$$old" && printf 'Pruned old backup: %s\n' "$$old"; \
			done; \
	done
	install -m 755 $(BUILD_DIR)/dial bin/dial
	install -m 755 $(BUILD_DIR)/dial "$(INSTALL_DIR)/dial"
	@if [ -f "$(HOME)/.cargo/bin/dial" ] && [ "$(INSTALL_DIR)" != "$(HOME)/.cargo/bin" ]; then install -m 755 $(BUILD_DIR)/dial "$(HOME)/.cargo/bin/dial"; fi
	@active="$$(command -v dial 2>/dev/null)"; \
	if [ -z "$$active" ] || ! cmp -s "$$active" $(BUILD_DIR)/dial; then \
		printf 'ERROR: PATH selects %s, which differs from the installed worktree build.\n' "$$active" >&2; \
		exit 1; \
	fi
	@printf 'Updated %s and %s\n' '$(CURDIR)/bin/dial' '$(INSTALL_DIR)/dial'
	@printf '%s\n' 'If your shell cached another dial path, run: hash -r (bash) or rehash (zsh).'


install: update

build:
	$(CARGO) build --profile $(PROFILE) --locked

check:
	$(CARGO) check --locked

test:
	$(CARGO) test --locked

test-lib:
	$(CARGO) test --locked --lib

test-integration:
	$(CARGO) test --locked --test integration

# Fast loop for one lib module: make test-module MOD=llm::provider
test-module:
	@test -n "$(MOD)" || { printf '%s\n' 'Usage: make test-module MOD=llm::provider' >&2; exit 2; }
	$(CARGO) test --locked --lib $(MOD)

lint:
	$(CARGO) clippy --locked --all-targets -- -D clippy::correctness -D clippy::suspicious
