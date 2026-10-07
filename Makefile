CARGO ?= cargo
INSTALL_DIR ?= $(shell path="$$(command -v dial 2>/dev/null)"; if [ -n "$$path" ]; then dirname "$$path"; else printf '%s' "$(HOME)/.local/bin"; fi)


.DEFAULT_GOAL := help
.PHONY: help update install build check test

help:
	@printf '%s\n' \
		'make update   Rebuild and replace the installed dial from this checkout' \
		'make install  Same as update' \
		'make build    Build the release binary without installing' \
		'make check    Check Rust compilation' \
		'make test     Run the Rust tests'

update:
	$(CARGO) build --release --locked
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
	install -m 755 target/release/dial bin/dial
	install -m 755 target/release/dial "$(INSTALL_DIR)/dial"
	@if [ -f "$(HOME)/.cargo/bin/dial" ] && [ "$(INSTALL_DIR)" != "$(HOME)/.cargo/bin" ]; then install -m 755 target/release/dial "$(HOME)/.cargo/bin/dial"; fi
	@active="$$(command -v dial 2>/dev/null)"; \
	if [ -z "$$active" ] || ! cmp -s "$$active" target/release/dial; then \
		printf 'ERROR: PATH selects %s, which differs from the installed worktree build.\n' "$$active" >&2; \
		exit 1; \
	fi
	@printf 'Updated %s and %s\n' '$(CURDIR)/bin/dial' '$(INSTALL_DIR)/dial'
	@printf '%s\n' 'If your shell cached another dial path, run: hash -r (bash) or rehash (zsh).'


install: update

build:
	$(CARGO) build --release --locked

check:
	$(CARGO) check --locked

test:
	$(CARGO) test --locked
