# Makefile for musman.
#
#   make            build the debug binary (default)
#   make release    build the release binary
#   make install    install the release binary to $(PREFIX) (default: ~/.local)
#   make test       run the test suite
#   make clippy     lint with clippy
#   make fmt        format the code
#   make fmt-check  check formatting without modifying files
#   make clean      remove build artifacts

CARGO   ?= cargo
PREFIX  ?= $(HOME)/.local
BINDIR  ?= $(PREFIX)/bin
DESTDIR ?=

BIN := musman

.PHONY: all release install test clippy fmt fmt-check clean
.DELETE_ON_ERROR:

all:
	$(CARGO) build

release:
	$(CARGO) build --release

install: release
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 0755 "target/release/$(BIN)" "$(DESTDIR)$(BINDIR)/$(BIN)"

test:
	$(CARGO) test

clippy:
	$(CARGO) clippy --all-targets

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt --check

clean:
	$(CARGO) clean
