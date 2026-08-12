#!/usr/bin/env bash
# Checks the documentation against the thing it documents: that roff is
# happy with the man page, that apropos can index it, and that the man
# page and the markdown still describe the binary and the tools that
# exist.
#
# Documentation does not fail loudly when the code moves. It just
# becomes wrong, and the person who finds out is a user or a packager.
# Everything here compares a document against the source of truth
# rather than reading it for sense.
#
# Needs groff and lexgrog (man-db), and the built binary:
#
#   MCPD=target/release/systemd-mcpd bash tests/docs.sh
set -euo pipefail

PAGE=${PAGE:-systemd-mcpd.1}
MCPD=${MCPD:-target/release/systemd-mcpd}

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== roff"
# -ww is every warning, -z discards the output: this is the check
# Debian's lintian runs as manpage-has-errors-from-man.
warnings=$(LC_ALL=C groff -mandoc -ww -z "$PAGE" 2>&1) || fail "groff rejected $PAGE"
[ -z "$warnings" ] || { printf 'FAIL: groff warnings:\n%s\n' "$warnings" >&2; exit 1; }

echo "== whatis entry"
# A NAME line lexgrog cannot parse means no apropos or whatis entry,
# which Debian treats as a bug in the package.
lexgrog "$PAGE" >/dev/null || fail "lexgrog cannot parse the NAME section"

echo "== agrees with the binary"
[ -x "$MCPD" ] || fail "no binary at $MCPD (set MCPD, or cargo build --release)"
usage=$("$MCPD" --help)

# Every scope the binary accepts must be documented, and every scope
# the page documents must still exist. Both directions matter: the
# first catches a new scope, the second a removed one.
usage_scopes=$(grep -oE '^  [a-z]+:[a-z]+' <<<"$usage" | tr -d ' ' | sort -u)
page_scopes=$(grep -oE '^\.B [a-z]+:[a-z]+' "$PAGE" | cut -d' ' -f2 | sort -u)
[ -n "$usage_scopes" ] || fail "found no scopes in --help; has the usage text changed shape?"
diff <(echo "$usage_scopes") <(echo "$page_scopes") ||
  fail "scopes differ between --help (left) and $PAGE (right)"

# Same for long options. The page spells them with roff escapes.
usage_flags=$(grep -oE '\-\-[a-z]+' <<<"$usage" | sort -u)
page_flags=$(sed 's/\\-/-/g' "$PAGE" | grep -oE '\-\-[a-z]+' | sort -u)
for flag in $usage_flags; do
  grep -qx -- "$flag" <<<"$page_flags" || fail "$flag is in --help but not in $PAGE"
done
for flag in $page_flags; do
  grep -qx -- "$flag" <<<"$usage_flags" || fail "$flag is in $PAGE but not in --help"
done

# `make install` substitutes the version into the .TH source field, so
# the checked-in page must carry the bare name for that to match. A
# silent miss would ship a man page with no version in its footer.
grep -q '^\.TH SYSTEMD\\-MCPD 1 .*"systemd-mcpd" "User Commands"' "$PAGE" ||
  fail "the .TH line no longer has the form make install substitutes into"

echo "== the tool tables match the registry"
# The registry in src/mcp.rs is the source of truth: it is what the
# server advertises. A tool renamed there and not here leaves the
# README describing something no client can call.
scope_name() { # Rust variant -> wire name
  case $1 in
    UnitsRead) echo units:read ;;
    JournalRead) echo journal:read ;;
    BootRead) echo boot:read ;;
    UnitsWrite) echo units:write ;;
    *) fail "unknown scope variant $1, teach this script about it" ;;
  esac
}
registry=$(mktemp)
# Each entry is `name: "x"` followed by `scope: Scope::Y`.
paste -d' ' \
  <(grep -oP '^\s+name: "\K[a-z_]+' src/mcp.rs) \
  <(grep -oP '^\s+scope: Scope::\K\w+' src/mcp.rs) |
  while read -r name variant; do echo "$name $(scope_name "$variant")"; done |
  sort > "$registry"
[ -s "$registry" ] || fail "found no tools in src/mcp.rs; has the registry changed shape?"

# The README table carries both facts, so compare both.
readme=$(mktemp)
grep -oP '^\| `\K[a-z_]+` +\| `[a-z:]+`' README.md |
  tr -d '`|' | awk '{print $1, $2}' | sort > "$readme"
diff "$registry" "$readme" ||
  fail "the README tool table disagrees with src/mcp.rs (left: code, right: README)"

# TOOLS.md documents arguments per tool; every tool needs an entry.
while read -r name _; do
  grep -q "\`$name\`" docs/TOOLS.md || fail "$name is not documented in docs/TOOLS.md"
done < "$registry"
rm -f "$registry" "$readme"

echo "PASS: man page is clean and indexable, and the docs match the code"
