CARGO ?= cargo

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
	mkdir -p bin
	cp target/release/dial bin/dial
	@printf '%s\n' 'Updated the dial binary at bin/dial. Run ./bin/dial or add this directory to your PATH.' \
		'If your shell runs another copy, check: command -v dial.'

install: update

build:
	$(CARGO) build --release --locked

check:
	$(CARGO) check --locked

test:
	$(CARGO) test --locked
