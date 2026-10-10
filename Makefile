BINARY  := ostrich
PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
CARGO   ?= cargo
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
COMMIT  ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo none)
DATE    ?= $(shell git log -1 --format=%cI 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)
TARGET  ?= $(shell rustc -vV 2>/dev/null | sed -n 's/^host: //p')

export OSTRICH_VERSION := $(VERSION)
export OSTRICH_COMMIT  := $(COMMIT)
export OSTRICH_DATE    := $(DATE)

.PHONY: all build test clippy fmt check install uninstall dist clean

all: build

# Optimised binary, copied next to the Makefile like the Go build did.
build:
	$(CARGO) build --release
	cp target/release/$(BINARY) $(BINARY)

test:
	$(CARGO) test

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all -- --check

check: fmt clippy test

install: build
	install -Dm755 $(BINARY) $(DESTDIR)$(BINDIR)/$(BINARY)

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/$(BINARY)

# A release archive for this host, the way the release workflow packages
# them: ostrich_<version>_<os>_<arch>.tar.gz plus checksums.txt in ./dist.
dist: build
	./scripts/package.sh "$(VERSION)" "$(TARGET)" target/release/$(BINARY) dist

clean:
	rm -f $(BINARY)
	rm -rf dist
	$(CARGO) clean
