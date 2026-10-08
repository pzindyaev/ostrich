BINARY  := ostrich
PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
GO      ?= go
LDFLAGS ?= -s -w

.PHONY: all build test vet install uninstall clean

all: build

build:
	$(GO) build -ldflags '$(LDFLAGS)' -o $(BINARY) .

test:
	$(GO) test ./...

vet:
	$(GO) vet ./...

install: build
	install -Dm755 $(BINARY) $(DESTDIR)$(BINDIR)/$(BINARY)

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/$(BINARY)

clean:
	rm -f $(BINARY)
