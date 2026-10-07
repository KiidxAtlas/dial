CARGO ?= cargo
INSTALL_DIR ?= $(HOME)/.local/bin


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
	install -m 755 target/release/dial bin/dial
	install -m 755 target/release/dial "$(INSTALL_DIR)/dial"
	@printf 'Updated %s and %s\n' '$(CURDIR)/bin/dial' '$(INSTALL_DIR)/dial'
	@printf '%s\n' 'If your shell cached another dial path, run: hash -r (bash) or rehash (zsh).'


install: update

build:
	$(CARGO) build --release --locked

check:
	$(CARGO) check --locked

test:
	$(CARGO) test --locked
