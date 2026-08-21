# Install targets for downstream packaging.
#
# Everything is overridable and nothing is assumed about the build
# environment: DESTDIR for staged installs, prefix and the *dir
# variables per the GNU standards, CARGO and CARGOFLAGS for offline or
# vendored builds.
#
#     make
#     make DESTDIR=/tmp/stage prefix=/usr install
#
# The binaries need no configuration file. Local operators may keep
# one under $XDG_CONFIG_HOME/systemd-ops/; packages should not.

prefix      ?= /usr/local
exec_prefix ?= $(prefix)
bindir      ?= $(exec_prefix)/bin
datarootdir ?= $(prefix)/share
mandir      ?= $(datarootdir)/man
man1dir     ?= $(mandir)/man1
docdir      ?= $(datarootdir)/doc/systemd-ops
licensedir  ?= $(datarootdir)/licenses/systemd-ops

SYSTEMD_UNITDIR ?= $(shell pkg-config --variable=systemdsystemunitdir systemd 2>/dev/null)
ifeq ($(SYSTEMD_UNITDIR),)
SYSTEMD_UNITDIR := $(prefix)/lib/systemd/system
endif
unitdir ?= $(SYSTEMD_UNITDIR)

CARGO      ?= cargo
CARGOFLAGS ?= --release --locked
INSTALL    ?= install

BIN     := target/release/systemd-ops
MCPBIN  := target/release/systemd-ops-mcp
DOCS    := README.md docs/TOOLS.md docs/DESIGN.md docs/TESTING.md
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

.PHONY: all build check test install uninstall clean

all: build

build:
	$(CARGO) build $(CARGOFLAGS)

check test: build
	$(CARGO) test $(CARGOFLAGS)
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) fmt --check
	@if command -v groff >/dev/null && command -v lexgrog >/dev/null; then \
	    bash tests/docs.sh; \
	else \
	    echo "skipping the documentation check: groff or lexgrog is missing"; \
	fi

install: build
	$(INSTALL) -D -m 0755 $(BIN) $(DESTDIR)$(bindir)/systemd-ops
	$(INSTALL) -D -m 0755 $(MCPBIN) $(DESTDIR)$(bindir)/systemd-ops-mcp
	$(INSTALL) -d -m 0755 $(DESTDIR)$(man1dir)
	sed 's/"systemd-ops-mcp" "User Commands"/"systemd-ops-mcp $(VERSION)" "User Commands"/' \
	    systemd-ops-mcp.1 > $(DESTDIR)$(man1dir)/systemd-ops-mcp.1
	chmod 0644 $(DESTDIR)$(man1dir)/systemd-ops-mcp.1
	$(INSTALL) -d -m 0755 $(DESTDIR)$(unitdir)
	sed 's,/usr/local/bin/systemd-ops-mcp,$(bindir)/systemd-ops-mcp,' \
	    systemd-ops-mcp@.service > $(DESTDIR)$(unitdir)/systemd-ops-mcp@.service
	chmod 0644 $(DESTDIR)$(unitdir)/systemd-ops-mcp@.service
	$(INSTALL) -D -m 0644 systemd-ops-mcp.socket $(DESTDIR)$(unitdir)/systemd-ops-mcp.socket
	$(INSTALL) -D -m 0644 -t $(DESTDIR)$(docdir) $(DOCS)
	$(INSTALL) -D -m 0644 LICENSE $(DESTDIR)$(licensedir)/LICENSE

uninstall:
	rm -f $(DESTDIR)$(bindir)/systemd-ops
	rm -f $(DESTDIR)$(bindir)/systemd-ops-mcp
	rm -f $(DESTDIR)$(man1dir)/systemd-ops-mcp.1
	rm -f $(DESTDIR)$(unitdir)/systemd-ops-mcp@.service
	rm -f $(DESTDIR)$(unitdir)/systemd-ops-mcp.socket
	rm -rf $(DESTDIR)$(docdir) $(DESTDIR)$(licensedir)

clean:
	$(CARGO) clean
