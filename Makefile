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
# The binary needs no configuration file and reads no environment, so
# there is nothing to install under /etc.

prefix      ?= /usr/local
exec_prefix ?= $(prefix)
bindir      ?= $(exec_prefix)/bin
datarootdir ?= $(prefix)/share
mandir      ?= $(datarootdir)/man
man1dir     ?= $(mandir)/man1
docdir      ?= $(datarootdir)/doc/systemd-mcpd
licensedir  ?= $(datarootdir)/licenses/systemd-mcpd

# Where the unit file goes. Distributions put system units in
# /usr/lib/systemd/system; ask systemd rather than guessing, and fall
# back for the case where pkg-config or systemd is absent.
SYSTEMD_UNITDIR ?= $(shell pkg-config --variable=systemdsystemunitdir systemd 2>/dev/null)
ifeq ($(SYSTEMD_UNITDIR),)
SYSTEMD_UNITDIR := $(prefix)/lib/systemd/system
endif
unitdir ?= $(SYSTEMD_UNITDIR)

CARGO      ?= cargo
CARGOFLAGS ?= --release --locked
INSTALL    ?= install

BIN     := target/release/systemd-mcpd
DOCS    := README.md docs/TOOLS.md docs/DESIGN.md docs/TESTING.md

.PHONY: all build check test install uninstall clean

all: build

build:
	$(CARGO) build $(CARGOFLAGS)

# What CI gates on, for a packager who wants to run the upstream checks
# without a systemd to test against.
check test:
	$(CARGO) test $(CARGOFLAGS)
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) fmt --check

# The unit file ships with a /usr/local ExecStart, which is right for a
# manual install and wrong for a package. Rewrite it to the bindir
# actually being installed into rather than making every packager patch
# it.
install: build
	$(INSTALL) -D -m 0755 $(BIN) $(DESTDIR)$(bindir)/systemd-mcpd
	$(INSTALL) -D -m 0644 systemd-mcpd.1 $(DESTDIR)$(man1dir)/systemd-mcpd.1
	$(INSTALL) -d -m 0755 $(DESTDIR)$(unitdir)
	sed 's,/usr/local/bin/systemd-mcpd,$(bindir)/systemd-mcpd,' \
	    systemd-mcpd.service > $(DESTDIR)$(unitdir)/systemd-mcpd.service
	chmod 0644 $(DESTDIR)$(unitdir)/systemd-mcpd.service
	$(INSTALL) -D -m 0644 -t $(DESTDIR)$(docdir) $(DOCS)
	$(INSTALL) -D -m 0644 LICENSE $(DESTDIR)$(licensedir)/LICENSE

uninstall:
	rm -f $(DESTDIR)$(bindir)/systemd-mcpd
	rm -f $(DESTDIR)$(man1dir)/systemd-mcpd.1
	rm -f $(DESTDIR)$(unitdir)/systemd-mcpd.service
	rm -rf $(DESTDIR)$(docdir) $(DESTDIR)$(licensedir)

clean:
	$(CARGO) clean
