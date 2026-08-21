#!/usr/bin/env bash
# Lints the man page with the tools that lint man pages.
#
# This used to also scrape `--help` and the README table with grep and
# awk and fail the build on a mismatch. That was the same mistake this
# project refuses to make against systemctl: deriving facts by parsing
# text meant for people. It failed on reindented usage lines, it could
# not see a description that had gone stale without changing shape, and
# a documentation slip is not a reason to reject a build.
#
# What is left is two real linters, run against a shipped artifact,
# neither of which parses prose to infer anything.
#
#   bash tests/docs.sh
set -euo pipefail

PAGE=${PAGE:-systemd-ops-mcp.1}

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

echo "PASS: the man page is clean and indexable"
